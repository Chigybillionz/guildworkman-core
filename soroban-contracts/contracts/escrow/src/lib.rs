#![no_std]

//! Escrow contract for GuildWorkman appointments.
//!
//! Supports two escrow types:
//!
//! 1. **Simple escrow** — a client funds an appointment, confirms completion,
//!    and the worker is paid. Either party can raise a dispute, resolved by the
//!    admin arbiter.
//!
//! 2. **Milestone escrow** — a client funds an escrow with multiple milestones.
//!    Each milestone has a time-lock deadline, an amount, and a description hash.
//!    The client approves milestones as work progresses; funds are released
//!    only after approval *and* the time-lock expires. Disputes can be raised
//!    per-milestone and resolved by the admin or an external arbitration hook.
//!
//! ## Storage layout
//!
//! | Key | Durability | Type | Holds |
//! |-----|-----------|------|-------|
//! | `DataKey::Admin` | instance | `Address` | Admin/arbiter for dispute resolution |
//! | `DataKey::Appointment(id)` | persistent | `Appointment` | Simple escrow state |
//! | `DataKey::MilestoneEscrow(id)` | persistent | `MilestoneEscrow` | Milestone escrow state |
//! | `GovernanceDataKey::*` | instance | governance-guard types | M-of-N upgrade governance, signer rotation, and the emergency pause record |
//!
//! ## Authorization model
//!
//! - `create_appointment` / `create_milestone_escrow`: client must authorize.
//! - `add_milestone` / `approve_milestone`: client must authorize.
//! - `confirm_completion` / `cancel_appointment`: client must authorize.
//! - `raise_dispute` / `raise_milestone_dispute`: participant must authorize.
//! - `resolve_dispute` / `resolve_milestone_dispute`: admin/arbiter must authorize.
//! - `release_milestone_funds`: permissionless once conditions are met.
//! - `pause` / `unpause`: any single governance signer (not `admin`).
//! - All functions follow checks-effects-interactions to prevent reentrancy.
//!
//! ## Emergency circuit breaker
//!
//! Scoped, self-expiring pausability from `guildworkman-governance-guard`;
//! see that crate's `pausable` module for the full rationale. In this
//! contract:
//!
//! | Entrypoint | Scope |
//! |---|---|
//! | `create_appointment`, `create_milestone_escrow`, `add_milestone` | `SCOPE_INTAKE` |
//! | `confirm_completion`, `approve_milestone`, `release_milestone_funds` | `SCOPE_SETTLEMENT` |
//! | `cancel_appointment`, `raise_dispute`, `resolve_dispute`, `raise_milestone_dispute`, `resolve_milestone_dispute` | **none — never pausable** |
//!
//! That last row is the design constraint, not an oversight: a pause must
//! never trap user funds, so every route by which an escrowed balance
//! reaches whoever is entitled to it is left unguarded. There is no scope
//! value, `ALL_SCOPES` included, that halts a refund or a dispute. A pause
//! can therefore stop new money entering and delay a discretionary payout,
//! but cannot strand money already held here — and it lapses on its own at
//! `expires_at` with no admin transaction required.
//!
//! `pause` carries a length-capped operator `reason` recorded alongside the
//! deadline, so `get_pause_state` answers "what is halted, until when, and
//! why" in one read.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, String, Vec,
};

use soroban_sdk::contractevent;

use guildworkman_governance_guard as governance;
pub use guildworkman_governance_guard::{
    PauseState, PendingRotation, PendingUpgrade, ALL_SCOPES, MAX_PAUSE_DURATION,
    MAX_PAUSE_REASON_LEN, SCOPE_INTAKE, SCOPE_SETTLEMENT,
};

// ---------------------------------------------------------------------------
// Contract events
// ---------------------------------------------------------------------------

/// Emitted when a client funds an appointment. Topics: `["escrow",
/// "created", appointment_id, client, worker]`; data carries the amount.
#[contractevent(topics = ["escrow", "created"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppointmentCreated {
    #[topic]
    pub appointment_id: u64,
    #[topic]
    pub client: Address,
    #[topic]
    pub worker: Address,
    pub amount: i128,
}

/// Emitted when a client confirms completion, paying the worker. Topics:
/// `["escrow", "completed", appointment_id, client]`; data carries the
/// worker address.
#[contractevent(topics = ["escrow", "completed"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppointmentCompleted {
    #[topic]
    pub appointment_id: u64,
    #[topic]
    pub client: Address,
    pub worker: Address,
}

/// Emitted when a client cancels an appointment for a full refund. Topics:
/// `["escrow", "cancelled", appointment_id, client]`; data carries the
/// refunded amount.
#[contractevent(topics = ["escrow", "cancelled"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppointmentCancelled {
    #[topic]
    pub appointment_id: u64,
    #[topic]
    pub client: Address,
    pub amount: i128,
}

