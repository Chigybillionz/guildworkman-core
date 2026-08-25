#![no_std]

//! Sybil-resistant weighted reputation scoring contract for GuildWorkman.
//!
//! Computes time-decayed, stake-weighted scores from signed attestations
//! while resisting Sybil, collusion, and self-dealing attacks.
//!
//! ## Settlement router gating
//!
//! `submit_attestation` accepts *any* `appointment_id` on its own — nothing
//! in this contract proves the appointment existed in `escrow`, was
//! funded, or was completed. `set_router` (admin-only) closes that gap:
//! once a router address is configured, `submit_attestation` additionally
//! requires `router.require_auth()` alongside the client's own
//! authorization. A contract address can only ever satisfy
//! `require_auth()` for *itself*, and only by directly executing the call —
//! there is no key an externally-owned account could sign with to forge
//! it — so once `Router` is set to a deployed
//! `guildworkman-settlement-router`, the only way this function can
//! succeed is a call arriving from that router's own code, which by
//! construction only happens after it has confirmed the appointment
//! on-chain. See that crate's docs for the full settlement flow.
//!
//! `Router` defaults to unset, which preserves this contract's original,
//! client-authorized-only behavior — the gate is opt-in per deployment
//! until an operator finishes wiring a router in.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, BytesN, Env, String, Vec,
};

use soroban_sdk::contractevent;

use guildworkman_governance_guard as governance;
pub use guildworkman_governance_guard::{
    PauseState, PendingRotation, PendingUpgrade, ALL_SCOPES, MAX_PAUSE_DURATION,
    MAX_PAUSE_REASON_LEN, SCOPE_ATTESTATION,
};

// ---------------------------------------------------------------------------
// Contract events
// ---------------------------------------------------------------------------

/// Emitted when a client submits an attestation for a worker. Topics:
/// `["rep", "attested", appointment_id, client, worker]`; data carries the
/// rating.
#[contractevent(topics = ["rep", "attested"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttestationSubmitted {
    #[topic]
    pub appointment_id: u64,
    #[topic]
    pub client: Address,
    #[topic]
    pub worker: Address,
    pub rating: u32,
}

/// Bump when this contract's storage layout actually changes shape and
/// needs a real transformation in `migrate`. There's no such change yet —
/// this just proves the version-gated migration path end to end and gives
/// a real future migration somewhere to hook in.
const CURRENT_STORAGE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DAY_IN_LEDGERS: u32 = 17_280; // ~5 s per ledger
const LEDGERS_THRESHOLD: u32 = DAY_IN_LEDGERS; // ~1 day
const LEDGERS_EXTEND_TO: u32 = DAY_IN_LEDGERS * 30; // ~30 days
const SCALE: i128 = 10_000; // fixed-point scale for decay weights

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Immutable configuration, set once at `initialize`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// Rate-limit window length in ledgers (~5 s each).
    pub window: u32,
    /// Max attestations per reviewer per window.
    pub reviewer_cap: u32,
    /// Max attestations globally per window (circuit breaker).
    pub global_cap: u32,
    /// Minimum stake required to submit an attestation (0 = disabled).
    pub min_stake: u64,
    /// Decay rate per ledger in basis points (e.g. 1 = 0.01%).
    pub decay_rate_bps: u32,
    /// Maximum age in ledgers before an attestation decays to zero weight.
    pub max_age_ledgers: u32,
}

/// A signed attestation from a client about a worker's performance.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Attestation {
    pub client: Address,
    pub worker: Address,
    pub appointment_id: u64,
    pub rating: u32,
    /// Client's stake at the time of attestation.
    pub stake_at_time: u64,
    /// Ledger sequence when submitted.
    pub ledger_submitted: u32,
    /// SHA-256 hash of canonical attestation data for integrity.
    pub attestation_hash: BytesN<32>,
}

