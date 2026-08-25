#![cfg(test)]

use super::*;
use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};
use soroban_sdk::{token, Env, IntoVal};

use guildworkman_escrow::{EscrowContract, EscrowContractClient};
use guildworkman_loyalty_token::{
    LoyaltyToken as RealLoyaltyToken, LoyaltyTokenClient as RealLoyaltyTokenClient,
};
use guildworkman_reputation::{
    Config as ReputationConfig, ReputationContract, ReputationContractClient,
};

const CLIENT_REWARD: i128 = 50;
const WORKER_REWARD: i128 = 100;
const APPOINTMENT_AMOUNT: i128 = 10_000;

fn create_token_contract<'a>(
    env: &Env,
    admin: &Address,
) -> (Address, token::StellarAssetClient<'a>, token::Client<'a>) {
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let address = sac.address();
    (
        address.clone(),
        token::StellarAssetClient::new(env, &address),
        token::Client::new(env, &address),
    )
}

fn make_signers(env: &Env, n: u32) -> Vec<Address> {
    let mut signers = Vec::new(env);
    for _ in 0..n {
        signers.push_back(Address::generate(env));
    }
    signers
}

fn dummy_hash(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[0u8; 32])
}

#[allow(dead_code)]
struct Fixture<'a> {
    env: Env,
    router: SettlementRouterClient<'a>,
    escrow: EscrowContractClient<'a>,
    reputation: ReputationContractClient<'a>,
    loyalty: RealLoyaltyTokenClient<'a>,
    payment_token: token::Client<'a>,
    client: Address,
    worker: Address,
}

/// Deploys `escrow`, `reputation` and `loyalty-token`, wires each of them
/// through this router (`loyalty.set_minter` / `reputation.set_router`),
/// and hands back a ready-to-settle fixture. Mirrors the wiring order
/// documented in the crate-level "Deploying under partial rollout" notes.
fn setup() -> Fixture<'static> {
    let env = Env::default();
    // `settle` is deliberately permissionless at its own root (see the
    // crate docs' "Authorization model"): the client's authorization is
    // required by the *nested* `confirm_completion` / `submit_attestation`
    // calls, not by `settle` itself. Plain `mock_all_auths` enforces a
    // stricter "authorized at the root" heuristic that doesn't fit that
    // shape; this variant exists precisely for a contract that bundles
    // calls to others this way.
    env.mock_all_auths_allowing_non_root_auth();

    let client = Address::generate(&env);
    let worker = Address::generate(&env);
    let token_issuer = Address::generate(&env);
    let (_payment_token_addr, payment_token_admin, payment_token) =
        create_token_contract(&env, &token_issuer);
    payment_token_admin.mint(&client, &1_000_000);

    let escrow_id = env.register(EscrowContract, ());
    let escrow = EscrowContractClient::new(&env, &escrow_id);
    escrow.initialize(
        &Address::generate(&env),
        &governance::GovernanceInit {
            signers: make_signers(&env, 1),
            threshold: 1,
        },
    );

    let reputation_id = env.register(ReputationContract, ());
    let reputation = ReputationContractClient::new(&env, &reputation_id);
    reputation.initialize(
        &Address::generate(&env),
        &ReputationConfig {
            window: 100,
            reviewer_cap: 5,
            global_cap: 20,
            min_stake: 0,
            decay_rate_bps: 1,
            max_age_ledgers: 10_000,
        },
        &governance::GovernanceInit {
            signers: make_signers(&env, 1),
            threshold: 1,
        },
    );

    let loyalty_admin = Address::generate(&env);
    let loyalty_id = env.register(RealLoyaltyToken, ());
    let loyalty = RealLoyaltyTokenClient::new(&env, &loyalty_id);
    loyalty.initialize(
        &loyalty_admin,
        &loyalty_admin, // temporary minter, rotated to the router below
        &2,
        &String::from_str(&env, "GuildWorkman Points"),
        &String::from_str(&env, "GWP"),
        &governance::GovernanceInit {
            signers: make_signers(&env, 1),
            threshold: 1,
        },
    );

    let router_id = env.register(SettlementRouter, ());
    let router = SettlementRouterClient::new(&env, &router_id);
    router.initialize(
        &Address::generate(&env),
        &escrow_id,
        &reputation_id,
        &loyalty_id,
        &RewardConfig {
            client_reward: CLIENT_REWARD,
            worker_reward: WORKER_REWARD,
        },
        &governance::GovernanceInit {
            signers: make_signers(&env, 1),
            threshold: 1,
        },
    );

    // The two steps that actually close the pre-router holes: loyalty
    // mints only through this router, and reputation only accepts
    // attestations this router vouches for.
    loyalty.set_minter(&router_id);
    reputation.set_router(&router_id);

    Fixture {
        env,
        router,
        escrow,
        reputation,
        loyalty,
        payment_token,
        client,
        worker,
    }
}

