#![cfg(test)]

use super::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::testutils::Ledger;
use soroban_sdk::testutils::{MockAuth, MockAuthInvoke};
use soroban_sdk::{BytesN, Env, IntoVal, Val};

fn default_config() -> Config {
    Config {
        window: 100,
        reviewer_cap: 5,
        global_cap: 20,
        min_stake: 0,
        decay_rate_bps: 1,
        max_age_ledgers: 10_000,
    }
}

fn single_signer(env: &Env) -> (soroban_sdk::Vec<Address>, Address) {
    let signer = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(signer.clone());
    (signers, signer)
}

fn gov_init(signers: soroban_sdk::Vec<Address>, threshold: u32) -> governance::GovernanceInit {
    governance::GovernanceInit { signers, threshold }
}

fn setup() -> (Env, ReputationContractClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let client = Address::generate(&env);
    let worker = Address::generate(&env);

    let contract_id = env.register(ReputationContract, ());
    let contract = ReputationContractClient::new(&env, &contract_id);

    let (signers, _signer) = single_signer(&env);
    contract.initialize(
        &Address::generate(&env),
        &default_config(),
        &gov_init(signers, 1),
    );

    (env, contract, client, worker)
}

fn setup_with_admin() -> (
    Env,
    ReputationContractClient<'static>,
    Address,
    Address,
    Address,
) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let client = Address::generate(&env);
    let worker = Address::generate(&env);

    let contract_id = env.register(ReputationContract, ());
    let contract = ReputationContractClient::new(&env, &contract_id);

    let (signers, _signer) = single_signer(&env);
    contract.initialize(&admin, &default_config(), &gov_init(signers, 1));

    (env, contract, admin, client, worker)
}

/// Same as `setup_with_admin`, but also hands back the governance signer
/// and the raw wasm hash used across the upgrade-governance tests.
fn setup_with_governance() -> (
    Env,
    ReputationContractClient<'static>,
    Address,
    soroban_sdk::Vec<Address>,
) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let contract_id = env.register(ReputationContract, ());
    let contract = ReputationContractClient::new(&env, &contract_id);

    let mut signers = soroban_sdk::Vec::new(&env);
    signers.push_back(Address::generate(&env));
    signers.push_back(Address::generate(&env));
    signers.push_back(Address::generate(&env));

    contract.initialize(&admin, &default_config(), &gov_init(signers.clone(), 2));

    (env, contract, admin, signers)
}

fn dummy_hash(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[0u8; 32])
}

fn other_hash(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[7u8; 32])
}

fn set_ledger(env: &Env, seq: u32) {
    env.ledger().with_mut(|l| l.sequence_number = seq);
}

// ===========================================================================
// Success paths
// ===========================================================================

#[test]
fn submit_attestation_updates_accumulator() {
    let (env, contract, client, worker) = setup();

    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    contract.submit_attestation(&2, &client, &worker, &3, &dummy_hash(&env));

    let acc = contract.get_weighted_score(&worker);
    assert_eq!(acc.count, 2);
    assert_eq!(contract.get_attestation_count(&worker), 2);

    // Both at same ledger, no decay. Stake=0 -> treated as 1.
    // weighted_sum = 5*1*10000 + 3*1*10000 = 80000
    // weight_sum = 1*10000 + 1*10000 = 20000
    // score = (80000 * 10000) / 20000 = 40000 = 4.0000
    let score = contract.get_reputation_score_x10000(&worker);
    assert_eq!(score, 40000);
}

#[test]
fn get_reputation_score_x10000_basic() {
    let (env, contract, client, worker) = setup();

    contract.submit_attestation(&1, &client, &worker, &4, &dummy_hash(&env));

    // Single attestation, rating=4, stake=0->1, decay=10000
    // weighted_sum = 4 * 1 * 10000 = 40000
    // weight_sum = 1 * 10000 = 10000
    // score = (40000 * 10000) / 10000 = 40000 = 4.0000
    let score = contract.get_reputation_score_x10000(&worker);
    assert_eq!(score, 40000);
}

