#![no_std]

//! Cross-contract settlement router for GuildWorkman.
//!
//! Before this contract existed, `escrow`, `reputation` and `loyalty-token`
//! were three independent doors:
//!
//! - `escrow::confirm_completion` moves funds and marks the appointment
//!   `Completed` — and stops there.
//! - `reputation::submit_attestation` accepted *any* `appointment_id` with no
//!   proof that the appointment existed, was funded, or was completed.
//! - `loyalty-token::mint` is gated only on a single `minter` address, so
//!   reward points were only ever as trustworthy as whoever held that key.
//!
//! A client could review a worker for an appointment that never happened,
//! and loyalty points were minted on the backend's word rather than on
//! settled on-chain state. This contract closes both gaps by becoming the
//! *only* authority that can walk escrow → reputation → loyalty in one
//! atomic settlement.
//!
//! ## Settlement flow
//!
//! [`SettlementRouter::settle`] does exactly this, in order, all inside one
//! host invocation:
//!
//! 1. Reject a replayed `appointment_id` outright ([`Error::AlreadySettled`]).
//! 2. Read the appointment from `escrow` and require it to be `Funded`
//!    ([`Error::AppointmentNotFunded`]) — this is the on-chain proof the
//!    reputation and loyalty writes below are conditioned on.
//! 3. Mark the appointment settled (see "Idempotency" below).
//! 4. Call `escrow::confirm_completion` — releases the escrowed funds to the
//!    worker, exactly as it always did, still under the client's own
//!    authorization.
//! 5. Call `reputation::submit_attestation` — this contract's own address
//!    stands as the router `reputation` has been configured to trust (see
//!    "Reputation gating" below), so the write only succeeds because step 2
//!    already proved the appointment was funded and complete.
//! 6. Call `loyalty-token::mint` for the client and the worker, at the fixed
//!    amounts in [`RewardConfig`] — this contract must hold the token's
//!    `minter` role for this step to succeed (see "Loyalty minting" below).
//!
//! Because every one of steps 4-6 is a plain (non-`try_`) cross-contract
//! call, **any `Err` returned by the sub-contract — or a pause on its
//! side — aborts the whole transaction.** Soroban has no partial-commit
//! concept: a panic anywhere unwinds the entire invocation, so a completed
//! appointment either settles as one indivisible unit (funds released,
//! review slot unlocked, loyalty minted) or none of it happens, including
//! the idempotency marker written in step 3. There is nothing for an
//! operator to reconcile after a failed `settle` — chain state is exactly
//! as if it had never been called. The corollary is worth stating plainly:
//! if `reputation` or `loyalty-token` is paused, or simply not yet deployed
//! and wired at the addresses this router holds, `settle` fails shut rather
//! than degrading — a completed appointment's funds do **not** release
//! until reputation and loyalty are both reachable. That trade favors
//! atomicity over availability; see "Deploying under partial rollout" below
//! for how to sequence a first deployment around it.
//!
//! ## Idempotency
//!
//! [`DataKey::Settled`] is written *before* any cross-contract call is
//! made, following the same checks-effects-interactions discipline as
//! `escrow::release_milestone_funds`. A second `settle` call for the same
//! `appointment_id` is rejected at step 1 before touching any other
//! contract — a replay is never a double release or a double mint. This is
//! defense in depth, not the only guard: even without the marker,
//! `escrow::confirm_completion` itself refuses a second call once the
//! appointment is no longer `Funded`, so a bug in this router's own
//! bookkeeping could not resurrect a double payout on its own.
//!
//! ## Reputation gating
//!
//! `reputation::submit_attestation` accepts an *optional* router address
//! (`reputation::set_router`, admin-gated). Once set, a call must satisfy
//! **both**: `client.require_auth()` (unchanged — the client still consents
//! to their own rating) **and** `router.require_auth()`. The second check
//! is what closes the original hole. A contract address can only ever
//! satisfy `require_auth()` for *itself*, and only by being the contract
//! directly executing the call — there is no private key an
//! externally-owned account could sign with to forge it. So once
//! `reputation` is pointed at this router's address, the only way
//! `submit_attestation` can succeed is a call arriving from this contract's
//! own code, which by construction only ever happens after step 2 above has
//! confirmed the appointment on-chain. Deploying this router does not, by
//! itself, close the hole — an operator must also call
//! `reputation.set_router(router_address)`; see "Deploying under partial
//! rollout" below.
//!
//! ## Loyalty minting
//!
//! `loyalty-token::mint` already only trusts a single `minter` address
//! (unchanged by this contract). The migration this router calls for is
//! operational, not a code change: point that role at this router
//! (`loyalty_token.set_minter(router_address)`) the same way
//! `loyalty-emissions` already does for its own `claim` flow. `mint`'s
//! internal `minter.require_auth()` then succeeds automatically once this
//! contract's own address is that minter, for the same self-authorizing
//! reason described above — no code in `loyalty-token` needed to change.
//! Reward amounts are fixed in [`RewardConfig`], set by this contract's
//! admin, and are **never** taken from a `settle` caller's arguments —
//! letting a caller name their own mint amount would turn `settle` into an
//! unbounded mint.
//!
//! ## Deploying under partial rollout
//!
//! Because a paused or unreachable sub-contract fails the whole settlement
//! (see "Settlement flow"), wiring this router in is an ordered rollout,
//! not a single flag flip:
//!
//! 1. Deploy this contract with `initialize`, pointing it at the already-
//!    deployed `escrow`, `reputation` and `loyalty-token` addresses.
//! 2. Call `loyalty_token.set_minter(router_address)` — `loyalty-emissions`
//!    (if deployed) loses mint access at this point and must be
//!    re-pointed or retired first if it is still meant to run.
//! 3. Call `reputation.set_router(router_address)` — this is the step that
//!    actually closes the "any appointment_id" hole; direct
//!    `submit_attestation` calls that omit the router's authorization stop
//!    working the instant this lands.
//! 4. From here on, `escrow::confirm_completion` should only be reached
//!    through `settle` — a client calling it directly still works (it has
//!    no router gate of its own, by design: see `escrow`'s own docs on why
//!    fund-recovery-adjacent paths stay permissionless) but bypasses the
//!    reputation/loyalty side effects entirely, so front ends should be
//!    updated to call `settle` instead.
//!
//! ## Storage layout
//!
//! | Key | Durability | Type | Holds |
//! |-----|-----------|------|-------|
//! | `DataKey::Admin` | instance | `Address` | Configures contract addresses and `RewardConfig` |
//! | `DataKey::Escrow` / `Reputation` / `LoyaltyToken` | instance | `Address` | The three contracts this router orchestrates |
//! | `DataKey::RewardConfig` | instance | `RewardConfig` | Fixed loyalty mint amounts per settlement |
//! | `DataKey::Settled(appointment_id)` | persistent | `bool` | Idempotency marker, written before any cross-contract call |
//! | `GovernanceDataKey::*` | instance | governance-guard types | M-of-N upgrade governance and the emergency pause record |
//!
//! ## Authorization model
//!
//! - `initialize`: `admin` must authorize.
//! - `settle`: **permissionless caller** — anyone may submit the
//!   transaction, but the required authorizations (the appointment's
//!   `client`, for both `escrow::confirm_completion` and
//!   `reputation::submit_attestation`) must already be present in it, the
//!   same permissionless-relay pattern `escrow::release_milestone_funds`
//!   uses. `settle` itself calls no `require_auth` of its own — client-side
//!   tooling should simulate and sign against the `settle` entrypoint
//!   (not `confirm_completion` directly), so the resulting authorization
//!   entry's root invocation is `settle` with `confirm_completion` and
//!   `submit_attestation` as its sub-invocations. That is what binds the
//!   client's signature to the whole atomic settlement rather than to
//!   `confirm_completion` alone, which — being independently
//!   client-authorized and permissionless-to-callers on `escrow`'s own
//!   side — remains directly callable regardless of this router's
//!   existence, exactly as it always was.
//! - `set_contracts` / `set_reward_config`: admin must authorize.
//! - `pause` / `unpause`: any single governance signer (not `admin`).
//!
//! ## Emergency circuit breaker
//!
//! `settle` is guarded by [`SCOPE_SETTLEMENT`] — the same shared scope
//! `escrow::confirm_completion` and `loyalty-emissions::claim` already use,
//! so an operator broadcasting a settlement-wide pause during an incident
//! reaches this router with the identical call it already sends everywhere
//! else. There is deliberately no separate scope: this contract mints and
//! releases nothing of its own — every effect flows through the guards the
//! sub-contracts already enforce on their own entrypoints — so a second,
//! router-specific scope would only ever be paused in lockstep with
//! `SCOPE_SETTLEMENT` and would just be one more broadcast target during an
//! incident, not an independent control.

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, Address, BytesN, Env,
    String, Vec,
};