/// On-chain incremental weighted score accumulator.
#[contracttype]
#[derive(Clone, Copy, Debug, Default)]
pub struct WeightedScoreAccumulator {
    /// Sum of (rating * stake * decay_weight), scaled by SCALE.
    pub weighted_sum: i128,
    /// Sum of (stake * decay_weight), scaled by SCALE.
    pub weight_sum: i128,
    /// Number of attestations contributing.
    pub count: u32,
    /// Last ledger at which the accumulator was updated.
    pub last_update_ledger: u32,
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

#[contracttype]
pub enum DataKey {
    // Instance (singletons)
    Admin,
    Config,
    /// Optional settlement-router address (`guildworkman-settlement-router`).
    /// `None` until an admin calls `set_router`. See `submit_attestation`.
    Router,
    // Persistent (entity data)
    Reviewed(u64),
    Review(Address, u32),
    ReviewCount(Address),
    WeightedScore(Address),
    Stake(Address),
    // Temporary (rate-limit tumbling windows)
    ReviewerWindow(Address, u64),
    GlobalReviewWindow(u64),
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Error {
    InvalidRating = 1,
    AlreadyReviewed = 2,
    NoReviews = 3,
    AlreadyInitialized = 4,
    NotInitialized = 5,
    InvalidConfig = 6,
    InsufficientStake = 7,
    ReviewerRateLimited = 8,
    GlobalRateLimited = 9,
    SelfDealing = 10,
    Unauthorized = 11,
    // --- Upgrade governance (see guildworkman-governance-guard) ---
    GovernanceAlreadyInitialized = 12,
    GovernanceNotInitialized = 13,
    InvalidThreshold = 14,
    DuplicateSigner = 15,
    NotASigner = 16,
    NoPendingUpgrade = 17,
    AlreadyApproved = 18,
    ProposalExpired = 19,
    HashMismatch = 20,
    AlreadyMigrated = 21,
    NothingToMigrate = 22,
    // --- Signer rotation (see guildworkman-governance-guard) ---
    NoPendingRotation = 23,
    RotationMismatch = 24,
    RotationNotReady = 25,
    RotationTimelockActive = 26,
    RotationExpired = 27,
    RotationInProgress = 28,
    // --- Emergency circuit breaker (see guildworkman-governance-guard) ---
    OperationPaused = 29,
    InvalidPauseScope = 30,
    InvalidPauseDuration = 31,
    NotPaused = 32,
    InvalidPauseReason = 33,
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

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct ReputationContract;

#[contractimpl]
impl ReputationContract {
    // ----- Initialization & admin -----

    /// `signers`/`threshold` configure the M-of-N governance guard that
    /// gates upgrading this contract's code (`propose_upgrade` /
    /// `approve_upgrade`) and running a post-upgrade migration
    /// (`migrate`). This is entirely separate from `admin` above — a
    /// compromised or colluding signer set can only ever swap this
    /// contract's code, never call `update_config`, `set_stake`, or
    /// `recalculate_score` directly.
    pub fn initialize(
        env: Env,
        admin: Address,
        config: Config,
        governance_init: governance::GovernanceInit,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        Self::validate_config(&config)?;
        governance::init_governance(&env, governance_init)?;

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Config, &config);
        Self::bump_instance(&env);
        Ok(())
    }

    // ----- Upgrade governance -----

    /// Opens (or, with a 1-of-N guard, immediately executes) a proposal to
    /// replace this contract's code with `wasm_hash`, which must already
    /// be uploaded on the network. Returns `true` if this call's approval
    /// reached the configured threshold, in which case the swap has
    /// already happened — it takes effect after this invocation finishes.
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

    /// Approves the currently pending upgrade proposal. Returns `true` if
    /// this approval reached the threshold, in which case the swap has
    /// already happened.
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

    /// Any signer can cancel the pending proposal outright, whether or
    /// not they approved it.
    pub fn cancel_upgrade(env: Env, caller: Address) -> Result<(), Error> {
        governance::cancel_upgrade(&env, caller).map_err(Into::into)
    }

    // ----- Signer rotation -----

    /// Opens a proposal to rotate the governance signer set and threshold,
    /// authorized by the current signers at the current threshold. The
    /// change is timelocked: reaching threshold only *schedules* it — it is
    /// applied by `execute_signer_rotation` after the delay. Returns `true`
    /// if this call reached threshold (rotation now scheduled).
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

    /// Runs this code version's storage migration, if one is owed. Callable
    /// by any signer (not the full threshold) — the risky decision, which
    /// code to trust, was already gated by the upgrade's multi-sig
    /// threshold; this just runs the migration that code ships with.
    /// A no-op contract upgrade (no storage shape change) has nothing to
    /// migrate and this returns `NothingToMigrate`.
    pub fn migrate(env: Env, signer: Address) -> Result<(), Error> {
        governance::require_signer(&env, &signer)?;

        if governance::current_storage_version(&env) >= CURRENT_STORAGE_VERSION {
            return Err(Error::NothingToMigrate);
        }

        // No storage shape has changed since v1 — nothing to transform.
        // A real future migration adds its field-by-field logic here,
        // guarded by the same version check above.

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
    /// The only scope with teeth here is [`SCOPE_ATTESTATION`], which halts
    /// `submit_attestation`. Scores hold no funds, so nothing in this
    /// contract can be stranded by a pause; the reason to halt it is a
    /// scoring incident — a Sybil ring or a collusion pattern poisoning the
    /// aggregate — which is deliberately independent of anything happening
    /// in `escrow`. Reads (`get_reputation_score_x10000` and friends) stay
    /// open throughout: a consumer must always be able to see the scores
    /// that already exist, including to notice they went bad.
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
    /// halted.
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

    pub fn update_config(env: Env, config: Config) -> Result<(), Error> {
        Self::require_admin(&env)?;
        Self::validate_config(&config)?;
        env.storage().instance().set(&DataKey::Config, &config);
        Self::bump_instance(&env);
        Ok(())
    }

    /// Admin-only: point this contract at a deployed settlement router.
    /// Once set, `submit_attestation` requires that router's authorization
    /// in addition to the client's — see the crate-level "Settlement
    /// router gating" notes. Passing a fresh address here is also how a
    /// router upgrade or redeploy gets wired in; there is no "unset" call,
    /// since a reputation contract that has ever required router
    /// authorization should not silently fall back to accepting
    /// unverified attestations again.
    pub fn set_router(env: Env, router: Address) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Router, &router);
        Self::bump_instance(&env);
        Ok(())
    }