#[test]
fn stake_weighting_favors_higher_stake() {
    let (env, contract, _admin, client_a, worker) = setup_with_admin();
    let client_b = Address::generate(&env);

    // Client A: stake=1000, rating=5
    // Client B: stake=100, rating=4
    contract.set_stake(&client_a, &1000);
    contract.set_stake(&client_b, &100);

    contract.submit_attestation(&1, &client_a, &worker, &5, &dummy_hash(&env));
    contract.submit_attestation(&2, &client_b, &worker, &4, &dummy_hash(&env));

    // weighted_sum = 5*1000*10000 + 4*100*10000 = 50000000 + 4000000 = 54000000
    // weight_sum = 1000*10000 + 100*10000 = 10000000 + 1000000 = 11000000
    // score = (54000000 * 10000) / 11000000 = 49090 (approx 4.9090)
    let score = contract.get_reputation_score_x10000(&worker);
    assert!(
        score > 45000,
        "score should be closer to 5 than 4, got {score}"
    );
    assert!(score < 50000, "score should be less than 5, got {score}");
}

#[test]
fn admin_set_stake_works() {
    let (env, contract, _admin, client, worker) = setup_with_admin();

    contract.set_stake(&client, &500);
    assert_eq!(contract.get_stake(&client), 500);

    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    let att = contract.get_attestation(&worker, &0).unwrap();
    assert_eq!(att.stake_at_time, 500);
}

// ===========================================================================
// Failure paths
// ===========================================================================

#[test]
fn cannot_review_same_appointment_twice() {
    let (env, contract, client, worker) = setup();

    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    let result = contract.try_submit_attestation(&1, &client, &worker, &4, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::AlreadyReviewed)));
}

#[test]
fn rejects_out_of_range_rating() {
    let (env, contract, client, worker) = setup();

    let result = contract.try_submit_attestation(&1, &client, &worker, &6, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::InvalidRating)));

    let result = contract.try_submit_attestation(&1, &client, &worker, &0, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::InvalidRating)));
}

#[test]
fn self_dealing_rejected() {
    let (env, contract, client, _worker) = setup();

    let result = contract.try_submit_attestation(&1, &client, &client, &5, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::SelfDealing)));
}

#[test]
fn insufficient_stake_rejected() {
    let (env, contract, _admin, client, worker) = setup_with_admin();

    // Update config to require min_stake
    let mut config = default_config();
    config.min_stake = 100;
    contract.update_config(&config);

    // Client has no stake
    let result = contract.try_submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::InsufficientStake)));

    // Set stake below minimum
    contract.set_stake(&client, &50);
    let result = contract.try_submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::InsufficientStake)));

    // Set stake at minimum — should succeed
    contract.set_stake(&client, &100);
    let result = contract.try_submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert!(result.is_ok());
}

#[test]
fn reviewer_rate_limited() {
    let (env, contract, client, _worker) = setup();

    // reviewer_cap is 5, submit 5
    for i in 1..=5 {
        let w = Address::generate(&env);
        contract.submit_attestation(&(i as u64), &client, &w, &4, &dummy_hash(&env));
    }

    // 6th should fail
    let w = Address::generate(&env);
    let result = contract.try_submit_attestation(&6, &client, &w, &4, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::ReviewerRateLimited)));
}

#[test]
fn global_rate_limited() {
    let (env, contract, _admin, _client, _worker) = setup_with_admin();

    // Set low global cap
    let mut config = default_config();
    config.global_cap = 3;
    config.reviewer_cap = 100;
    contract.update_config(&config);

    // 3 different reviewers fill the global cap
    for i in 1..=3 {
        let c = Address::generate(&env);
        let w = Address::generate(&env);
        contract.submit_attestation(&(i as u64), &c, &w, &4, &dummy_hash(&env));
    }

    // 4th should fail
    let c = Address::generate(&env);
    let w = Address::generate(&env);
    let result = contract.try_submit_attestation(&4, &c, &w, &4, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::GlobalRateLimited)));
}

#[test]
fn double_initialize_fails() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let contract_id = env.register(ReputationContract, ());
    let contract = ReputationContractClient::new(&env, &contract_id);

    let (signers, _signer) = single_signer(&env);
    contract.initialize(&admin, &default_config(), &gov_init(signers.clone(), 1));
    let result = contract.try_initialize(&admin, &default_config(), &gov_init(signers, 1));
    assert_eq!(result, Err(Ok(Error::AlreadyInitialized)));
}

