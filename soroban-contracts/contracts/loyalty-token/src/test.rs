#![cfg(test)]

use super::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::Env;

fn setup() -> (Env, LoyaltyTokenClient<'static>, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let minter = Address::generate(&env);
    let user = Address::generate(&env);

    let contract_id = env.register(LoyaltyToken, ());
    let contract = LoyaltyTokenClient::new(&env, &contract_id);

    let mut signers = soroban_sdk::Vec::new(&env);
    signers.push_back(Address::generate(&env));
    signers.push_back(Address::generate(&env));
    signers.push_back(Address::generate(&env));

    contract.initialize(
        &admin,
        &minter,
        &2,
        &String::from_str(&env, "GuildWorkman Points"),
        &String::from_str(&env, "GWP"),
        &governance::GovernanceInit {
            signers,
            threshold: 2,
        },
    );

    (env, contract, admin, minter, user)
}

fn setup_with_signers() -> (Env, LoyaltyTokenClient<'static>, soroban_sdk::Vec<Address>) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let minter = Address::generate(&env);

    let contract_id = env.register(LoyaltyToken, ());
    let contract = LoyaltyTokenClient::new(&env, &contract_id);

    let mut signers = soroban_sdk::Vec::new(&env);
    signers.push_back(Address::generate(&env));
    signers.push_back(Address::generate(&env));
    signers.push_back(Address::generate(&env));

    contract.initialize(
        &admin,
        &minter,
        &2,
        &String::from_str(&env, "GuildWorkman Points"),
        &String::from_str(&env, "GWP"),
        &governance::GovernanceInit {
            signers: signers.clone(),
            threshold: 2,
        },
    );

    (env, contract, signers)
}

fn dummy_hash(env: &Env) -> soroban_sdk::BytesN<32> {
    soroban_sdk::BytesN::from_array(env, &[0u8; 32])
}

#[test]
fn mint_and_balance() {
    let (_env, contract, _admin, _minter, user) = setup();
    contract.mint(&user, &1_000);
    assert_eq!(contract.balance(&user), 1_000);
}

#[test]
fn transfer_moves_balance() {
    let (env, contract, _admin, _minter, user) = setup();
    let other = Address::generate(&env);

    contract.mint(&user, &500);
    contract.transfer(&user, &other, &200);

    assert_eq!(contract.balance(&user), 300);
    assert_eq!(contract.balance(&other), 200);
}

#[test]
fn transfer_more_than_balance_fails() {
    let (env, contract, _admin, _minter, user) = setup();
    let other = Address::generate(&env);
    contract.mint(&user, &100);

    let result = contract.try_transfer(&user, &other, &200);
    assert_eq!(result, Err(Ok(Error::InsufficientBalance)));
}

#[test]
fn approve_and_transfer_from() {
    let (env, contract, _admin, _minter, user) = setup();
    let spender = Address::generate(&env);
    let recipient = Address::generate(&env);

    contract.mint(&user, &1_000);
    contract.approve(&user, &spender, &300, &(env.ledger().sequence() + 1_000));
    assert_eq!(contract.allowance(&user, &spender), 300);

    contract.transfer_from(&spender, &user, &recipient, &300);
    assert_eq!(contract.balance(&recipient), 300);
    assert_eq!(contract.balance(&user), 700);
    assert_eq!(contract.allowance(&user, &spender), 0);
}

#[test]
fn burn_reduces_balance() {
    let (_env, contract, _admin, _minter, user) = setup();
    contract.mint(&user, &400);
    contract.burn(&user, &150);
    assert_eq!(contract.balance(&user), 250);
}

#[test]
fn admin_can_rotate_minter() {
    let (env, contract, _admin, _minter, user) = setup();
    let new_minter = Address::generate(&env);

    contract.set_minter(&new_minter);
    contract.mint(&user, &50);
    assert_eq!(contract.balance(&user), 50);
}

// ===========================================================================
// Upgrade governance — see the equivalent block in reputation/src/test.rs
// for why these all stop one approval short of the configured threshold.
// ===========================================================================

#[test]
fn initialize_stores_governance_config() {
    let (_env, contract, signers) = setup_with_signers();
    assert_eq!(contract.get_signers(), signers);
    assert_eq!(contract.get_upgrade_threshold(), 2);
    assert_eq!(contract.get_storage_version(), 1);
}

#[test]
fn propose_upgrade_by_non_signer_fails() {
    let (env, contract, _signers) = setup_with_signers();
    let outsider = Address::generate(&env);
    let result = contract.try_propose_upgrade(&outsider, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::NotASigner)));
}

#[test]
fn approve_upgrade_below_threshold_is_not_ready() {
    let (env, contract, signers) = setup_with_signers();
    let target = dummy_hash(&env);

    let ready = contract.propose_upgrade(&signers.get_unchecked(0), &target);
    assert!(!ready);

    let pending = contract.get_pending_upgrade().unwrap();
    assert_eq!(pending.approvals.len(), 1);
}