    pub fn get_router(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Router)
    }

    pub fn set_stake(env: Env, user: Address, stake: u64) -> Result<(), Error> {
        Self::require_admin(&env)?;
        let key = DataKey::Stake(user);
        env.storage().persistent().set(&key, &stake);
        env.storage()
            .persistent()
            .extend_ttl(&key, LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);
        Ok(())
    }

    // ----- Core submission -----

    /// Guarded by [`SCOPE_ATTESTATION`]: this is the contract's only
    /// state-poisoning surface, and halting it is what buys time to ship a
    /// scoring fix without also freezing payments in `escrow`.
    pub fn submit_attestation(
        env: Env,
        appointment_id: u64,
        client: Address,
        worker: Address,
        rating: u32,
        attestation_hash: BytesN<32>,
    ) -> Result<(), Error> {
        // 0. Circuit breaker, ahead of everything else: a halted operation
        //    should cost nothing and touch no storage.
        governance::require_not_paused(&env, governance::SCOPE_ATTESTATION)?;

        // 1. Authorization (signed attestation via Soroban auth).
        client.require_auth();

        // 1b. If a settlement router is configured, this call must also be
        // arriving from that router's own code — see the crate-level
        // "Settlement router gating" notes for why this closes off
        // submitting an attestation for an appointment that was never
        // verified against escrow.
        if let Some(router) = Self::get_router(env.clone()) {
            router.require_auth();
        }

        // 2. Self-dealing prevention.
        if client == worker {
            return Err(Error::SelfDealing);
        }

        // 3. Rating validation.
        if !(1..=5).contains(&rating) {
            return Err(Error::InvalidRating);
        }

        // 4. One review per appointment.
        let reviewed_key = DataKey::Reviewed(appointment_id);
        if env.storage().persistent().has(&reviewed_key) {
            return Err(Error::AlreadyReviewed);
        }

        // 5. Load config.
        let config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(Error::NotInitialized)?;

        // 6. Stake check.
        let stake: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::Stake(client.clone()))
            .unwrap_or(0);
        if config.min_stake > 0 && stake < config.min_stake {
            return Err(Error::InsufficientStake);
        }