fn fund_appointment(f: &Fixture, appointment_id: u64) {
    f.escrow.create_appointment(
        &appointment_id,
        &f.client,
        &f.worker,
        &f.payment_token.address,
        &APPOINTMENT_AMOUNT,
    );
}

// ===========================================================================
// Happy path
// ===========================================================================

#[test]
fn settle_atomically_releases_funds_attests_and_mints() {
    let f = setup();
    fund_appointment(&f, 1);

    f.router.settle(&1, &5, &dummy_hash(&f.env));

    // Funds released to the worker.
    assert_eq!(f.payment_token.balance(&f.worker), APPOINTMENT_AMOUNT);
    assert_eq!(f.payment_token.balance(&f.escrow.address), 0);

    // Attestation recorded against on-chain-verified parties.
    assert_eq!(f.reputation.get_attestation_count(&f.worker), 1);
    assert_eq!(f.reputation.get_reputation_score_x10000(&f.worker), 50_000);

    // Loyalty minted to both parties at the fixed, admin-configured amounts.
    assert_eq!(f.loyalty.balance(&f.client), CLIENT_REWARD);
    assert_eq!(f.loyalty.balance(&f.worker), WORKER_REWARD);

    assert!(f.router.is_settled(&1));
}

#[test]
fn zero_reward_skips_minting_for_that_party() {
    let f = setup();
    f.router.set_reward_config(&RewardConfig {
        client_reward: 0,
        worker_reward: WORKER_REWARD,
    });
    fund_appointment(&f, 1);

    f.router.settle(&1, &5, &dummy_hash(&f.env));

    assert_eq!(f.loyalty.balance(&f.client), 0);
    assert_eq!(f.loyalty.balance(&f.worker), WORKER_REWARD);
}

// ===========================================================================
// Idempotency
// ===========================================================================

#[test]
fn settle_is_idempotent_no_double_release_or_mint() {
    let f = setup();
    fund_appointment(&f, 1);
    f.router.settle(&1, &5, &dummy_hash(&f.env));

    let res = f.router.try_settle(&1, &5, &dummy_hash(&f.env));
    assert_eq!(res, Err(Ok(Error::AlreadySettled)));

    assert_eq!(f.payment_token.balance(&f.worker), APPOINTMENT_AMOUNT);
    assert_eq!(f.loyalty.balance(&f.worker), WORKER_REWARD);
    assert_eq!(f.reputation.get_attestation_count(&f.worker), 1);
}

// ===========================================================================
// Rejected settlement states
// ===========================================================================

#[test]
fn settle_rejects_unknown_appointment() {
    let f = setup();
    let res = f.router.try_settle(&1, &5, &dummy_hash(&f.env));
    assert!(res.is_err());
}

#[test]
fn settle_rejects_appointment_already_completed_outside_the_router() {
    let f = setup();
    fund_appointment(&f, 1);
    // A client bypassing the router entirely and confirming directly.
    f.escrow.confirm_completion(&1);

    let res = f.router.try_settle(&1, &5, &dummy_hash(&f.env));
    assert_eq!(res, Err(Ok(Error::AppointmentNotFunded)));

    // The bypassed appointment paid the worker, but no attestation or
    // loyalty mint was ever produced for it -- exactly the gap this
    // router exists to close for callers who *do* go through `settle`.
    assert_eq!(f.reputation.get_attestation_count(&f.worker), 0);
    assert_eq!(f.loyalty.balance(&f.client), 0);
    assert_eq!(f.loyalty.balance(&f.worker), 0);
}

#[test]
fn settle_rejects_cancelled_appointment() {
    let f = setup();
    fund_appointment(&f, 1);
    f.escrow.cancel_appointment(&1);

    let res = f.router.try_settle(&1, &5, &dummy_hash(&f.env));
    assert_eq!(res, Err(Ok(Error::AppointmentNotFunded)));
}

#[test]
fn settle_rejects_disputed_appointment() {
    let f = setup();
    fund_appointment(&f, 1);
    f.escrow.raise_dispute(&1, &f.client);

    let res = f.router.try_settle(&1, &5, &dummy_hash(&f.env));
    assert_eq!(res, Err(Ok(Error::AppointmentNotFunded)));
}

// ===========================================================================
// Atomicity across sub-contract failures
// ===========================================================================