#[test]
fn no_reviews_error() {
    let (_env, contract, _client, worker) = setup();

    let result = contract.try_get_reputation_score_x10000(&worker);
    assert_eq!(result, Err(Ok(Error::NoReviews)));
}

#[test]
fn invalid_config_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let contract_id = env.register(ReputationContract, ());
    let contract = ReputationContractClient::new(&env, &contract_id);
    let (signers, _signer) = single_signer(&env);

    let mut config = default_config();
    config.window = 0;
    let result = contract.try_initialize(&admin, &config, &gov_init(signers.clone(), 1));
    assert_eq!(result, Err(Ok(Error::InvalidConfig)));

    config = default_config();
    config.reviewer_cap = 0;
    let result = contract.try_initialize(&admin, &config, &gov_init(signers.clone(), 1));
    assert_eq!(result, Err(Ok(Error::InvalidConfig)));

    config = default_config();
    config.decay_rate_bps = 0;
    let result = contract.try_initialize(&admin, &config, &gov_init(signers, 1));
    assert_eq!(result, Err(Ok(Error::InvalidConfig)));
}

// ===========================================================================
// Time decay tests
// ===========================================================================

#[test]
fn time_decay_reduces_old_attestation_weight() {
    let (env, contract, client, worker) = setup();

    // Submit at ledger 1000
    set_ledger(&env, 1000);
    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));

    // The accumulator was computed at submission time with age=0
    let acc = contract.get_weighted_score(&worker);
    assert_eq!(acc.weighted_sum, 50000);
    assert_eq!(acc.weight_sum, 10000);

    // After recalculation at ledger 6000:
    set_ledger(&env, 6000);
    contract.recalculate_score(&worker);
    let acc = contract.get_weighted_score(&worker);
    // age = 6000 - 1000 = 5000, decay = 10000 - 5000*1 = 5000
    // weighted_sum = 5 * 1 * 5000 = 25000
    // weight_sum = 1 * 5000 = 5000
    assert_eq!(acc.weighted_sum, 25000);
    assert_eq!(acc.weight_sum, 5000);

    // Score should still be 5.0 (single attestation, just decayed equally)
    let score = contract.get_reputation_score_x10000(&worker);
    assert_eq!(score, 50000);
}

#[test]
fn fully_decayed_attestation_yields_no_reviews_after_recalculate() {
    let (env, contract, client, worker) = setup();

    // Submit at ledger 100
    set_ledger(&env, 100);
    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));

    // Advance past max_age_ledgers (10000)
    set_ledger(&env, 10200);

    // Recalculate — attestation fully decayed
    contract.recalculate_score(&worker);
    let acc = contract.get_weighted_score(&worker);
    assert_eq!(acc.weight_sum, 0);

    // Score query should return NoReviews
    let result = contract.try_get_reputation_score_x10000(&worker);
    assert_eq!(result, Err(Ok(Error::NoReviews)));
}

#[test]
fn decay_rate_boundary_immediate_decay() {
    let (env, contract, _admin, client, worker) = setup_with_admin();

    // Set decay_rate_bps = 10000, max_age = 2
    let mut config = default_config();
    config.decay_rate_bps = 10_000;
    config.max_age_ledgers = 2;
    contract.update_config(&config);

    set_ledger(&env, 1000);
    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));

    // At ledger 1001 (age=1): decay = 10000 - 1*10000 = 0
    set_ledger(&env, 1001);
    contract.recalculate_score(&worker);
    let acc = contract.get_weighted_score(&worker);
    assert_eq!(acc.weight_sum, 0);
}

// ===========================================================================
// Config update test
// ===========================================================================