use guildworkman_governance_guard as governance;
pub use guildworkman_governance_guard::{
    PauseState, PendingRotation, PendingUpgrade, ALL_SCOPES, MAX_PAUSE_DURATION,
    MAX_PAUSE_REASON_LEN, SCOPE_SETTLEMENT,
};

/// Bump when this contract's storage layout actually changes shape and
/// needs a real transformation in `migrate`. There's no such change yet.
const CURRENT_STORAGE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Cross-contract clients
// ---------------------------------------------------------------------------
//
// Declared locally rather than depending on the `escrow` / `reputation` /
// `loyalty-token` crates directly, following the convention
// `loyalty-emissions` already established for its `loyalty-token` call:
// keeps each deployed contract's exported symbols from colliding at wasm
// link time, and keeps this crate from having to recompile against every
// sibling contract's full surface just to call the handful of functions it
// actually needs. The error enums below mirror only the numeric
// discriminants reachable through those specific calls — contract errors
// decode by discriminant, not by variant name, so these values must stay in
// lockstep with the corresponding `Error` variant in the sibling crate.

/// Mirrors `escrow::Status`. Soroban encodes a plain (all-unit-variant)
/// `#[contracttype]` enum by variant *name*, so this only has to match
/// `escrow`'s variant names, not their declaration order or discriminants.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Funded,
    Completed,
    Cancelled,
    Disputed,
    Resolved,
}