#[test]
fn cancel_upgrade_by_a_non_approving_signer_still_works() {
    let (env, contract, signers) = setup_with_signers();
    contract.propose_upgrade(&signers.get_unchecked(0), &dummy_hash(&env));

    contract.cancel_upgrade(&signers.get_unchecked(2));
    assert!(contract.get_pending_upgrade().is_none());
}

#[test]
fn migrate_with_nothing_to_migrate_fails() {
    let (_env, contract, signers) = setup_with_signers();
    let result = contract.try_migrate(&signers.get_unchecked(0));
    assert_eq!(result, Err(Ok(Error::NothingToMigrate)));
}

#[test]
fn migrate_by_non_signer_fails() {
    let (env, contract, _signers) = setup_with_signers();
    let outsider = Address::generate(&env);
    let result = contract.try_migrate(&outsider);
    assert_eq!(result, Err(Ok(Error::NotASigner)));
}

// ===========================================================================
// Emergency circuit breaker
// ===========================================================================
//
// The breaker's own semantics are unit-tested in
// `guildworkman-governance-guard`. The property that matters here is narrow
// and absolute: `mint` is halted, and nothing a holder does with a balance
// they already own ever is.

/// Reason string for tests that don't exercise the field itself. Kept
/// non-empty so the round-trip through storage is actually covered by every
/// pause test rather than only the ones that look at it.
fn reason(env: &Env) -> soroban_sdk::String {
    soroban_sdk::String::from_str(env, "INC-000 test")
}

fn set_time(env: &Env, timestamp: u64) {
    use soroban_sdk::testutils::Ledger as _;
    env.ledger().with_mut(|l| l.timestamp = timestamp);
}

#[test]
fn paused_intake_blocks_minting() {
    let (env, contract, signers) = setup_with_signers();
    let user = Address::generate(&env);

    set_time(&env, 1_000);
    contract.pause(
        &signers.get_unchecked(0),
        &SCOPE_INTAKE,
        &3_600,
        &reason(&contract.env),
    );

    let res = contract.try_mint(&user, &1_000);
    assert_eq!(res, Err(Ok(Error::OperationPaused)));
    assert_eq!(contract.balance(&user), 0);
}

#[test]
fn holders_keep_full_control_of_existing_balances_while_everything_is_paused() {
    // The whole point of guarding only `mint`: a holder's points are their
    // property, and a halt must never reach them.
    let (env, contract, signers) = setup_with_signers();
    let user = Address::generate(&env);
    let other = Address::generate(&env);
    let spender = Address::generate(&env);
    contract.mint(&user, &1_000);

    set_time(&env, 1_000);
    contract.pause(
        &signers.get_unchecked(0),
        &ALL_SCOPES,
        &3_600,
        &reason(&contract.env),
    );
    assert_eq!(contract.paused_scopes(), ALL_SCOPES);

    contract.transfer(&user, &other, &100);
    assert_eq!(contract.balance(&other), 100);

    contract.approve(&user, &spender, &200, &10_000);
    contract.transfer_from(&spender, &user, &other, &200);
    assert_eq!(contract.balance(&other), 300);

    contract.burn(&user, &50);
    assert_eq!(contract.balance(&user), 650);

    // And the pause is genuinely still in force — these worked because they
    // are exempt, not because the halt lapsed.
    assert_eq!(contract.paused_scopes(), ALL_SCOPES);
    assert_eq!(
        contract.try_mint(&user, &1),
        Err(Ok(Error::OperationPaused))
    );
}

#[test]
fn minting_resumes_on_its_own_once_the_pause_expires() {
    let (env, contract, signers) = setup_with_signers();
    let user = Address::generate(&env);

    set_time(&env, 1_000);
    contract.pause(
        &signers.get_unchecked(0),
        &SCOPE_INTAKE,
        &3_600,
        &reason(&contract.env),
    );
    assert_eq!(
        contract.try_mint(&user, &1_000),
        Err(Ok(Error::OperationPaused))
    );

    // No unpause transaction; only the clock moves.
    set_time(&env, 4_600);

    assert_eq!(contract.paused_scopes(), 0);
    contract.mint(&user, &1_000);
    assert_eq!(contract.balance(&user), 1_000);
}

#[test]
fn a_non_signer_cannot_pause_the_token() {
    let (env, contract, _signers) = setup_with_signers();
    let outsider = Address::generate(&env);

    let res = contract.try_pause(&outsider, &ALL_SCOPES, &3_600, &reason(&contract.env));
    assert_eq!(res, Err(Ok(Error::NotASigner)));
    assert_eq!(contract.paused_scopes(), 0);
}

#[test]
fn a_pause_longer_than_the_cap_is_refused() {
    let (env, contract, signers) = setup_with_signers();
    set_time(&env, 1_000);

    let res = contract.try_pause(
        &signers.get_unchecked(0),
        &SCOPE_INTAKE,
        &(MAX_PAUSE_DURATION + 1),
        &reason(&contract.env),
    );
    assert_eq!(res, Err(Ok(Error::InvalidPauseDuration)));
    assert_eq!(contract.paused_scopes(), 0);
}