#[test]
fn config_update_affects_new_submissions() {
    let (env, contract, _admin, client, worker) = setup_with_admin();

    // Submit with original config (decay_rate_bps=1)
    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));

    // Update config to aggressive decay
    let mut config = default_config();
    config.decay_rate_bps = 100;
    config.max_age_ledgers = 50;
    contract.update_config(&config);

    // Advance 30 ledgers and submit another
    set_ledger(&env, 30);
    let client2 = Address::generate(&env);
    contract.submit_attestation(&2, &client2, &worker, &3, &dummy_hash(&env));

    // Recalculate at ledger 30
    contract.recalculate_score(&worker);
    let acc = contract.get_weighted_score(&worker);

    // First attestation: age=30, decay = 10000 - 30*100 = 7000
    // contribution = 5 * 1 * 7000 = 35000
    // Second attestation: age=0, decay = 10000
    // contribution = 3 * 1 * 10000 = 30000
    assert_eq!(acc.weighted_sum, 65000);
    assert_eq!(acc.weight_sum, 17000);
}

// ===========================================================================
// Recalculate score test
// ===========================================================================

#[test]
fn recalculate_score_fixes_drift() {
    let (env, contract, _admin, client, worker) = setup_with_admin();

    // Submit attestation at ledger 1000
    set_ledger(&env, 1000);
    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));

    // Without recalculate, accumulator reflects submission-time values
    let acc = contract.get_weighted_score(&worker);
    assert_eq!(acc.weighted_sum, 50000);

    // Advance 2000 ledgers and recalculate
    set_ledger(&env, 3000);
    contract.recalculate_score(&worker);
    let acc = contract.get_weighted_score(&worker);

    // age=2000, decay = 10000 - 2000*1 = 8000
    // weighted_sum = 5 * 1 * 8000 = 40000
    // weight_sum = 1 * 8000 = 8000
    assert_eq!(acc.weighted_sum, 40000);
    assert_eq!(acc.weight_sum, 8000);
    assert_eq!(acc.count, 1);
}

// ===========================================================================
// Legacy compatibility
// ===========================================================================

#[test]
fn zero_stake_legacy_treated_as_one() {
    let (env, contract, client, worker) = setup();

    // Submit without setting stake (stake=0, treated as 1)
    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));

    let acc = contract.get_weighted_score(&worker);
    // effective_stake=1, decay=10000
    // weighted_sum = 5 * 1 * 10000 = 50000
    // weight_sum = 1 * 10000 = 10000
    assert_eq!(acc.weighted_sum, 50000);
    assert_eq!(acc.weight_sum, 10000);
}

// ===========================================================================
// Rate limit window reset
// ===========================================================================

#[test]
fn fresh_window_allows_more_submissions() {
    let (env, contract, client, _worker) = setup();

    // reviewer_cap is 5, fill it
    for i in 1..=5 {
        let w = Address::generate(&env);
        contract.submit_attestation(&(i as u64), &client, &w, &4, &dummy_hash(&env));
    }

    // Should be rate limited
    let w = Address::generate(&env);
    let result = contract.try_submit_attestation(&6, &client, &w, &4, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::ReviewerRateLimited)));

    // Advance to next window (window=100)
    set_ledger(&env, 100);

    // Should work now
    let w = Address::generate(&env);
    let result = contract.try_submit_attestation(&7, &client, &w, &4, &dummy_hash(&env));
    assert!(result.is_ok());
}

// ===========================================================================
// View functions
// ===========================================================================

#[test]
fn get_attestation_returns_correct_data() {
    let (env, contract, client, worker) = setup();

    let hash = BytesN::from_array(&env, &[42u8; 32]);
    contract.submit_attestation(&1, &client, &worker, &3, &hash);

    let att = contract.get_attestation(&worker, &0).unwrap();
    assert_eq!(att.client, client);
    assert_eq!(att.worker, worker);
    assert_eq!(att.appointment_id, 1);
    assert_eq!(att.rating, 3);
    assert_eq!(att.attestation_hash, hash);
}

#[test]
fn get_attestation_count_tracks_correctly() {
    let (env, contract, client, worker) = setup();

    assert_eq!(contract.get_attestation_count(&worker), 0);

    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(contract.get_attestation_count(&worker), 1);

    let client2 = Address::generate(&env);
    contract.submit_attestation(&2, &client2, &worker, &4, &dummy_hash(&env));
    assert_eq!(contract.get_attestation_count(&worker), 2);
}