        // 7. Per-reviewer rate limit.
        let window_index = Self::window_index(&env, &config);
        let reviewer_key = DataKey::ReviewerWindow(client.clone(), window_index);
        let reviewer_count: u32 = env.storage().temporary().get(&reviewer_key).unwrap_or(0);
        if reviewer_count >= config.reviewer_cap {
            return Err(Error::ReviewerRateLimited);
        }

        // 8. Global rate limit.
        let global_key = DataKey::GlobalReviewWindow(window_index);
        let global_count: u32 = env.storage().temporary().get(&global_key).unwrap_or(0);
        if global_count >= config.global_cap {
            return Err(Error::GlobalRateLimited);
        }

        // --- All checks passed, mutate state ---

        let current_ledger = env.ledger().sequence();

        // Mark appointment as reviewed.
        env.storage().persistent().set(&reviewed_key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&reviewed_key, LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);

        // Store attestation.
        let count_key = DataKey::ReviewCount(worker.clone());
        let index: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
        let attestation = Attestation {
            client: client.clone(),
            worker: worker.clone(),
            appointment_id,
            rating,
            stake_at_time: stake,
            ledger_submitted: current_ledger,
            attestation_hash,
        };
        let review_key = DataKey::Review(worker.clone(), index);
        env.storage().persistent().set(&review_key, &attestation);
        env.storage()
            .persistent()
            .extend_ttl(&review_key, LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);
        env.storage().persistent().set(&count_key, &(index + 1));
        env.storage()
            .persistent()
            .extend_ttl(&count_key, LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);

        // Update weighted score accumulator.
        let effective_stake = if stake == 0 { 1 } else { stake }; // legacy compat
        let decay_weight = Self::compute_decay_weight(0, &config); // age=0 at submission
        let weighted_contribution = rating as i128 * effective_stake as i128 * decay_weight;
        let weight_contribution = effective_stake as i128 * decay_weight;

        let acc_key = DataKey::WeightedScore(worker.clone());
        let mut acc: WeightedScoreAccumulator =
            env.storage().persistent().get(&acc_key).unwrap_or_default();
        acc.weighted_sum += weighted_contribution;
        acc.weight_sum += weight_contribution;
        acc.count += 1;
        acc.last_update_ledger = current_ledger;
        env.storage().persistent().set(&acc_key, &acc);
        env.storage()
            .persistent()
            .extend_ttl(&acc_key, LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);

        // Increment rate-limit counters.
        let window_ttl = config.window * 2;
        env.storage()
            .temporary()
            .set(&reviewer_key, &(reviewer_count + 1));
        env.storage()
            .temporary()
            .extend_ttl(&reviewer_key, window_ttl, window_ttl);

        env.storage()
            .temporary()
            .set(&global_key, &(global_count + 1));
        env.storage()
            .temporary()
            .extend_ttl(&global_key, window_ttl, window_ttl);

        AttestationSubmitted {
            appointment_id,
            client,
            worker,
            rating,
        }
        .publish(&env);