#[test]
fn any_signer_can_lift_a_pause_placed_by_another() {
    let (env, contract, signers) = setup_with_signers();
    let user = Address::generate(&env);

    set_time(&env, 1_000);
    contract.pause(
        &signers.get_unchecked(0),
        &SCOPE_INTAKE,
        &3_600,
        &reason(&contract.env),
    );
    assert_eq!(contract.unpause(&signers.get_unchecked(2), &ALL_SCOPES), 0);

    contract.mint(&user, &1_000);
    assert_eq!(contract.balance(&user), 1_000);
}

#[test]
fn scopes_the_token_has_no_entrypoints_for_are_well_formed_no_ops() {
    // An operator sweeping every contract with one mask must not have to
    // special-case which scopes a given contract implements. Settlement and
    // attestation mean nothing here, and must not bleed into `mint`.
    let (env, contract, signers) = setup_with_signers();
    let user = Address::generate(&env);

    set_time(&env, 1_000);
    contract.pause(
        &signers.get_unchecked(0),
        &(governance::SCOPE_SETTLEMENT | governance::SCOPE_ATTESTATION),
        &3_600,
        &reason(&contract.env),
    );

    assert_eq!(
        contract.paused_scopes(),
        governance::SCOPE_SETTLEMENT | governance::SCOPE_ATTESTATION
    );
    contract.mint(&user, &1_000);
    assert_eq!(contract.balance(&user), 1_000);
}

#[test]
fn an_over_long_pause_reason_is_rejected() {
    let (env, contract, signers) = setup_with_signers();
    set_time(&env, 1_000);

    let too_long = soroban_sdk::String::from_str(
        &env,
        "01234567890123456789012345678901234567890123456789012345678901234",
    );
    let res = contract.try_pause(&signers.get_unchecked(0), &SCOPE_INTAKE, &3_600, &too_long);
    assert_eq!(res, Err(Ok(Error::InvalidPauseReason)));
    assert_eq!(contract.paused_scopes(), 0);
}

#[test]
fn the_pause_reason_is_readable_from_chain_state() {
    let (env, contract, signers) = setup_with_signers();
    set_time(&env, 1_000);
    let why = soroban_sdk::String::from_str(&env, "INC-412 mint overflow");
    contract.pause(&signers.get_unchecked(0), &SCOPE_INTAKE, &3_600, &why);

    assert_eq!(contract.get_pause_state().unwrap().reason, why);
}

// ===========================================================================
// Contract events — structured contract events (#45)
// ===========================================================================
//
// We check that each state-changing entry point emits exactly one event
// and that failed operations emit none. Topic ordering and data shapes
// are documented in the README (contract-level) and pinned by the
// #[contractevent] derive macros on the event structs in lib.rs.

#[test]
fn mint_emits_one_event() {
    use soroban_sdk::testutils::Events as _;

    let (env, contract, _admin, _minter, user) = setup();
    let before = env.events().all().events().len();
    contract.mint(&user, &1_000);
    let after = env.events().all().events().len();
    assert_eq!(after - before, 1, "mint must emit exactly one event");
}

#[test]
fn transfer_emits_one_event() {
    use soroban_sdk::testutils::Events as _;

    let (env, contract, _admin, _minter, user) = setup();
    let other = Address::generate(&env);
    contract.mint(&user, &500);
    // events().all() only returns events from the most recent invocation
    contract.transfer(&user, &other, &200);
    assert_eq!(
        env.events().all().events().len(),
        1,
        "transfer must emit exactly one event"
    );
}

#[test]
fn burn_emits_one_event() {
    use soroban_sdk::testutils::Events as _;

    let (env, contract, _admin, _minter, user) = setup();
    contract.mint(&user, &400);
    // events().all() only returns events from the most recent invocation
    contract.burn(&user, &150);
    assert_eq!(
        env.events().all().events().len(),
        1,
        "burn must emit exactly one event"
    );
}

#[test]
fn set_minter_emits_one_event() {
    use soroban_sdk::testutils::Events as _;

    let (env, contract, _admin, _minter, _user) = setup();
    let new_minter = Address::generate(&env);
    contract.set_minter(&new_minter);
    assert_eq!(
        env.events().all().events().len(),
        1,
        "set_minter must emit exactly one event"
    );
}

#[test]
fn failed_mint_emits_no_event() {
    use soroban_sdk::testutils::Events as _;

    let (env, contract, _admin, _minter, user) = setup();
    let count_before = env.events().all().events().len();
    let res = contract.try_mint(&user, &0);
    assert_eq!(res, Err(Ok(Error::InvalidAmount)));
    let count_after = env.events().all().events().len();
    assert_eq!(
        count_before, count_after,
        "failed operation must emit no event"
    );
}

#[test]
fn failed_transfer_emits_no_event() {
    use soroban_sdk::testutils::Events as _;

    let (env, contract, _admin, _minter, user) = setup();
    let other = Address::generate(&env);
    contract.mint(&user, &100);
    let res = contract.try_transfer(&user, &other, &200);
    assert_eq!(res, Err(Ok(Error::InsufficientBalance)));
    assert_eq!(
        env.events().all().events().len(),
        0,
        "failed operation must emit no event"
    );
}