#[test]
fn get_admin_returns_admin() {
    let (_env, contract, admin, _client, _worker) = setup_with_admin();

    assert_eq!(contract.get_admin(), admin);
}

#[test]
fn get_config_returns_config() {
    let (_env, contract, _admin, _client, _worker) = setup_with_admin();

    let config = contract.get_config();
    assert_eq!(config.window, 100);
    assert_eq!(config.reviewer_cap, 5);
    assert_eq!(config.global_cap, 20);
}

// ===========================================================================
// Upgrade governance
//
// These stop one approval short of the configured threshold everywhere,
// deliberately. Crossing it calls env.deployer().update_current_contract_wasm,
// which requires the hash to correspond to Wasm actually uploaded on the
// ledger — there's no such artifact available inside a plain `cargo test`
// run. What's tested here is everything up to that point: signer checks,
// threshold math, replay/expiry protection, and the migration version gate,
// all exercised through the real contract entrypoints rather than by calling
// guildworkman-governance-guard directly (that crate's own 20 tests already
// cover the underlying logic in isolation).
// ===========================================================================

#[test]
fn initialize_stores_governance_config() {
    let (_env, contract, _admin, signers) = setup_with_governance();

    assert_eq!(contract.get_signers(), signers);
    assert_eq!(contract.get_upgrade_threshold(), 2);
    assert_eq!(contract.get_storage_version(), 1);
}

#[test]
fn propose_upgrade_by_non_signer_fails() {
    let (env, contract, _admin, _signers) = setup_with_governance();
    let outsider = Address::generate(&env);

    let result = contract.try_propose_upgrade(&outsider, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::NotASigner)));
}

#[test]
fn propose_upgrade_below_threshold_is_not_ready() {
    let (env, contract, _admin, signers) = setup_with_governance();

    let ready = contract.propose_upgrade(&signers.get_unchecked(0), &dummy_hash(&env));
    assert!(!ready);

    let pending = contract.get_pending_upgrade().unwrap();
    assert_eq!(pending.wasm_hash, dummy_hash(&env));
    assert_eq!(pending.approvals.len(), 1);
}

#[test]
fn approve_upgrade_by_non_signer_fails() {
    let (env, contract, _admin, signers) = setup_with_governance();
    contract.propose_upgrade(&signers.get_unchecked(0), &dummy_hash(&env));

    let outsider = Address::generate(&env);
    let result = contract.try_approve_upgrade(&outsider, &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::NotASigner)));
}

#[test]
fn approve_upgrade_with_wrong_hash_fails() {
    let (env, contract, _admin, signers) = setup_with_governance();
    contract.propose_upgrade(&signers.get_unchecked(0), &dummy_hash(&env));

    let result = contract.try_approve_upgrade(&signers.get_unchecked(1), &other_hash(&env));
    assert_eq!(result, Err(Ok(Error::HashMismatch)));
}

#[test]
fn same_signer_approving_twice_fails() {
    let (env, contract, _admin, signers) = setup_with_governance();
    contract.propose_upgrade(&signers.get_unchecked(0), &dummy_hash(&env));

    let result = contract.try_approve_upgrade(&signers.get_unchecked(0), &dummy_hash(&env));
    assert_eq!(result, Err(Ok(Error::AlreadyApproved)));
}

#[test]
fn cancel_upgrade_clears_the_pending_proposal() {
    let (env, contract, _admin, signers) = setup_with_governance();
    contract.propose_upgrade(&signers.get_unchecked(0), &dummy_hash(&env));
    assert!(contract.get_pending_upgrade().is_some());

    // The third signer, who never approved, can still cancel.
    contract.cancel_upgrade(&signers.get_unchecked(2));
    assert!(contract.get_pending_upgrade().is_none());
}

#[test]
fn cancel_upgrade_by_non_signer_fails() {
    let (env, contract, _admin, signers) = setup_with_governance();
    contract.propose_upgrade(&signers.get_unchecked(0), &dummy_hash(&env));

    let outsider = Address::generate(&env);
    let result = contract.try_cancel_upgrade(&outsider);
    assert_eq!(result, Err(Ok(Error::NotASigner)));
}