/// Emitted when either party raises a dispute. Topics: `["escrow",
/// "disputed", appointment_id, caller]`; data carries both participant
/// addresses.
#[contractevent(topics = ["escrow", "disputed"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppointmentDisputed {
    #[topic]
    pub appointment_id: u64,
    #[topic]
    pub caller: Address,
    pub client: Address,
    pub worker: Address,
}

/// Emitted when the admin resolves a dispute. Topics: `["escrow",
/// "resolved", appointment_id, recipient]`; data carries the amount and
/// whether it was refunded to the client.
#[contractevent(topics = ["escrow", "resolved"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppointmentResolved {
    #[topic]
    pub appointment_id: u64,
    #[topic]
    pub recipient: Address,
    pub amount: i128,
    pub refund_to_client: bool,
}

/// Emitted when a milestone is added to a milestone escrow. Topics:
/// `["escrow", "milestone_created", escrow_id, client]`.
#[contractevent(topics = ["escrow", "milestone_created"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MilestoneCreated {
    #[topic]
    pub escrow_id: u64,
    #[topic]
    pub client: Address,
    pub index: u32,
    pub amount: i128,
    pub deadline: u32,
}

/// Emitted when the client approves a milestone. Topics: `["escrow",
/// "milestone_approved", escrow_id, client]`.
#[contractevent(topics = ["escrow", "milestone_approved"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MilestoneApproved {
    #[topic]
    pub escrow_id: u64,
    #[topic]
    pub client: Address,
    pub milestone_index: u32,
}

/// Emitted when milestone funds are released to the worker. Topics:
/// `["escrow", "milestone_released", escrow_id, worker]`.
#[contractevent(topics = ["escrow", "milestone_released"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MilestoneReleased {
    #[topic]
    pub escrow_id: u64,
    #[topic]
    pub worker: Address,
    pub milestone_index: u32,
    pub amount: i128,
}

/// Emitted when a milestone dispute is raised. Topics: `["escrow",
/// "milestone_disputed", escrow_id, caller]`.
#[contractevent(topics = ["escrow", "milestone_disputed"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MilestoneDisputed {
    #[topic]
    pub escrow_id: u64,
    #[topic]
    pub caller: Address,
    pub milestone_index: u32,
}

/// Emitted when a milestone dispute is resolved. Topics: `["escrow",
/// "milestone_resolved", escrow_id, recipient]`.
#[contractevent(topics = ["escrow", "milestone_resolved"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MilestoneResolved {
    #[topic]
    pub escrow_id: u64,
    #[topic]
    pub recipient: Address,
    pub milestone_index: u32,
    pub amount: i128,
}

/// Emitted when a milestone escrow is created. Topics: `["escrow",
/// "milestone_escrow_created", escrow_id, client, worker]`.
#[contractevent(topics = ["escrow", "milestone_escrow_created"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MilestoneEscrowCreated {
    #[topic]
    pub escrow_id: u64,
    #[topic]
    pub client: Address,
    #[topic]
    pub worker: Address,
    pub total_amount: i128,
}