        Ok(())
    }

    // ----- Score queries -----

    /// Weighted reputation score scaled by 10_000 (e.g. 43725 = 4.3725 out of 5.0000).
    pub fn get_reputation_score_x10000(env: Env, worker: Address) -> Result<u64, Error> {
        let acc = Self::get_weighted_score(env, worker.clone())?;
        if acc.weight_sum == 0 {
            return Err(Error::NoReviews);
        }
        // weighted_sum/weight_sum = average rating. Multiply by 10000 to get 0..50000 range.
        Ok(((acc.weighted_sum * SCALE) / acc.weight_sum) as u64)
    }

    pub fn get_weighted_score(
        env: Env,
        worker: Address,
    ) -> Result<WeightedScoreAccumulator, Error> {
        let key = DataKey::WeightedScore(worker);
        env.storage().persistent().get(&key).ok_or(Error::NoReviews)
    }

    pub fn get_attestation(env: Env, worker: Address, index: u32) -> Option<Attestation> {
        env.storage()
            .persistent()
            .get(&DataKey::Review(worker, index))
    }

    pub fn get_attestation_count(env: Env, worker: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ReviewCount(worker))
            .unwrap_or(0)
    }

    pub fn get_stake(env: Env, user: Address) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::Stake(user))
            .unwrap_or(0)
    }

    pub fn get_admin(env: Env) -> Result<Address, Error> {
        Self::require_admin(&env)
    }

    pub fn get_config(env: Env) -> Result<Config, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(Error::NotInitialized)
    }

    // ----- Admin recalculation -----

    /// Full rescan of all attestations for a worker, recomputing the
    /// accumulator from scratch with current decay. Corrects drift from
    /// incremental accumulation.
    pub fn recalculate_score(env: Env, worker: Address) -> Result<(), Error> {
        Self::require_admin(&env)?;

        let config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(Error::NotInitialized)?;

        let count = Self::get_attestation_count(env.clone(), worker.clone());
        let current_ledger = env.ledger().sequence();

        let mut weighted_sum: i128 = 0;
        let mut weight_sum: i128 = 0;

        for i in 0..count {
            let key = DataKey::Review(worker.clone(), i);
            let att: Attestation = env.storage().persistent().get(&key).unwrap();
            let age = current_ledger.saturating_sub(att.ledger_submitted);
            let decay_weight = Self::compute_decay_weight(age, &config);
            if decay_weight <= 0 {
                continue;
            }
            let effective_stake = if att.stake_at_time == 0 {
                1i128
            } else {
                att.stake_at_time as i128
            };
            weighted_sum += att.rating as i128 * effective_stake * decay_weight;
            weight_sum += effective_stake * decay_weight;
        }

        let acc_key = DataKey::WeightedScore(worker);
        let acc = WeightedScoreAccumulator {
            weighted_sum,
            weight_sum,
            count,
            last_update_ledger: current_ledger,
        };
        env.storage().persistent().set(&acc_key, &acc);
        env.storage()
            .persistent()
            .extend_ttl(&acc_key, LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);

        Ok(())
    }

    // ----- Private helpers -----

    fn require_admin(env: &Env) -> Result<Address, Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        admin.require_auth();
        Ok(admin)
    }

    fn validate_config(config: &Config) -> Result<(), Error> {
        if config.window == 0
            || config.reviewer_cap == 0
            || config.global_cap == 0
            || config.decay_rate_bps == 0
            || config.max_age_ledgers == 0
        {
            return Err(Error::InvalidConfig);
        }
        Ok(())
    }

    fn window_index(env: &Env, config: &Config) -> u64 {
        env.ledger().sequence() as u64 / config.window as u64
    }

    /// Compute decay weight for an attestation of given age.
    /// Returns 0 if fully decayed.
    fn compute_decay_weight(age: u32, config: &Config) -> i128 {
        if age >= config.max_age_ledgers {
            return 0;
        }
        let decay = age as i128 * config.decay_rate_bps as i128;
        let weight = SCALE - decay;
        if weight < 0 {
            0
        } else {
            weight
        }
    }

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(LEDGERS_THRESHOLD, LEDGERS_EXTEND_TO);
    }
}

#[cfg(test)]
mod test;