#[test]
fn migrate_by_non_signer_fails() {
    let (env, contract, _admin, _signers) = setup_with_governance();
    let outsider = Address::generate(&env);

    let result = contract.try_migrate(&outsider);
    assert_eq!(result, Err(Ok(Error::NotASigner)));
}

#[test]
fn migrate_with_nothing_to_migrate_fails() {
    let (_env, contract, _admin, signers) = setup_with_governance();

    // Fresh init already sits at CURRENT_STORAGE_VERSION — there's no v2
    // shipped yet, so there's nothing for a signer to migrate.
    let result = contract.try_migrate(&signers.get_unchecked(0));
    assert_eq!(result, Err(Ok(Error::NothingToMigrate)));
}

// ===========================================================================
// Emergency circuit breaker
// ===========================================================================
//
// Scope arithmetic, expiry and authorization are unit-tested in
// `guildworkman-governance-guard`. These cover the wiring specific to this
// contract: `submit_attestation` is halted by `SCOPE_ATTESTATION` and nothing
// else in this contract is halted by anything.

/// Reason string for tests that don't exercise the field itself. Kept
/// non-empty so the round-trip through storage is actually covered by every
/// pause test rather than only the ones that look at it.
fn reason(env: &Env) -> soroban_sdk::String {
    soroban_sdk::String::from_str(env, "INC-000 test")
}

fn set_time(env: &Env, timestamp: u64) {
    env.ledger().with_mut(|l| l.timestamp = timestamp);
}

#[test]
fn paused_attestations_are_rejected_and_write_no_state() {
    let (env, contract, _admin, signers) = setup_with_governance();
    let client = Address::generate(&env);
    let worker = Address::generate(&env);

    set_time(&env, 1_000);
    contract.pause(
        &signers.get_unchecked(0),
        &SCOPE_ATTESTATION,
        &3_600,
        &reason(&contract.env),
    );

    let res = contract.try_submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(res, Err(Ok(Error::OperationPaused)));

    // Nothing was recorded, so the same appointment is still reviewable
    // once the halt lifts — a rejected call must not burn the one review.
    assert_eq!(contract.get_attestation_count(&worker), 0);
    contract.unpause(&signers.get_unchecked(0), &SCOPE_ATTESTATION);
    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(contract.get_attestation_count(&worker), 1);
}

#[test]
fn scores_stay_readable_while_attestations_are_paused() {
    // Halting writes must not blind consumers to the scores that already
    // exist — including the bad ones that prompted the halt.
    let (env, contract, _admin, signers) = setup_with_governance();
    let client = Address::generate(&env);
    let worker = Address::generate(&env);
    contract.submit_attestation(&1, &client, &worker, &4, &dummy_hash(&env));

    set_time(&env, 1_000);
    contract.pause(
        &signers.get_unchecked(0),
        &ALL_SCOPES,
        &3_600,
        &reason(&contract.env),
    );

    assert_eq!(contract.get_reputation_score_x10000(&worker), 40_000);
    assert_eq!(contract.get_attestation_count(&worker), 1);
    assert!(contract.get_attestation(&worker, &0).is_some());
}

#[test]
fn attestations_resume_on_their_own_once_the_pause_expires() {
    let (env, contract, _admin, signers) = setup_with_governance();
    let client = Address::generate(&env);
    let worker = Address::generate(&env);

    set_time(&env, 1_000);
    contract.pause(
        &signers.get_unchecked(0),
        &SCOPE_ATTESTATION,
        &3_600,
        &reason(&contract.env),
    );
    let res = contract.try_submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(res, Err(Ok(Error::OperationPaused)));

    // No unpause transaction; only the clock moves.
    set_time(&env, 4_600);

    assert_eq!(contract.paused_scopes(), 0);
    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(contract.get_attestation_count(&worker), 1);
}

#[test]
fn a_scope_this_contract_has_no_entrypoints_for_is_a_well_formed_no_op() {
    // Operators broadcast the same mask to every contract during an
    // incident; a scope that means nothing here must not error and must not
    // accidentally halt attestations either.
    let (env, contract, _admin, signers) = setup_with_governance();
    let client = Address::generate(&env);
    let worker = Address::generate(&env);

    set_time(&env, 1_000);
    contract.pause(
        &signers.get_unchecked(0),
        &(governance::SCOPE_INTAKE | governance::SCOPE_SETTLEMENT),
        &3_600,
        &reason(&contract.env),
    );

    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(contract.get_attestation_count(&worker), 1);
}