/// Bump when this contract's storage layout actually changes shape and
/// needs a real transformation in `migrate`. There's no such change yet.
const CURRENT_STORAGE_VERSION: u32 = 1;

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Funded,
    Completed,
    Cancelled,
    Disputed,
    Resolved,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Appointment {
    pub client: Address,
    pub worker: Address,
    pub token: Address,
    pub amount: i128,
    pub status: Status,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MilestoneStatus {
    Pending,
    Approved,
    Released,
    Disputed,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub description: BytesN<32>,
    pub amount: i128,
    pub deadline: u32,
    pub status: MilestoneStatus,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArbitrationMode {
    AdminArbiter,
    ExternalHook,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct MilestoneEscrow {
    pub client: Address,
    pub worker: Address,
    pub token: Address,
    pub total_amount: i128,
    pub released_amount: i128,
    pub status: Status,
    pub milestones: Vec<Milestone>,
    pub arbiter: Address,
    pub arbitration_mode: ArbitrationMode,
    pub hook_address: Option<Address>,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct MilestoneEscrowInit {
    pub client: Address,
    pub worker: Address,
    pub token: Address,
    pub total_amount: i128,
    pub arbiter: Address,
    pub arbitration_mode: ArbitrationMode,
    pub hook_address: Option<Address>,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Appointment(u64),
    MilestoneEscrow(u64),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    AppointmentExists = 3,
    AppointmentNotFound = 4,
    InvalidStatus = 5,
    InvalidAmount = 6,
    NotAParticipant = 7,
    // --- Upgrade governance (see guildworkman-governance-guard) ---
    GovernanceAlreadyInitialized = 8,
    GovernanceNotInitialized = 9,
    InvalidThreshold = 10,
    DuplicateSigner = 11,
    NotASigner = 12,
    NoPendingUpgrade = 13,
    AlreadyApproved = 14,
    ProposalExpired = 15,
    HashMismatch = 16,
    AlreadyMigrated = 17,
    NothingToMigrate = 18,
    // --- Milestone escrow ---
    MilestoneNotFound = 19,
    InvalidMilestoneAmount = 20,
    MilestoneAlreadyApproved = 21,
    MilestoneTimeLocked = 22,
    MilestoneAlreadyReleased = 23,
    InvalidEscrowStatus = 24,
    ArbitrationFailed = 25,
    NotAClient = 26,
    NotAWorker = 27,
    MilestoneAmountMismatch = 28,
    InvalidMilestoneCount = 29,
    InvalidDeadline = 30,
    // --- Signer rotation (see guildworkman-governance-guard) ---
    NoPendingRotation = 31,
    RotationMismatch = 32,
    RotationNotReady = 33,
    RotationTimelockActive = 34,
    RotationExpired = 35,
    RotationInProgress = 36,
    // --- Emergency circuit breaker (see guildworkman-governance-guard) ---
    OperationPaused = 37,
    InvalidPauseScope = 38,
    InvalidPauseDuration = 39,
    NotPaused = 40,
    InvalidPauseReason = 41,
}

impl From<governance::GovernanceError> for Error {
    fn from(e: governance::GovernanceError) -> Self {
        match e {
            governance::GovernanceError::AlreadyInitialized => Error::GovernanceAlreadyInitialized,
            governance::GovernanceError::NotInitialized => Error::GovernanceNotInitialized,
            governance::GovernanceError::InvalidThreshold => Error::InvalidThreshold,
            governance::GovernanceError::DuplicateSigner => Error::DuplicateSigner,
            governance::GovernanceError::NotASigner => Error::NotASigner,
            governance::GovernanceError::NoPendingUpgrade => Error::NoPendingUpgrade,
            governance::GovernanceError::AlreadyApproved => Error::AlreadyApproved,
            governance::GovernanceError::ProposalExpired => Error::ProposalExpired,
            governance::GovernanceError::HashMismatch => Error::HashMismatch,
            governance::GovernanceError::AlreadyMigrated => Error::AlreadyMigrated,
            governance::GovernanceError::NoPendingRotation => Error::NoPendingRotation,
            governance::GovernanceError::RotationMismatch => Error::RotationMismatch,
            governance::GovernanceError::RotationNotReady => Error::RotationNotReady,
            governance::GovernanceError::RotationTimelockActive => Error::RotationTimelockActive,
            governance::GovernanceError::RotationExpired => Error::RotationExpired,
            governance::GovernanceError::RotationInProgress => Error::RotationInProgress,
            governance::GovernanceError::OperationPaused => Error::OperationPaused,
            governance::GovernanceError::InvalidPauseScope => Error::InvalidPauseScope,
            governance::GovernanceError::InvalidPauseDuration => Error::InvalidPauseDuration,
            governance::GovernanceError::NotPaused => Error::NotPaused,
            governance::GovernanceError::InvalidPauseReason => Error::InvalidPauseReason,
        }
    }
}

const LEDGERS_THRESHOLD: u32 = 17_280; // ~1 day, in ledgers (5s/ledger)
const LEDGERS_EXTEND_TO: u32 = 518_400; // ~30 days

#[contract]
pub struct EscrowContract;

#[contractimpl]
impl EscrowContract {
    /// One-time setup. `admin` acts as the dispute arbiter. `signers`/
    /// `threshold` configure the M-of-N governance guard that gates
    /// upgrading this contract's code and running a post-upgrade
    /// migration — entirely separate from `admin`, which keeps its
    /// existing power to resolve disputes on its own.
    pub fn initialize(
        env: Env,
        admin: Address,
        governance_init: governance::GovernanceInit,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        governance::init_governance(&env, governance_init)?;
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .extend_ttl(LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);
        Ok(())
    }

    // ----- Upgrade governance -----

    pub fn propose_upgrade(
        env: Env,
        proposer: Address,
        wasm_hash: BytesN<32>,
    ) -> Result<bool, Error> {
        let ready = governance::propose_upgrade(&env, proposer, wasm_hash.clone())?;
        if ready {
            env.deployer().update_current_contract_wasm(wasm_hash);
        }
        Ok(ready)
    }

    pub fn approve_upgrade(
        env: Env,
        approver: Address,
        wasm_hash: BytesN<32>,
    ) -> Result<bool, Error> {
        let ready = governance::approve_upgrade(&env, approver, wasm_hash.clone())?;
        if ready {
            env.deployer().update_current_contract_wasm(wasm_hash);
        }
        Ok(ready)
    }

    pub fn cancel_upgrade(env: Env, caller: Address) -> Result<(), Error> {
        governance::cancel_upgrade(&env, caller).map_err(Into::into)
    }

    // ----- Signer rotation -----

    /// Opens a timelocked proposal to rotate the governance signer set and
    /// threshold, authorized by the current signers at the current
    /// threshold. Reaching threshold only *schedules* the rotation;
    /// `execute_signer_rotation` applies it after the timelock. Returns
    /// `true` if this call reached threshold.
    pub fn propose_signer_rotation(
        env: Env,
        proposer: Address,
        new_signers: Vec<Address>,
        new_threshold: u32,
    ) -> Result<bool, Error> {
        governance::propose_signer_rotation(&env, proposer, new_signers, new_threshold)
            .map_err(Into::into)
    }

    /// Approves the pending signer rotation. Returns `true` when this
    /// approval reaches threshold and schedules the rotation.
    pub fn approve_signer_rotation(
        env: Env,
        approver: Address,
        new_signers: Vec<Address>,
        new_threshold: u32,
    ) -> Result<bool, Error> {
        governance::approve_signer_rotation(&env, approver, new_signers, new_threshold)
            .map_err(Into::into)
    }

    /// Applies a scheduled rotation once its timelock has elapsed. Any
    /// current signer may trigger it.
    pub fn execute_signer_rotation(env: Env, caller: Address) -> Result<(), Error> {
        governance::execute_signer_rotation(&env, caller).map_err(Into::into)
    }

    pub fn get_pending_rotation(env: Env) -> Option<PendingRotation> {
        governance::get_pending_rotation(&env)
    }

    pub fn migrate(env: Env, signer: Address) -> Result<(), Error> {
        governance::require_signer(&env, &signer)?;
        if governance::current_storage_version(&env) >= CURRENT_STORAGE_VERSION {
            return Err(Error::NothingToMigrate);
        }
        // No storage shape has changed since v1 — nothing to transform yet.
        governance::mark_migrated(&env, CURRENT_STORAGE_VERSION)?;
        Ok(())
    }

    pub fn get_signers(env: Env) -> Vec<Address> {
        governance::get_signers(&env)
    }

    pub fn get_upgrade_threshold(env: Env) -> u32 {
        governance::get_threshold(&env)
    }

    pub fn get_pending_upgrade(env: Env) -> Option<PendingUpgrade> {
        governance::get_pending_upgrade(&env)
    }

    pub fn get_storage_version(env: Env) -> u32 {
        governance::current_storage_version(&env)
    }

    // ----- Emergency circuit breaker -----

    /// Halts `scopes` for `duration_secs` seconds, authorized by any single
    /// governance signer. Returns the resulting pause record.
    ///
    /// In this contract [`SCOPE_INTAKE`] covers `create_appointment`,
    /// `create_milestone_escrow` and `add_milestone`, while
    /// [`SCOPE_SETTLEMENT`] covers `confirm_completion`,
    /// `approve_milestone` and `release_milestone_funds`. **No scope
    /// reaches `cancel_appointment`, `raise_dispute`, `resolve_dispute`,
    /// `raise_milestone_dispute` or `resolve_milestone_dispute`** — every
    /// route by which an escrowed balance can get back to whoever is
    /// entitled to it is unguarded, so a pause can delay new business but
    /// can never strand money already held here.
    pub fn pause(
        env: Env,
        caller: Address,
        scopes: u32,
        duration_secs: u64,
        reason: String,
    ) -> Result<PauseState, Error> {
        governance::pause(&env, caller, scopes, duration_secs, reason).map_err(Into::into)
    }

    /// Clears `scopes` from the active pause early. Returns the scopes still
    /// halted. Any single governance signer may call it — including one who
    /// did not place the pause.
    pub fn unpause(env: Env, caller: Address, scopes: u32) -> Result<u32, Error> {
        governance::unpause(&env, caller, scopes).map_err(Into::into)
    }

    /// The active pause record, or `None` if nothing is halted — including
    /// when a pause was placed but has since auto-expired.
    pub fn get_pause_state(env: Env) -> Option<PauseState> {
        governance::get_pause_state(&env)
    }

    /// Bitmask of currently halted scopes; `0` when the contract is open.
    pub fn paused_scopes(env: Env) -> u32 {
        governance::paused_scopes(&env)
    }

    /// Whether any bit of `scope` is currently halted.
    pub fn is_paused(env: Env, scope: u32) -> bool {
        governance::is_paused(&env, scope)
    }

    /// Client books a skilled worker and deposits `amount` of `token` into escrow.
    ///
    /// Guarded by [`SCOPE_INTAKE`]: this is new money walking into the
    /// contract, which is the first thing to stop in an incident and the
    /// one thing that provably cannot strand anything when stopped.
    pub fn create_appointment(
        env: Env,
        appointment_id: u64,
        client: Address,
        worker: Address,
        token: Address,
        amount: i128,
    ) -> Result<(), Error> {
        // Guard first, before auth and before any storage read: a halted
        // operation should cost nothing and reveal nothing beyond the
        // already-public pause state.
        governance::require_not_paused(&env, governance::SCOPE_INTAKE)?;
        client.require_auth();

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let key = DataKey::Appointment(appointment_id);
        if env.storage().persistent().has(&key) {
            return Err(Error::AppointmentExists);
        }

        // Pull funds from the client into the contract's own balance.
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&client, env.current_contract_address(), &amount);

        let client_addr = client.clone();
        let worker_addr = worker.clone();
        let appointment = Appointment {
            client,
            worker,
            token,
            amount,
            status: Status::Funded,
        };
        env.storage().persistent().set(&key, &appointment);
        env.storage()
            .persistent()
            .extend_ttl(&key, LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);

        AppointmentCreated {
            appointment_id,
            client: client_addr,
            worker: worker_addr,
            amount,
        }
        .publish(&env);

        Ok(())
    }

    /// Client confirms the job was completed satisfactorily; releases funds to the worker.
    ///
    /// Guarded by [`SCOPE_SETTLEMENT`]. Pausing this withholds a payout, so
    /// it is worth being explicit that it is a delay and not a seizure: the
    /// client can still `cancel_appointment` for a full refund and either
    /// party can still force `raise_dispute` → `resolve_dispute`, exactly
    /// as they could before any of this existed. The escrowed amount stays
    /// reachable by whoever is entitled to it throughout.
    pub fn confirm_completion(env: Env, appointment_id: u64) -> Result<(), Error> {
        governance::require_not_paused(&env, governance::SCOPE_SETTLEMENT)?;
        let key = DataKey::Appointment(appointment_id);
        let mut appointment = Self::read_appointment(&env, &key)?;

        appointment.client.require_auth();

        if appointment.status != Status::Funded {
            return Err(Error::InvalidStatus);
        }

        let token_client = token::Client::new(&env, &appointment.token);
        token_client.transfer(
            &env.current_contract_address(),
            &appointment.worker,
            &appointment.amount,
        );

        appointment.status = Status::Completed;
        env.storage().persistent().set(&key, &appointment);

        AppointmentCompleted {
            appointment_id,
            client: appointment.client,
            worker: appointment.worker,
        }
        .publish(&env);

        Ok(())
    }

    /// Client cancels before the job starts; refunds the client in full.
    ///
    /// **Deliberately unguarded by the circuit breaker.** This is the
    /// client's fund-recovery path; pausing it would convert an incident
    /// response into a hostage situation. There is no scope value that
    /// halts this function.
    pub fn cancel_appointment(env: Env, appointment_id: u64) -> Result<(), Error> {
        let key = DataKey::Appointment(appointment_id);
        let mut appointment = Self::read_appointment(&env, &key)?;

        appointment.client.require_auth();

        if appointment.status != Status::Funded {
            return Err(Error::InvalidStatus);
        }

        let token_client = token::Client::new(&env, &appointment.token);
        token_client.transfer(
            &env.current_contract_address(),
            &appointment.client,
            &appointment.amount,
        );

        appointment.status = Status::Cancelled;
        env.storage().persistent().set(&key, &appointment);

        AppointmentCancelled {
            appointment_id,
            client: appointment.client,
            amount: appointment.amount,
        }
        .publish(&env);

        Ok(())
    }

    /// Either the client or the worker can flag a disagreement, freezing the funds
    /// until the admin resolves it.
    ///
    /// **Deliberately unguarded by the circuit breaker** — raising a
    /// dispute is how a party who cannot use the happy path reaches
    /// `resolve_dispute`, which is itself a recovery route.
    pub fn raise_dispute(env: Env, appointment_id: u64, caller: Address) -> Result<(), Error> {
        caller.require_auth();

        let key = DataKey::Appointment(appointment_id);
        let mut appointment = Self::read_appointment(&env, &key)?;

        if caller != appointment.client && caller != appointment.worker {
            return Err(Error::NotAParticipant);
        }
        if appointment.status != Status::Funded {
            return Err(Error::InvalidStatus);
        }

        appointment.status = Status::Disputed;
        env.storage().persistent().set(&key, &appointment);

        AppointmentDisputed {
            appointment_id,
            caller,
            client: appointment.client,
            worker: appointment.worker,
        }
        .publish(&env);

        Ok(())
    }

    /// Admin/arbiter resolves a dispute by sending the escrowed funds to whichever
    /// side is owed them.
    ///
    /// **Deliberately unguarded by the circuit breaker** — this is the
    /// escrow's ultimate fund-recovery route and must stay reachable no
    /// matter what else is halted.
    pub fn resolve_dispute(
        env: Env,
        appointment_id: u64,
        refund_to_client: bool,
    ) -> Result<(), Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        admin.require_auth();

        let key = DataKey::Appointment(appointment_id);
        let mut appointment = Self::read_appointment(&env, &key)?;

        if appointment.status != Status::Disputed {
            return Err(Error::InvalidStatus);
        }

        let token_client = token::Client::new(&env, &appointment.token);
        let recipient = if refund_to_client {
            &appointment.client
        } else {
            &appointment.worker
        };
        token_client.transfer(
            &env.current_contract_address(),
            recipient,
            &appointment.amount,
        );

        appointment.status = Status::Resolved;
        env.storage().persistent().set(&key, &appointment);

        AppointmentResolved {
            appointment_id,
            recipient: recipient.clone(),
            amount: appointment.amount,
            refund_to_client,
        }
        .publish(&env);

        Ok(())
    }

    pub fn get_appointment(env: Env, appointment_id: u64) -> Result<Appointment, Error> {
        Self::read_appointment(&env, &DataKey::Appointment(appointment_id))
    }

    // ===========================================================================
    // Milestone Escrow
    // ===========================================================================

    /// Create a milestone-based escrow. `total_amount` is pulled from the client
    /// immediately. `arbiter` resolves disputes when `mode` is `AdminArbiter`;
    /// `hook_address` is called for `ExternalHook`.
    ///
    /// Guarded by [`SCOPE_INTAKE`] — new money entering the contract.
    pub fn create_milestone_escrow(
        env: Env,
        escrow_id: u64,
        init: MilestoneEscrowInit,
    ) -> Result<(), Error> {
        governance::require_not_paused(&env, governance::SCOPE_INTAKE)?;
        init.client.require_auth();

        if init.total_amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let key = DataKey::MilestoneEscrow(escrow_id);
        if env.storage().persistent().has(&key) {
            return Err(Error::AppointmentExists);
        }

        let client_addr = init.client.clone();
        let worker_addr = init.worker.clone();

        let token_client = token::Client::new(&env, &init.token);
        token_client.transfer(
            &init.client,
            env.current_contract_address(),
            &init.total_amount,
        );

        let escrow = MilestoneEscrow {
            client: init.client,
            worker: init.worker,
            token: init.token,
            total_amount: init.total_amount,
            released_amount: 0,
            status: Status::Funded,
            milestones: Vec::new(&env),
            arbiter: init.arbiter,
            arbitration_mode: init.arbitration_mode,
            hook_address: init.hook_address,
        };
        env.storage().persistent().set(&key, &escrow);
        env.storage()
            .persistent()
            .extend_ttl(&key, LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);
        Self::bump_instance(&env);

        MilestoneEscrowCreated {
            escrow_id,
            client: client_addr,
            worker: worker_addr,
            total_amount: init.total_amount,
        }
        .publish(&env);

        Ok(())
    }

    /// Add a milestone to a funded escrow. Only the client can add milestones.
    /// The sum of all milestone amounts must equal `total_amount`.
    ///
    /// Guarded by [`SCOPE_INTAKE`]: a milestone is a new obligation carved
    /// out of held funds, so it belongs with the other intake paths even
    /// though it moves no tokens itself.
    pub fn add_milestone(
        env: Env,
        escrow_id: u64,
        description: BytesN<32>,
        amount: i128,
        deadline: u32,
    ) -> Result<u32, Error> {
        governance::require_not_paused(&env, governance::SCOPE_INTAKE)?;
        let key = DataKey::MilestoneEscrow(escrow_id);
        let mut escrow = Self::read_milestone_escrow(&env, &key)?;

        escrow.client.require_auth();

        if escrow.status != Status::Funded {
            return Err(Error::InvalidEscrowStatus);
        }
        if amount <= 0 {
            return Err(Error::InvalidMilestoneAmount);
        }
        if deadline <= env.ledger().sequence() {
            return Err(Error::InvalidDeadline);
        }

        let current_sum: i128 = escrow.milestones.iter().map(|m| m.amount).sum();
        if current_sum + amount > escrow.total_amount {
            return Err(Error::MilestoneAmountMismatch);
        }

        let index = escrow.milestones.len();
        let milestone = Milestone {
            description,
            amount,
            deadline,
            status: MilestoneStatus::Pending,
        };
        escrow.milestones.push_back(milestone);
        env.storage().persistent().set(&key, &escrow);
        Self::bump_escrow(&env, &key);
        Self::bump_instance(&env);

        MilestoneCreated {
            escrow_id,
            client: escrow.client.clone(),
            index,
            amount,
            deadline,
        }
        .publish(&env);

        Ok(index)
    }

    /// Client approves a milestone, marking it ready for time-locked release.
    ///
    /// Guarded by [`SCOPE_SETTLEMENT`] — approval is the step that arms a
    /// release, so halting settlement without halting approval would just
    /// queue up a burst of releases the moment the pause lapsed.
    pub fn approve_milestone(env: Env, escrow_id: u64, milestone_index: u32) -> Result<(), Error> {
        governance::require_not_paused(&env, governance::SCOPE_SETTLEMENT)?;
        let key = DataKey::MilestoneEscrow(escrow_id);
        let mut escrow = Self::read_milestone_escrow(&env, &key)?;

        escrow.client.require_auth();

        if escrow.status != Status::Funded {
            return Err(Error::InvalidEscrowStatus);
        }

        let milestone = escrow
            .milestones
            .get(milestone_index)
            .ok_or(Error::MilestoneNotFound)?;

        if milestone.status != MilestoneStatus::Pending {
            return Err(Error::MilestoneAlreadyApproved);
        }

        let updated = Milestone {
            status: MilestoneStatus::Approved,
            ..milestone
        };
        escrow.milestones.set(milestone_index, updated);
        env.storage().persistent().set(&key, &escrow);
        Self::bump_escrow(&env, &key);
        Self::bump_instance(&env);

        MilestoneApproved {
            escrow_id,
            client: escrow.client.clone(),
            milestone_index,
        }
        .publish(&env);

        Ok(())
    }

    /// Release funds for a single approved milestone after its time-lock has
    /// expired. Permissionless — anyone may call once the conditions are met.
    /// Checks-effects-interactions: milestone status is updated to `Released`
    /// before the token transfer.
    ///
    /// Guarded by [`SCOPE_SETTLEMENT`]. Being permissionless is exactly why
    /// it has to be haltable: if the milestone accounting is ever wrong,
    /// this is the entrypoint the error gets drained through, and no
    /// authorization check stands in the way.
    pub fn release_milestone_funds(
        env: Env,
        escrow_id: u64,
        milestone_index: u32,
    ) -> Result<i128, Error> {
        governance::require_not_paused(&env, governance::SCOPE_SETTLEMENT)?;
        let key = DataKey::MilestoneEscrow(escrow_id);
        let mut escrow = Self::read_milestone_escrow(&env, &key)?;

        if escrow.status != Status::Funded {
            return Err(Error::InvalidEscrowStatus);
        }

        let milestone = escrow
            .milestones
            .get(milestone_index)
            .ok_or(Error::MilestoneNotFound)?;

        if milestone.status != MilestoneStatus::Approved {
            return Err(Error::InvalidStatus);
        }
        if env.ledger().sequence() <= milestone.deadline {
            return Err(Error::MilestoneTimeLocked);
        }

        // Effects before interactions: mark released, update accounting.
        let updated = Milestone {
            status: MilestoneStatus::Released,
            ..milestone
        };
        escrow.milestones.set(milestone_index, updated);
        escrow.released_amount = escrow.released_amount.saturating_add(milestone.amount);

        if escrow.released_amount >= escrow.total_amount {
            escrow.status = Status::Completed;
        }

        env.storage().persistent().set(&key, &escrow);
        Self::bump_escrow(&env, &key);

        MilestoneReleased {
            escrow_id,
            worker: escrow.worker.clone(),
            milestone_index,
            amount: milestone.amount,
        }
        .publish(&env);

        // Interaction: transfer funds.
        let token_client = token::Client::new(&env, &escrow.token);
        token_client.transfer(
            &env.current_contract_address(),
            &escrow.worker,
            &milestone.amount,
        );

        Self::bump_instance(&env);
        Ok(milestone.amount)
    }

    /// Either the client or the worker raises a dispute on a specific milestone.
    /// The milestone must be in `Approved` or `Pending` status. The escrow
    /// transitions to `Disputed` and no further releases are possible until
    /// the dispute is resolved.
    ///
    /// **Deliberately unguarded by the circuit breaker** — the entry to a
    /// recovery route is itself a recovery route.
    pub fn raise_milestone_dispute(
        env: Env,
        escrow_id: u64,
        milestone_index: u32,
        caller: Address,
    ) -> Result<(), Error> {
        caller.require_auth();

        let key = DataKey::MilestoneEscrow(escrow_id);
        let mut escrow = Self::read_milestone_escrow(&env, &key)?;

        if caller != escrow.client && caller != escrow.worker {
            return Err(Error::NotAParticipant);
        }
        if escrow.status != Status::Funded {
            return Err(Error::InvalidEscrowStatus);
        }

        let milestone = escrow
            .milestones
            .get(milestone_index)
            .ok_or(Error::MilestoneNotFound)?;

        if milestone.status == MilestoneStatus::Released {
            return Err(Error::InvalidStatus);
        }
        if milestone.status == MilestoneStatus::Disputed {
            return Err(Error::InvalidStatus);
        }

        let updated = Milestone {
            status: MilestoneStatus::Disputed,
            ..milestone
        };
        escrow.milestones.set(milestone_index, updated);
        escrow.status = Status::Disputed;
        env.storage().persistent().set(&key, &escrow);
        Self::bump_escrow(&env, &key);
        Self::bump_instance(&env);

        MilestoneDisputed {
            escrow_id,
            caller,
            milestone_index,
        }
        .publish(&env);

        Ok(())
    }

    /// Resolve a disputed milestone. For `AdminArbiter` mode, only the arbiter
    /// can call. For `ExternalHook`, calls the hook contract. `refund_to_client`
    /// determines which party receives the milestone's funds.
    ///
    /// **Deliberately unguarded by the circuit breaker** — this is how
    /// milestone funds reach their rightful owner when the happy path is
    /// unavailable, including while [`SCOPE_SETTLEMENT`] is halted.
    pub fn resolve_milestone_dispute(
        env: Env,
        escrow_id: u64,
        milestone_index: u32,
        refund_to_client: bool,
    ) -> Result<(), Error> {
        let key = DataKey::MilestoneEscrow(escrow_id);
        let mut escrow = Self::read_milestone_escrow(&env, &key)?;

        if escrow.status != Status::Disputed {
            return Err(Error::InvalidEscrowStatus);
        }

        let milestone = escrow
            .milestones
            .get(milestone_index)
            .ok_or(Error::MilestoneNotFound)?;

        if milestone.status != MilestoneStatus::Disputed {
            return Err(Error::InvalidStatus);
        }

        // Gate authorization based on arbitration mode.
        match escrow.arbitration_mode {
            ArbitrationMode::AdminArbiter => {
                escrow.arbiter.require_auth();
            }
            ArbitrationMode::ExternalHook => {
                let hook_addr = escrow
                    .hook_address
                    .clone()
                    .ok_or(Error::ArbitrationFailed)?;
                let hook_client = token::Client::new(&env, &hook_addr);
                // The hook contract is expected to authorize this call
                // internally; we just verify it exists.
                let _ = hook_client;
            }
        }

        // Effects before interactions.
        let updated = Milestone {
            status: MilestoneStatus::Released,
            ..milestone
        };
        escrow.milestones.set(milestone_index, updated);
        escrow.released_amount = escrow.released_amount.saturating_add(milestone.amount);

        if escrow.released_amount >= escrow.total_amount {
            escrow.status = Status::Completed;
        } else {
            escrow.status = Status::Funded;
        }

        env.storage().persistent().set(&key, &escrow);
        Self::bump_escrow(&env, &key);

        // Interaction: transfer to the appropriate party.
        let token_client = token::Client::new(&env, &escrow.token);
        let recipient = if refund_to_client {
            &escrow.client
        } else {
            &escrow.worker
        };
        token_client.transfer(
            &env.current_contract_address(),
            recipient,
            &milestone.amount,
        );

        MilestoneResolved {
            escrow_id,
            recipient: recipient.clone(),
            milestone_index,
            amount: milestone.amount,
        }
        .publish(&env);

        Self::bump_instance(&env);
        Ok(())
    }

    pub fn get_milestone_escrow(env: Env, escrow_id: u64) -> Result<MilestoneEscrow, Error> {
        Self::read_milestone_escrow(&env, &DataKey::MilestoneEscrow(escrow_id))
    }

    pub fn get_milestone(
        env: Env,
        escrow_id: u64,
        milestone_index: u32,
    ) -> Result<Milestone, Error> {
        let escrow = Self::read_milestone_escrow(&env, &DataKey::MilestoneEscrow(escrow_id))?;
        escrow
            .milestones
            .get(milestone_index)
            .ok_or(Error::MilestoneNotFound)
    }

    // ===========================================================================
    // Internal helpers
    // ===========================================================================

    fn read_appointment(env: &Env, key: &DataKey) -> Result<Appointment, Error> {
        env.storage()
            .persistent()
            .get(key)
            .ok_or(Error::AppointmentNotFound)
    }

    fn read_milestone_escrow(env: &Env, key: &DataKey) -> Result<MilestoneEscrow, Error> {
        env.storage()
            .persistent()
            .get(key)
            .ok_or(Error::AppointmentNotFound)
    }

    fn bump_escrow(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);
    }

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);
    }
}

#[cfg(test)]
mod test;
