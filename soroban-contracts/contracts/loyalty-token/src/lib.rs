#![no_std]

//! Loyalty rewards token for GuildWorkman.
//!
//! A SEP-41-style fungible token. The backend (as `minter`) rewards clients
//! and workers with points on completed appointments; holders can transfer
//! or burn their own balance like any other token.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, BytesN, Env, String, Vec,
};

use soroban_sdk::contractevent;

use guildworkman_governance_guard as governance;
pub use guildworkman_governance_guard::{
    PauseState, PendingRotation, PendingUpgrade, ALL_SCOPES, MAX_PAUSE_DURATION,
    MAX_PAUSE_REASON_LEN, SCOPE_INTAKE,
};

// ---------------------------------------------------------------------------
// Contract events
// ---------------------------------------------------------------------------

/// SEP-41-compatible transfer event. Topics: `["loyalty", "transfer", from, to]`;
/// data carries the amount.
#[contractevent(topics = ["loyalty", "transfer"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transfer {
    #[topic]
    pub from: Address,
    #[topic]
    pub to: Address,
    pub amount: i128,
}

/// SEP-41-compatible burn event. Topics: `["loyalty", "burn", from]`;
/// data carries the amount.
#[contractevent(topics = ["loyalty", "burn"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Burn {
    #[topic]
    pub from: Address,
    pub amount: i128,
}

/// Emitted on `mint`. Topics: `["loyalty", "mint", to]`;
/// data carries the amount.
#[contractevent(topics = ["loyalty", "mint"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mint {
    #[topic]
    pub to: Address,
    pub amount: i128,
}

/// Emitted when the minter is rotated. Topics: `["loyalty",
/// "minter_rotated", old_minter, new_minter]`.
#[contractevent(topics = ["loyalty", "minter_rotated"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MinterRotated {
    #[topic]
    pub old_minter: Address,
    #[topic]
    pub new_minter: Address,
}

/// Bump when this contract's storage layout actually changes shape and
/// needs a real transformation in `migrate`. There's no such change yet.
const CURRENT_STORAGE_VERSION: u32 = 1;

#[contracttype]
pub enum DataKey {
    Admin,
    Minter,
    Metadata,
    Balance(Address),
    Allowance(Address, Address),
}

#[contracttype]
#[derive(Clone)]
pub struct AllowanceValue {
    pub amount: i128,
    pub expiration_ledger: u32,
}

#[contracttype]
#[derive(Clone)]
pub struct TokenMetadata {
    pub decimals: u32,
    pub name: String,
    pub symbol: String,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InsufficientBalance = 3,
    InsufficientAllowance = 4,
    InvalidAmount = 5,
    NotAuthorized = 6,
    // --- Upgrade governance (see guildworkman-governance-guard) ---
    GovernanceAlreadyInitialized = 7,
    GovernanceNotInitialized = 8,
    InvalidThreshold = 9,
    DuplicateSigner = 10,
    NotASigner = 11,
    NoPendingUpgrade = 12,
    AlreadyApproved = 13,
    ProposalExpired = 14,
    HashMismatch = 15,
    AlreadyMigrated = 16,
    NothingToMigrate = 17,
    // --- Signer rotation (see guildworkman-governance-guard) ---
    NoPendingRotation = 18,
    RotationMismatch = 19,
    RotationNotReady = 20,
    RotationTimelockActive = 21,
    RotationExpired = 22,
    RotationInProgress = 23,
    // --- Emergency circuit breaker (see guildworkman-governance-guard) ---
    OperationPaused = 24,
    InvalidPauseScope = 25,
    InvalidPauseDuration = 26,
    NotPaused = 27,
    InvalidPauseReason = 28,
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

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP_AMOUNT: u32 = DAY_IN_LEDGERS * 60;
const INSTANCE_LIFETIME_THRESHOLD: u32 = DAY_IN_LEDGERS * 30;
const BALANCE_BUMP_AMOUNT: u32 = DAY_IN_LEDGERS * 30;
const BALANCE_LIFETIME_THRESHOLD: u32 = DAY_IN_LEDGERS * 29;

#[contract]
pub struct LoyaltyToken;

#[contractimpl]
impl LoyaltyToken {
    /// One-time setup. `admin` can rotate the minter; `minter` is the only
    /// address allowed to mint new reward points (typically the GuildWorkman
    /// backend's service account).
    pub fn initialize(
        env: Env,
        admin: Address,
        minter: Address,
        decimals: u32,
        name: String,
        symbol: String,
        governance_init: governance::GovernanceInit,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        governance::init_governance(&env, governance_init)?;

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Minter, &minter);
        env.storage().instance().set(
            &DataKey::Metadata,
            &TokenMetadata {
                decimals,
                name,
                symbol,
            },
        );
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
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
    /// **`mint` is the only guarded entrypoint in this contract**, under
    /// [`SCOPE_INTAKE`] — it is the sole route by which supply that does
    /// not yet exist can come into being, and therefore the only place a
    /// bug or a compromised minter can inflate the ledger.
    ///
    /// `transfer`, `transfer_from`, `approve` and `burn` are unguarded and
    /// stay callable through any pause, because they move *already-held*
    /// balances. A holder's points are their property; a token that can
    /// stop its holders from moving what they already own has not built a
    /// circuit breaker, it has built a freeze. Halting mint stops the
    /// bleeding without ever reaching into an existing balance.
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

    /// Admin-only: rotate which address is allowed to mint reward points.
    pub fn set_minter(env: Env, new_minter: Address) -> Result<(), Error> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();
        let old_minter: Address = env
            .storage()
            .instance()
            .get(&DataKey::Minter)
            .ok_or(Error::NotInitialized)?;
        env.storage().instance().set(&DataKey::Minter, &new_minter);