#[test]
fn a_non_signer_cannot_pause_reputation() {
    let (env, contract, _admin, _signers) = setup_with_governance();
    let outsider = Address::generate(&env);

    let res = contract.try_pause(&outsider, &ALL_SCOPES, &3_600, &reason(&contract.env));
    assert_eq!(res, Err(Ok(Error::NotASigner)));
    assert_eq!(contract.paused_scopes(), 0);
}

#[test]
fn the_config_admin_is_not_a_pause_authority() {
    // `admin` owns `update_config`/`set_stake`; the breaker answers to the
    // governance signer set instead.
    let (_env, contract, admin, _signers) = setup_with_governance();
    let res = contract.try_pause(&admin, &ALL_SCOPES, &3_600, &reason(&contract.env));
    assert_eq!(res, Err(Ok(Error::NotASigner)));
}

#[test]
fn a_pause_longer_than_the_cap_is_refused() {
    let (env, contract, _admin, signers) = setup_with_governance();
    set_time(&env, 1_000);

    let res = contract.try_pause(
        &signers.get_unchecked(0),
        &ALL_SCOPES,
        &(MAX_PAUSE_DURATION + 1),
        &reason(&contract.env),
    );
    assert_eq!(res, Err(Ok(Error::InvalidPauseDuration)));
    assert_eq!(contract.paused_scopes(), 0);
}

#[test]
fn unpause_with_nothing_halted_is_rejected() {
    let (_env, contract, _admin, signers) = setup_with_governance();
    let res = contract.try_unpause(&signers.get_unchecked(0), &ALL_SCOPES);
    assert_eq!(res, Err(Ok(Error::NotPaused)));
}

#[test]
fn pause_views_report_the_active_window() {
    let (env, contract, _admin, signers) = setup_with_governance();
    let signer = signers.get_unchecked(2);
    set_time(&env, 1_000);
    contract.pause(&signer, &SCOPE_ATTESTATION, &7_200, &reason(&contract.env));

    let state = contract.get_pause_state().unwrap();
    assert_eq!(state.scopes, SCOPE_ATTESTATION);
    assert_eq!(state.paused_by, signer);
    assert_eq!(state.expires_at, 8_200);
    assert!(contract.is_paused(&SCOPE_ATTESTATION));
}

// ===========================================================================
// Settlement router gating
// ===========================================================================

#[test]
fn router_defaults_to_unset() {
    let (_env, contract, _client, _worker) = setup();
    assert_eq!(contract.get_router(), None);
}

#[test]
fn set_router_then_get_router_reflects_it() {
    let (env, contract, _client, _worker) = setup();
    let router = Address::generate(&env);
    contract.set_router(&router);
    assert_eq!(contract.get_router(), Some(router));
}

#[test]
fn submit_attestation_without_router_configured_keeps_legacy_behavior() {
    // Regression: a deployment that never wires a router in behaves
    // exactly as before this feature existed.
    let (env, contract, client, worker) = setup();
    contract.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(contract.get_attestation_count(&worker), 1);
}

#[test]
fn submit_attestation_direct_call_fails_once_router_is_configured() {
    let (env, contract, client, worker) = setup();
    let router = Address::generate(&env);
    contract.set_router(&router);

    // Only the client's own authorization is mocked -- a direct caller
    // that is not the router contract itself has no way to satisfy
    // `require_auth` for the router's address, since a contract address
    // can only authorize by directly executing the call.
    let hash = dummy_hash(&env);
    let args: soroban_sdk::Vec<Val> =
        (1u64, client.clone(), worker.clone(), 5u32, hash.clone()).into_val(&env);
    env.mock_auths(&[MockAuth {
        address: &client,
        invoke: &MockAuthInvoke {
            contract: &contract.address,
            fn_name: "submit_attestation",
            args,
            sub_invokes: &[],
        },
    }]);

    let res = contract.try_submit_attestation(&1, &client, &worker, &5, &hash);
    assert!(res.is_err());
}