/// Mirrors `escrow::Appointment`. `#[contracttype]` structs with named
/// fields decode by field name, so every field `escrow::Appointment`
/// declares must be present here with a matching name and type, even the
/// ones this contract never reads.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Appointment {
    pub client: Address,
    pub worker: Address,
    pub token: Address,
    pub amount: i128,
    pub status: Status,
}

/// The subset of `escrow::Error` reachable through `get_appointment` and
/// `confirm_completion`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum EscrowError {
    AppointmentNotFound = 4,
    InvalidStatus = 5,
    OperationPaused = 37,
}

#[contractclient(name = "EscrowClient")]
pub trait EscrowInterface {
    fn get_appointment(env: Env, appointment_id: u64) -> Result<Appointment, EscrowError>;
    fn confirm_completion(env: Env, appointment_id: u64) -> Result<(), EscrowError>;
}

/// The subset of `reputation::Error` reachable through `submit_attestation`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum ReputationError {
    InvalidRating = 1,
    AlreadyReviewed = 2,
    NotInitialized = 5,
    InsufficientStake = 7,
    ReviewerRateLimited = 8,
    GlobalRateLimited = 9,
    SelfDealing = 10,
    OperationPaused = 29,
}

#[contractclient(name = "ReputationClient")]
pub trait ReputationInterface {
    fn submit_attestation(
        env: Env,
        appointment_id: u64,
        client: Address,
        worker: Address,
        rating: u32,
        attestation_hash: BytesN<32>,
    ) -> Result<(), ReputationError>;
}

/// The subset of `loyalty-token::Error` reachable through `mint`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum LoyaltyTokenError {
    NotInitialized = 2,
    InvalidAmount = 5,
    OperationPaused = 24,
}