#[test]
fn settle_reverts_entirely_when_reputation_is_paused() {
    let f = setup();
    fund_appointment(&f, 1);

    let reputation_signers = f.reputation.get_signers();
    f.reputation.pause(
        &reputation_signers.get(0).unwrap(),
        &governance::SCOPE_ATTESTATION,
        &3600,
        &String::from_str(&f.env, "incident"),
    );

    let res = f.router.try_settle(&1, &5, &dummy_hash(&f.env));
    assert!(res.is_err());

    // Nothing committed: escrow's confirm_completion ran earlier in the
    // same invocation, but the whole transaction unwinds on the later
    // panic, so funds are still exactly where they started.
    assert!(!f.router.is_settled(&1));
    assert_eq!(
        f.payment_token.balance(&f.escrow.address),
        APPOINTMENT_AMOUNT
    );
    assert_eq!(f.payment_token.balance(&f.worker), 0);
    assert_eq!(f.loyalty.balance(&f.client), 0);
}

#[test]
fn settle_reverts_entirely_when_loyalty_token_is_paused() {
    let f = setup();
    fund_appointment(&f, 1);

    let loyalty_signers = f.loyalty.get_signers();
    f.loyalty.pause(
        &loyalty_signers.get(0).unwrap(),
        &governance::SCOPE_INTAKE,
        &3600,
        &String::from_str(&f.env, "incident"),
    );

    let res = f.router.try_settle(&1, &5, &dummy_hash(&f.env));
    assert!(res.is_err());

    assert!(!f.router.is_settled(&1));
    assert_eq!(
        f.payment_token.balance(&f.escrow.address),
        APPOINTMENT_AMOUNT
    );
    assert_eq!(f.reputation.get_attestation_count(&f.worker), 0);
}

#[test]
fn settle_fails_while_router_settlement_scope_is_paused() {
    let f = setup();
    fund_appointment(&f, 1);

    let signers = f.router.get_signers();
    f.router.pause(
        &signers.get(0).unwrap(),
        &SCOPE_SETTLEMENT,
        &3600,
        &String::from_str(&f.env, "incident"),
    );

    let res = f.router.try_settle(&1, &5, &dummy_hash(&f.env));
    assert_eq!(res, Err(Ok(Error::OperationPaused)));
}

// ===========================================================================
// Reward configuration
// ===========================================================================

#[test]
fn reward_config_rejects_negative_amounts() {
    let f = setup();
    let res = f.router.try_set_reward_config(&RewardConfig {
        client_reward: -1,
        worker_reward: WORKER_REWARD,
    });
    assert_eq!(res, Err(Ok(Error::InvalidRewardConfig)));
}

#[test]
fn initialize_rejects_negative_reward_config() {
    let env = Env::default();
    env.mock_all_auths();
    let router_id = env.register(SettlementRouter, ());
    let router = SettlementRouterClient::new(&env, &router_id);

    let res = router.try_initialize(
        &Address::generate(&env),
        &Address::generate(&env),
        &Address::generate(&env),
        &Address::generate(&env),
        &RewardConfig {
            client_reward: 10,
            worker_reward: -1,
        },
        &governance::GovernanceInit {
            signers: make_signers(&env, 1),
            threshold: 1,
        },
    );
    assert_eq!(res, Err(Ok(Error::InvalidRewardConfig)));
}

// ===========================================================================
// Reputation's router gate, exercised directly
// ===========================================================================

#[test]
fn direct_reputation_call_without_router_auth_is_rejected() {
    let f = setup();
    // `reputation.set_router(&router_id)` already ran in `setup`. A caller
    // that is not the router contract itself has no way to satisfy
    // `require_auth` for the router's address -- only the client's own
    // authorization is mocked here, deliberately omitting the router's.
    let hash = dummy_hash(&f.env);
    let args: Vec<soroban_sdk::Val> =
        (1u64, f.client.clone(), f.worker.clone(), 5u32, hash.clone()).into_val(&f.env);
    f.env.mock_auths(&[MockAuth {
        address: &f.client,
        invoke: &MockAuthInvoke {
            contract: &f.reputation.address,
            fn_name: "submit_attestation",
            args,
            sub_invokes: &[],
        },
    }]);

    let res = f
        .reputation
        .try_submit_attestation(&1, &f.client, &f.worker, &5, &hash);
    assert!(res.is_err());
}

#[test]
fn reputation_without_router_configured_keeps_legacy_behavior() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let client = Address::generate(&env);
    let worker = Address::generate(&env);

    let reputation_id = env.register(ReputationContract, ());
    let reputation = ReputationContractClient::new(&env, &reputation_id);
    reputation.initialize(
        &admin,
        &ReputationConfig {
            window: 100,
            reviewer_cap: 5,
            global_cap: 20,
            min_stake: 0,
            decay_rate_bps: 1,
            max_age_ledgers: 10_000,
        },
        &governance::GovernanceInit {
            signers: make_signers(&env, 1),
            threshold: 1,
        },
    );

    assert_eq!(reputation.get_router(), None);
    reputation.submit_attestation(&1, &client, &worker, &5, &dummy_hash(&env));
    assert_eq!(reputation.get_attestation_count(&worker), 1);
}