        MinterRotated {
            old_minter,
            new_minter,
        }
        .publish(&env);

        Ok(())
    }

    /// Minter-only: award `amount` loyalty points to `to`.
    ///
    /// Guarded by [`SCOPE_INTAKE`] — the contract's only supply-creating
    /// path. Note that `loyalty-emissions` holds this contract's `minter`
    /// role, so halting intake here also stops emission `claim`s at the
    /// token boundary even if the emission engine itself is left open.
    pub fn mint(env: Env, to: Address, amount: i128) -> Result<(), Error> {
        governance::require_not_paused(&env, governance::SCOPE_INTAKE)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        let minter: Address = env
            .storage()
            .instance()
            .get(&DataKey::Minter)
            .ok_or(Error::NotInitialized)?;
        minter.require_auth();

        Self::receive_balance(&env, to.clone(), amount);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        Mint { to, amount }.publish(&env);

        Ok(())
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        Self::read_balance(&env, id)
    }

    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        Self::read_allowance(&env, from, spender).amount
    }

    pub fn approve(
        env: Env,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) -> Result<(), Error> {
        from.require_auth();
        if amount < 0 {
            return Err(Error::InvalidAmount);
        }
        Self::write_allowance(&env, from, spender, amount, expiration_ledger);
        Ok(())
    }

    /// **Deliberately unguarded by the circuit breaker** — moves a balance
    /// the holder already owns. See `pause` for why no scope reaches it.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) -> Result<(), Error> {
        from.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        Self::spend_balance(&env, from.clone(), amount)?;
        Self::receive_balance(&env, to.clone(), amount);

        Transfer { from, to, amount }.publish(&env);

        Ok(())
    }

    pub fn transfer_from(
        env: Env,
        spender: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), Error> {
        spender.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        Self::spend_allowance(&env, from.clone(), spender, amount)?;
        Self::spend_balance(&env, from.clone(), amount)?;
        Self::receive_balance(&env, to.clone(), amount);

        Transfer { from, to, amount }.publish(&env);

        Ok(())
    }

    /// **Deliberately unguarded by the circuit breaker** — a holder
    /// destroying their own balance can never be the thing an incident
    /// needs stopped.
    pub fn burn(env: Env, from: Address, amount: i128) -> Result<(), Error> {
        from.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        Self::spend_balance(&env, from.clone(), amount)?;

        Burn { from, amount }.publish(&env);

        Ok(())
    }

    pub fn decimals(env: Env) -> u32 {
        Self::read_metadata(&env).decimals
    }

    pub fn name(env: Env) -> String {
        Self::read_metadata(&env).name
    }

    pub fn symbol(env: Env) -> String {
        Self::read_metadata(&env).symbol
    }

    // --- internal helpers ---

    fn require_admin(env: &Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)
    }

    fn read_metadata(env: &Env) -> TokenMetadata {
        env.storage()
            .instance()
            .get(&DataKey::Metadata)
            .expect("not initialized")
    }

    fn read_balance(env: &Env, addr: Address) -> i128 {
        let key = DataKey::Balance(addr);
        if let Some(balance) = env.storage().persistent().get::<_, i128>(&key) {
            env.storage().persistent().extend_ttl(
                &key,
                BALANCE_LIFETIME_THRESHOLD,
                BALANCE_BUMP_AMOUNT,
            );
            balance
        } else {
            0
        }
    }

    fn write_balance(env: &Env, addr: Address, amount: i128) {
        let key = DataKey::Balance(addr);
        env.storage().persistent().set(&key, &amount);
        env.storage().persistent().extend_ttl(
            &key,
            BALANCE_LIFETIME_THRESHOLD,
            BALANCE_BUMP_AMOUNT,
        );
    }

    fn receive_balance(env: &Env, addr: Address, amount: i128) {
        let balance = Self::read_balance(env, addr.clone());
        Self::write_balance(env, addr, balance + amount);
    }

    fn spend_balance(env: &Env, addr: Address, amount: i128) -> Result<(), Error> {
        let balance = Self::read_balance(env, addr.clone());
        if balance < amount {
            return Err(Error::InsufficientBalance);
        }
        Self::write_balance(env, addr, balance - amount);
        Ok(())
    }

    fn read_allowance(env: &Env, from: Address, spender: Address) -> AllowanceValue {
        let key = DataKey::Allowance(from, spender);
        if let Some(allowance) = env.storage().temporary().get::<_, AllowanceValue>(&key) {
            if allowance.expiration_ledger < env.ledger().sequence() {
                AllowanceValue {
                    amount: 0,
                    expiration_ledger: allowance.expiration_ledger,
                }
            } else {
                allowance
            }
        } else {
            AllowanceValue {
                amount: 0,
                expiration_ledger: 0,
            }
        }
    }

    fn write_allowance(
        env: &Env,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) {
        let allowance = AllowanceValue {
            amount,
            expiration_ledger,
        };
        let key = DataKey::Allowance(from, spender);
        env.storage().temporary().set(&key, &allowance);
        if amount > 0 && expiration_ledger > env.ledger().sequence() {
            let live_for = expiration_ledger - env.ledger().sequence();
            env.storage()
                .temporary()
                .extend_ttl(&key, live_for, live_for);
        }
    }

    fn spend_allowance(
        env: &Env,
        from: Address,
        spender: Address,
        amount: i128,
    ) -> Result<(), Error> {
        let allowance = Self::read_allowance(env, from.clone(), spender.clone());
        if allowance.amount < amount {
            return Err(Error::InsufficientAllowance);
        }
        Self::write_allowance(
            env,
            from,
            spender,
            allowance.amount - amount,
            allowance.expiration_ledger,
        );
        Ok(())
    }
}

#[cfg(test)]
mod test;