#[contractclient(name = "LoyaltyTokenClient")]
pub trait LoyaltyTokenInterface {
    fn mint(env: Env, to: Address, amount: i128) -> Result<(), LoyaltyTokenError>;
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[contracttype]
pub enum DataKey {
    Admin,
    Escrow,
    Reputation,
    LoyaltyToken,
    RewardConfig,
    /// `appointment_id` -> `true` once settled. Presence alone is the
    /// signal; the value is always `true` when the key exists.
    Settled(u64),
}

/// Fixed loyalty-point amounts minted on a successful settlement. Set by
/// the admin at `initialize` and updatable via `set_reward_config` —
/// **never** taken from a `settle` caller's arguments, since that would let
/// any caller name their own mint amount.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RewardConfig {
    /// Points minted to the client on settlement. `0` disables the client
    /// mint entirely (no zero-amount `mint` call is made).
    pub client_reward: i128,
    /// Points minted to the worker on settlement. `0` disables the worker
    /// mint entirely.
    pub worker_reward: i128,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidRewardConfig = 3,
    AlreadySettled = 4,
    AppointmentNotFunded = 5,
    // --- Upgrade governance (see guildworkman-governance-guard) ---
    GovernanceAlreadyInitialized = 6,
    GovernanceNotInitialized = 7,
    InvalidThreshold = 8,
    DuplicateSigner = 9,
    NotASigner = 10,
    NoPendingUpgrade = 11,
    AlreadyApproved = 12,
    ProposalExpired = 13,
    HashMismatch = 14,
    AlreadyMigrated = 15,
    NothingToMigrate = 16,
    // --- Signer rotation (see guildworkman-governance-guard) ---
    NoPendingRotation = 17,
    RotationMismatch = 18,
    RotationNotReady = 19,
    RotationTimelockActive = 20,
    RotationExpired = 21,
    RotationInProgress = 22,
    // --- Emergency circuit breaker (see guildworkman-governance-guard) ---
    OperationPaused = 23,
    InvalidPauseScope = 24,
    InvalidPauseDuration = 25,
    NotPaused = 26,
    InvalidPauseReason = 27,
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

const DAY_IN_LEDGERS: u32 = 17_280; // ~5s per ledger
const INSTANCE_BUMP_AMOUNT: u32 = DAY_IN_LEDGERS * 60;
const INSTANCE_LIFETIME_THRESHOLD: u32 = DAY_IN_LEDGERS * 30;
const SETTLED_BUMP_AMOUNT: u32 = DAY_IN_LEDGERS * 30;
const SETTLED_LIFETIME_THRESHOLD: u32 = DAY_IN_LEDGERS * 29;

#[contract]
pub struct SettlementRouter;

#[contractimpl]
impl SettlementRouter {
    /// One-time setup. `admin` configures the three orchestrated contract
    /// addresses and the loyalty `RewardConfig`; it is unrelated to
    /// `governance_init`, which gates upgrades and the pause, exactly as in
    /// the sibling contracts.
    ///
    /// Wiring the sub-contracts to actually trust this router's address
    /// (`loyalty_token.set_minter`, `reputation.set_router`) is a separate,
    /// deliberately manual step — see the crate-level "Deploying under
    /// partial rollout" notes.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        escrow: Address,
        reputation: Address,
        loyalty_token: Address,
        reward_config: RewardConfig,
        governance_init: governance::GovernanceInit,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        Self::validate_reward_config(&reward_config)?;
        governance::init_governance(&env, governance_init)?;

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Escrow, &escrow);
        env.storage()
            .instance()
            .set(&DataKey::Reputation, &reputation);
        env.storage()
            .instance()
            .set(&DataKey::LoyaltyToken, &loyalty_token);
        env.storage()
            .instance()
            .set(&DataKey::RewardConfig, &reward_config);
        Self::bump_instance(&env);
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

    pub fn propose_signer_rotation(
        env: Env,
        proposer: Address,
        new_signers: Vec<Address>,
        new_threshold: u32,
    ) -> Result<bool, Error> {
        governance::propose_signer_rotation(&env, proposer, new_signers, new_threshold)
            .map_err(Into::into)
    }

    pub fn approve_signer_rotation(
        env: Env,
        approver: Address,
        new_signers: Vec<Address>,
        new_threshold: u32,
    ) -> Result<bool, Error> {
        governance::approve_signer_rotation(&env, approver, new_signers, new_threshold)
            .map_err(Into::into)
    }

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
    /// Only [`SCOPE_SETTLEMENT`] has teeth here, and it is the same shared
    /// scope `escrow::confirm_completion` and `loyalty-emissions::claim`
    /// use — see the crate-level "Emergency circuit breaker" notes for why
    /// this router deliberately defines no scope of its own.
    pub fn pause(
        env: Env,
        caller: Address,
        scopes: u32,
        duration_secs: u64,
        reason: String,
    ) -> Result<PauseState, Error> {
        governance::pause(&env, caller, scopes, duration_secs, reason).map_err(Into::into)
    }

    pub fn unpause(env: Env, caller: Address, scopes: u32) -> Result<u32, Error> {
        governance::unpause(&env, caller, scopes).map_err(Into::into)
    }

    pub fn get_pause_state(env: Env) -> Option<PauseState> {
        governance::get_pause_state(&env)
    }

    pub fn paused_scopes(env: Env) -> u32 {
        governance::paused_scopes(&env)
    }

    pub fn is_paused(env: Env, scope: u32) -> bool {
        governance::is_paused(&env, scope)
    }

    // ----- Admin configuration -----

    /// Admin-only: repoint the three orchestrated contract addresses, e.g.
    /// after redeploying one of them. Does **not** re-run any of the
    /// "Deploying under partial rollout" wiring on the sub-contracts
    /// themselves (`set_minter`, `set_router`) — those must still be called
    /// separately against the new addresses.
    pub fn set_contracts(
        env: Env,
        escrow: Address,
        reputation: Address,
        loyalty_token: Address,
    ) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Escrow, &escrow);
        env.storage()
            .instance()
            .set(&DataKey::Reputation, &reputation);
        env.storage()
            .instance()
            .set(&DataKey::LoyaltyToken, &loyalty_token);
        Self::bump_instance(&env);
        Ok(())
    }

    /// Admin-only: update the fixed loyalty reward amounts future
    /// settlements mint. Does not affect appointments already settled.
    pub fn set_reward_config(env: Env, reward_config: RewardConfig) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();
        Self::validate_reward_config(&reward_config)?;
        env.storage()
            .instance()
            .set(&DataKey::RewardConfig, &reward_config);
        Self::bump_instance(&env);
        Ok(())
    }

    // ----- Settlement -----

    /// Atomically settles a completed appointment: releases escrowed funds,
    /// records the client's attestation, and mints loyalty points to both
    /// parties — or none of it happens. See the crate-level docs for the
    /// full flow, idempotency, and failure semantics.
    ///
    /// Permissionless caller: `settle` itself performs no `require_auth`.
    /// The authorizations it depends on — the appointment's `client`, via
    /// `escrow::confirm_completion` and `reputation::submit_attestation` —
    /// must already be present in the submitted transaction, the same
    /// relay pattern `escrow::release_milestone_funds` uses. `rating` and
    /// `attestation_hash` are exactly what a direct `submit_attestation`
    /// caller would supply; this router adds no interpretation of its own.
    ///
    /// Guarded by [`SCOPE_SETTLEMENT`].
    pub fn settle(
        env: Env,
        appointment_id: u64,
        rating: u32,
        attestation_hash: BytesN<32>,
    ) -> Result<(), Error> {
        governance::require_not_paused(&env, governance::SCOPE_SETTLEMENT)?;

        let settled_key = DataKey::Settled(appointment_id);
        if env.storage().persistent().has(&settled_key) {
            return Err(Error::AlreadySettled);
        }

        let escrow_addr = Self::read_escrow(&env);
        let escrow_client = EscrowClient::new(&env, &escrow_addr);
        let appointment = escrow_client.get_appointment(&appointment_id);
        if appointment.status != Status::Funded {
            return Err(Error::AppointmentNotFunded);
        }

        // Effect before interactions: a replay never reaches any
        // cross-contract call, and any panic below reverts this write too.
        env.storage().persistent().set(&settled_key, &true);
        env.storage().persistent().extend_ttl(
            &settled_key,
            SETTLED_LIFETIME_THRESHOLD,
            SETTLED_BUMP_AMOUNT,
        );

        // Interactions: any Err here panics the whole transaction, so
        // funds, the attestation, and loyalty minting land together or not
        // at all.
        escrow_client.confirm_completion(&appointment_id);

        let reputation_client = ReputationClient::new(&env, &Self::read_reputation(&env));
        reputation_client.submit_attestation(
            &appointment_id,
            &appointment.client,
            &appointment.worker,
            &rating,
            &attestation_hash,
        );

        let reward_config = Self::read_reward_config(&env);
        let loyalty_client = LoyaltyTokenClient::new(&env, &Self::read_loyalty_token(&env));
        if reward_config.client_reward > 0 {
            loyalty_client.mint(&appointment.client, &reward_config.client_reward);
        }
        if reward_config.worker_reward > 0 {
            loyalty_client.mint(&appointment.worker, &reward_config.worker_reward);
        }

        Self::bump_instance(&env);
        Ok(())
    }

    // ----- Views -----

    pub fn is_settled(env: Env, appointment_id: u64) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::Settled(appointment_id))
    }

    pub fn get_admin(env: Env) -> Result<Address, Error> {
        Self::require_admin(&env)
    }

    pub fn get_escrow(env: Env) -> Address {
        Self::read_escrow(&env)
    }

    pub fn get_reputation(env: Env) -> Address {
        Self::read_reputation(&env)
    }

    pub fn get_loyalty_token(env: Env) -> Address {
        Self::read_loyalty_token(&env)
    }

    pub fn get_reward_config(env: Env) -> RewardConfig {
        Self::read_reward_config(&env)
    }

    // ----- Internal helpers -----

    fn require_admin(env: &Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)
    }

    fn validate_reward_config(config: &RewardConfig) -> Result<(), Error> {
        if config.client_reward < 0 || config.worker_reward < 0 {
            return Err(Error::InvalidRewardConfig);
        }
        Ok(())
    }

    fn read_escrow(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Escrow)
            .expect("not initialized")
    }

    fn read_reputation(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Reputation)
            .expect("not initialized")
    }

    fn read_loyalty_token(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::LoyaltyToken)
            .expect("not initialized")
    }

    fn read_reward_config(env: &Env) -> RewardConfig {
        env.storage()
            .instance()
            .get(&DataKey::RewardConfig)
            .expect("not initialized")
    }

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
    }
}

#[cfg(test)]
mod test;
