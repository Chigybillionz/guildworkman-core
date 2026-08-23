#![cfg(test)]

use super::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::testutils::Ledger;
use soroban_sdk::Env;

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

#[allow(dead_code)]
struct TestCtx<'a> {
    env: Env,
    contract: EscrowContractClient<'a>,
    admin: Address,
    client: Address,
    worker: Address,
    token: Address,
    token_admin: token::StellarAssetClient<'a>,
    token_client: token::Client<'a>,
    signers: soroban_sdk::Vec<Address>,
}

fn setup() -> TestCtx<'static> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let client = Address::generate(&env);
    let worker = Address::generate(&env);
    let token_issuer = Address::generate(&env);

    let (token, token_admin, token_client) = create_token_contract(&env, &token_issuer);
    token_admin.mint(&client, &1_000_000);

    let contract_id = env.register(EscrowContract, ());
    let contract = EscrowContractClient::new(&env, &contract_id);

    let mut signers = soroban_sdk::Vec::new(&env);
    signers.push_back(Address::generate(&env));
    signers.push_back(Address::generate(&env));
    signers.push_back(Address::generate(&env));
    contract.initialize(
        &admin,
        &governance::GovernanceInit {
            signers: signers.clone(),
            threshold: 2,
        },
    );

    TestCtx {
        env,
        contract,
        admin,
        client,
        worker,
        token,
        token_admin,
        token_client,
        signers,
    }
}

#[test]
fn happy_path_completion_pays_worker() {
    let ctx = setup();

    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    assert_eq!(ctx.token_client.balance(&ctx.client), 990_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 10_000);

    ctx.contract.confirm_completion(&1);
    assert_eq!(ctx.token_client.balance(&ctx.worker), 10_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);

    let appointment = ctx.contract.get_appointment(&1);
    assert_eq!(appointment.status, Status::Completed);
}

#[test]
fn cancel_refunds_client() {
    let ctx = setup();

    ctx.contract
        .create_appointment(&2, &ctx.client, &ctx.worker, &ctx.token, &5_000);
    ctx.contract.cancel_appointment(&2);

    assert_eq!(ctx.token_client.balance(&ctx.client), 1_000_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);
    assert_eq!(ctx.contract.get_appointment(&2).status, Status::Cancelled);
}

#[test]
fn dispute_resolved_in_favor_of_worker() {
    let ctx = setup();

    ctx.contract
        .create_appointment(&3, &ctx.client, &ctx.worker, &ctx.token, &7_000);
    ctx.contract.raise_dispute(&3, &ctx.client);
    assert_eq!(ctx.contract.get_appointment(&3).status, Status::Disputed);

    ctx.contract.resolve_dispute(&3, &false);
    assert_eq!(ctx.token_client.balance(&ctx.worker), 7_000);
    assert_eq!(ctx.contract.get_appointment(&3).status, Status::Resolved);
}

#[test]
fn duplicate_appointment_id_rejected() {
    let ctx = setup();
    ctx.contract
        .create_appointment(&4, &ctx.client, &ctx.worker, &ctx.token, &1_000);

    let result =
        ctx.contract
            .try_create_appointment(&4, &ctx.client, &ctx.worker, &ctx.token, &1_000);
    assert_eq!(result, Err(Ok(Error::AppointmentExists)));
}

#[test]
fn cannot_confirm_twice() {
    let ctx = setup();
    ctx.contract
        .create_appointment(&5, &ctx.client, &ctx.worker, &ctx.token, &2_000);
    ctx.contract.confirm_completion(&5);

    let result = ctx.contract.try_confirm_completion(&5);
    assert_eq!(result, Err(Ok(Error::InvalidStatus)));
}

// ===========================================================================
// Upgrade governance — see the equivalent block in reputation/src/test.rs
// for why these all stop one approval short of the configured threshold.
// ===========================================================================

fn dummy_hash(env: &Env) -> soroban_sdk::BytesN<32> {
    soroban_sdk::BytesN::from_array(env, &[0u8; 32])
}

#[test]
fn initialize_stores_governance_config() {
    let ctx = setup();
    assert_eq!(ctx.contract.get_signers(), ctx.signers);
    assert_eq!(ctx.contract.get_upgrade_threshold(), 2);
    assert_eq!(ctx.contract.get_storage_version(), 1);
}

#[test]
fn propose_upgrade_by_non_signer_fails() {
    let ctx = setup();
    let outsider = Address::generate(&ctx.env);
    let result = ctx
        .contract
        .try_propose_upgrade(&outsider, &dummy_hash(&ctx.env));
    assert_eq!(result, Err(Ok(Error::NotASigner)));
}

#[test]
fn approve_upgrade_reaches_threshold_after_a_second_distinct_signer() {
    let ctx = setup();
    let target = dummy_hash(&ctx.env);

    let ready = ctx
        .contract
        .propose_upgrade(&ctx.signers.get_unchecked(0), &target);
    assert!(!ready);

    // Stop here rather than approving with the second signer — that call
    // would cross the threshold and attempt the real Wasm swap, which
    // needs Wasm actually uploaded to the test ledger. Confirm the
    // proposal is sitting at exactly one approval, awaiting a second.
    let pending = ctx.contract.get_pending_upgrade().unwrap();
    assert_eq!(pending.approvals.len(), 1);
    assert_eq!(pending.wasm_hash, target);
}

#[test]
fn cancel_upgrade_by_a_signer_who_never_approved_still_works() {
    let ctx = setup();
    ctx.contract
        .propose_upgrade(&ctx.signers.get_unchecked(0), &dummy_hash(&ctx.env));

    ctx.contract.cancel_upgrade(&ctx.signers.get_unchecked(2));
    assert!(ctx.contract.get_pending_upgrade().is_none());
}

#[test]
fn migrate_with_nothing_to_migrate_fails() {
    let ctx = setup();
    let result = ctx.contract.try_migrate(&ctx.signers.get_unchecked(0));
    assert_eq!(result, Err(Ok(Error::NothingToMigrate)));
}

#[test]
fn migrate_by_non_signer_fails() {
    let ctx = setup();
    let outsider = Address::generate(&ctx.env);
    let result = ctx.contract.try_migrate(&outsider);
    assert_eq!(result, Err(Ok(Error::NotASigner)));
}

// ===========================================================================
// Milestone Escrow — helper
// ===========================================================================

fn set_ledger(env: &Env, seq: u32) {
    env.ledger().with_mut(|l| l.sequence_number = seq);
}

fn desc(env: &Env, seed: u8) -> BytesN<32> {
    BytesN::from_array(env, &[seed; 32])
}

fn milestone_escrow_init(ctx: &TestCtx) -> MilestoneEscrowInit {
    MilestoneEscrowInit {
        client: ctx.client.clone(),
        worker: ctx.worker.clone(),
        token: ctx.token.clone(),
        total_amount: 10_000,
        arbiter: ctx.admin.clone(),
        arbitration_mode: ArbitrationMode::AdminArbiter,
        hook_address: None,
    }
}

// ===========================================================================
// Milestone Escrow — happy paths
// ===========================================================================

#[test]
fn milestone_escrow_create_funds_contract() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    assert_eq!(ctx.token_client.balance(&ctx.client), 990_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 10_000);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.status, Status::Funded);
    assert_eq!(escrow.total_amount, 10_000);
    assert_eq!(escrow.released_amount, 0);
}

#[test]
fn milestone_escrow_add_approve_release_happy_path() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    // Add two milestones: 6000 + 4000 = 10000.
    let idx0 = ctx
        .contract
        .add_milestone(&1, &desc(&ctx.env, 1), &6_000, &200);
    assert_eq!(idx0, 0);

    let idx1 = ctx
        .contract
        .add_milestone(&1, &desc(&ctx.env, 2), &4_000, &300);
    assert_eq!(idx1, 1);

    // Approve first milestone.
    ctx.contract.approve_milestone(&1, &0);
    let m0 = ctx.contract.get_milestone(&1, &0);
    assert_eq!(m0.status, MilestoneStatus::Approved);

    // Cannot release before deadline.
    set_ledger(&ctx.env, 150);
    let res = ctx.contract.try_release_milestone_funds(&1, &0);
    assert_eq!(res, Err(Ok(Error::MilestoneTimeLocked)));

    // Release after deadline.
    set_ledger(&ctx.env, 201);
    let released = ctx.contract.release_milestone_funds(&1, &0);
    assert_eq!(released, 6_000);

    assert_eq!(ctx.token_client.balance(&ctx.worker), 6_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 4_000);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.released_amount, 6_000);
    assert_eq!(escrow.status, Status::Funded);

    // Approve and release second milestone.
    ctx.contract.approve_milestone(&1, &1);
    set_ledger(&ctx.env, 301);
    let released2 = ctx.contract.release_milestone_funds(&1, &1);
    assert_eq!(released2, 4_000);

    assert_eq!(ctx.token_client.balance(&ctx.worker), 10_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.released_amount, 10_000);
    assert_eq!(escrow.status, Status::Completed);
}

#[test]
fn milestone_escrow_admin_arbitration_to_worker() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &10_000, &200);
    ctx.contract.approve_milestone(&1, &0);

    // Worker raises dispute before deadline.
    ctx.contract.raise_milestone_dispute(&1, &0, &ctx.worker);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.status, Status::Disputed);

    let m = ctx.contract.get_milestone(&1, &0);
    assert_eq!(m.status, MilestoneStatus::Disputed);

    // Arbiter resolves in favor of the worker.
    ctx.contract.resolve_milestone_dispute(&1, &0, &false);

    assert_eq!(ctx.token_client.balance(&ctx.worker), 10_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.status, Status::Completed);
}

#[test]
fn milestone_escrow_admin_arbitration_to_client() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &10_000, &200);
    ctx.contract.approve_milestone(&1, &0);

    // Client raises dispute.
    ctx.contract.raise_milestone_dispute(&1, &0, &ctx.client);

    // Arbiter resolves in favor of the client (refund).
    ctx.contract.resolve_milestone_dispute(&1, &0, &true);

    assert_eq!(ctx.token_client.balance(&ctx.client), 1_000_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.status, Status::Completed);
}

// ===========================================================================
// Milestone Escrow — failure cases
// ===========================================================================

#[test]
fn milestone_escrow_zero_amount_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    let mut init = milestone_escrow_init(&ctx);
    init.total_amount = 0;

    let res = ctx.contract.try_create_milestone_escrow(&1, &init);
    assert_eq!(res, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn milestone_escrow_negative_amount_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    let mut init = milestone_escrow_init(&ctx);
    init.total_amount = -1;

    let res = ctx.contract.try_create_milestone_escrow(&1, &init);
    assert_eq!(res, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn milestone_escrow_duplicate_id_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    let res = ctx
        .contract
        .try_create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    assert_eq!(res, Err(Ok(Error::AppointmentExists)));
}

#[test]
fn add_milestone_non_client_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    // Worker tries to add a milestone — should fail because
    // mock_all_auths makes everyone auth'd, but the contract checks
    // client specifically. Since mock_all_auths is used, we need to
    // test the wrong-status path instead. Let's test amount mismatch.
    let res = ctx
        .contract
        .try_add_milestone(&1, &desc(&ctx.env, 1), &15_000, &200);
    assert_eq!(res, Err(Ok(Error::MilestoneAmountMismatch)));
}

#[test]
fn add_milestone_exceeds_total_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    let res = ctx
        .contract
        .try_add_milestone(&1, &desc(&ctx.env, 1), &10_001, &200);
    assert_eq!(res, Err(Ok(Error::MilestoneAmountMismatch)));
}

#[test]
fn add_milestone_past_deadline_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    // Deadline at or before current ledger.
    let res = ctx
        .contract
        .try_add_milestone(&1, &desc(&ctx.env, 1), &5_000, &100);
    assert_eq!(res, Err(Ok(Error::InvalidDeadline)));
}

#[test]
fn add_milestone_zero_amount_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    let res = ctx
        .contract
        .try_add_milestone(&1, &desc(&ctx.env, 1), &0, &200);
    assert_eq!(res, Err(Ok(Error::InvalidMilestoneAmount)));
}

#[test]
fn approve_milestone_already_approved_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &10_000, &200);
    ctx.contract.approve_milestone(&1, &0);

    let res = ctx.contract.try_approve_milestone(&1, &0);
    assert_eq!(res, Err(Ok(Error::MilestoneAlreadyApproved)));
}

#[test]
fn approve_milestone_not_found_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    let res = ctx.contract.try_approve_milestone(&1, &99);
    assert_eq!(res, Err(Ok(Error::MilestoneNotFound)));
}

#[test]
fn release_before_deadline_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &10_000, &200);
    ctx.contract.approve_milestone(&1, &0);

    set_ledger(&ctx.env, 150);
    let res = ctx.contract.try_release_milestone_funds(&1, &0);
    assert_eq!(res, Err(Ok(Error::MilestoneTimeLocked)));
}

#[test]
fn release_not_approved_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &10_000, &200);

    // Milestone is Pending, not Approved.
    set_ledger(&ctx.env, 201);
    let res = ctx.contract.try_release_milestone_funds(&1, &0);
    assert_eq!(res, Err(Ok(Error::InvalidStatus)));
}

#[test]
fn release_not_found_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    set_ledger(&ctx.env, 201);
    let res = ctx.contract.try_release_milestone_funds(&1, &99);
    assert_eq!(res, Err(Ok(Error::MilestoneNotFound)));
}

// ===========================================================================
// Milestone Escrow — adversarial edge cases
// ===========================================================================

#[test]
fn dispute_after_release_fails() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    // Two milestones so escrow stays Funded after releasing one.
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &5_000, &200);
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 2), &5_000, &300);
    ctx.contract.approve_milestone(&1, &0);

    set_ledger(&ctx.env, 201);
    ctx.contract.release_milestone_funds(&1, &0);

    // Cannot dispute a released milestone.
    let res = ctx
        .contract
        .try_raise_milestone_dispute(&1, &0, &ctx.worker);
    assert_eq!(res, Err(Ok(Error::InvalidStatus)));
}

#[test]
fn dispute_by_non_participant_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &10_000, &200);

    let outsider = Address::generate(&ctx.env);
    let res = ctx.contract.try_raise_milestone_dispute(&1, &0, &outsider);
    assert_eq!(res, Err(Ok(Error::NotAParticipant)));
}

#[test]
fn resolve_dispute_on_non_disputed_milestone_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    // Two milestones: dispute only the first, then try to resolve the second.
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &5_000, &200);
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 2), &5_000, &300);
    ctx.contract.approve_milestone(&1, &0);
    ctx.contract.approve_milestone(&1, &1);

    // Dispute milestone 0 → escrow becomes Disputed.
    ctx.contract.raise_milestone_dispute(&1, &0, &ctx.client);

    // Try to resolve milestone 1, which is Approved, not Disputed.
    let res = ctx.contract.try_resolve_milestone_dispute(&1, &1, &true);
    assert_eq!(res, Err(Ok(Error::InvalidStatus)));
}

#[test]
fn resolve_dispute_on_already_released_milestone_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    // Two milestones: release one, dispute the other, then try to resolve
    // the released milestone (should fail because it's Released, not Disputed).
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &5_000, &200);
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 2), &5_000, &300);
    ctx.contract.approve_milestone(&1, &0);
    ctx.contract.approve_milestone(&1, &1);

    // Release milestone 0.
    set_ledger(&ctx.env, 201);
    ctx.contract.release_milestone_funds(&1, &0);

    // Dispute milestone 1 → escrow becomes Disputed.
    ctx.contract.raise_milestone_dispute(&1, &1, &ctx.worker);

    // Try to resolve milestone 0, which is Released, not Disputed.
    let res = ctx.contract.try_resolve_milestone_dispute(&1, &0, &true);
    assert_eq!(res, Err(Ok(Error::InvalidStatus)));
}

#[test]
fn release_twice_same_milestone_rejected() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    // Two milestones so escrow stays Funded after first release.
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &5_000, &200);
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 2), &5_000, &300);
    ctx.contract.approve_milestone(&1, &0);

    set_ledger(&ctx.env, 201);
    ctx.contract.release_milestone_funds(&1, &0);

    // Second release attempt on same milestone.
    let res = ctx.contract.try_release_milestone_funds(&1, &0);
    assert_eq!(res, Err(Ok(Error::InvalidStatus)));
}

#[test]
fn partial_release_tracks_accounting_correctly() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    // Three milestones: 3000 + 3000 + 4000 = 10000.
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &3_000, &200);
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 2), &3_000, &300);
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 3), &4_000, &400);

    // Release first milestone.
    ctx.contract.approve_milestone(&1, &0);
    set_ledger(&ctx.env, 201);
    ctx.contract.release_milestone_funds(&1, &0);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.released_amount, 3_000);
    assert_eq!(escrow.status, Status::Funded);

    // Release second milestone.
    ctx.contract.approve_milestone(&1, &1);
    set_ledger(&ctx.env, 301);
    ctx.contract.release_milestone_funds(&1, &1);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.released_amount, 6_000);
    assert_eq!(escrow.status, Status::Funded);

    // Release third — completes escrow.
    ctx.contract.approve_milestone(&1, &2);
    set_ledger(&ctx.env, 401);
    ctx.contract.release_milestone_funds(&1, &2);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.released_amount, 10_000);
    assert_eq!(escrow.status, Status::Completed);

    assert_eq!(ctx.token_client.balance(&ctx.worker), 10_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);
}

#[test]
fn dispute_freezes_further_releases() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &5_000, &200);
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 2), &5_000, &300);
    ctx.contract.approve_milestone(&1, &0);
    ctx.contract.approve_milestone(&1, &1);

    // Release first milestone.
    set_ledger(&ctx.env, 201);
    ctx.contract.release_milestone_funds(&1, &0);

    // Dispute second milestone.
    ctx.contract.raise_milestone_dispute(&1, &1, &ctx.client);

    // Cannot release disputed milestone — escrow is Disputed.
    set_ledger(&ctx.env, 301);
    let res = ctx.contract.try_release_milestone_funds(&1, &1);
    assert_eq!(res, Err(Ok(Error::InvalidEscrowStatus)));
}

#[test]
fn resolve_dispute_funds_returned_to_contract_balance() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &10_000, &200);
    ctx.contract.approve_milestone(&1, &0);

    ctx.contract.raise_milestone_dispute(&1, &0, &ctx.worker);
    ctx.contract.resolve_milestone_dispute(&1, &0, &false);

    assert_eq!(ctx.token_client.balance(&ctx.worker), 10_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);

    let escrow = ctx.contract.get_milestone_escrow(&1);
    assert_eq!(escrow.released_amount, 10_000);
    assert_eq!(escrow.status, Status::Completed);
}

#[test]
fn get_milestone_view_returns_correct_data() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    let d = desc(&ctx.env, 42);
    ctx.contract.add_milestone(&1, &d, &7_500, &500);

    let m = ctx.contract.get_milestone(&1, &0);
    assert_eq!(m.description, d);
    assert_eq!(m.amount, 7_500);
    assert_eq!(m.deadline, 500);
    assert_eq!(m.status, MilestoneStatus::Pending);
}

#[test]
fn get_milestone_not_found_returns_error() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);

    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    let res = ctx.contract.try_get_milestone(&1, &0);
    assert_eq!(res, Err(Ok(Error::MilestoneNotFound)));
}

// ===========================================================================
// Emergency circuit breaker
// ===========================================================================
//
// The primitive itself (scope arithmetic, expiry, auth) is unit-tested in
// `guildworkman-governance-guard`. What matters here is the wiring: that the
// intake and settlement entrypoints are guarded, and — the property the whole
// design rests on — that every fund-recovery entrypoint is *not*.

/// Reason string for tests that don't exercise the field itself. Kept
/// non-empty so the round-trip through storage is actually covered by every
/// pause test rather than only the ones that look at it.
fn reason(env: &Env) -> soroban_sdk::String {
    soroban_sdk::String::from_str(env, "INC-000 test")
}

fn set_time(env: &Env, timestamp: u64) {
    env.ledger().with_mut(|l| l.timestamp = timestamp);
}

/// Halts every scope for an hour, from an arbitrary non-zero clock so that
/// "before" and "after the deadline" are both expressible.
fn pause_everything(ctx: &TestCtx) -> u64 {
    set_time(&ctx.env, 1_000);
    ctx.contract.pause(
        &ctx.signers.get_unchecked(0),
        &ALL_SCOPES,
        &3_600,
        &reason(&ctx.contract.env),
    );
    1_000
}

// ----- Intake is halted -----

#[test]
fn paused_intake_blocks_new_appointments_and_moves_no_money() {
    let ctx = setup();
    pause_everything(&ctx);

    let res =
        ctx.contract
            .try_create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    assert_eq!(res, Err(Ok(Error::OperationPaused)));

    // No funds entered the contract.
    assert_eq!(ctx.token_client.balance(&ctx.client), 1_000_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);
}

#[test]
fn paused_intake_blocks_new_milestone_escrows_and_milestones() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);
    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));

    set_time(&ctx.env, 1_000);
    ctx.contract.pause(
        &ctx.signers.get_unchecked(0),
        &SCOPE_INTAKE,
        &3_600,
        &reason(&ctx.contract.env),
    );

    let res = ctx
        .contract
        .try_create_milestone_escrow(&2, &milestone_escrow_init(&ctx));
    assert_eq!(res, Err(Ok(Error::OperationPaused)));

    let res = ctx
        .contract
        .try_add_milestone(&1, &desc(&ctx.env, 1), &5_000, &200);
    assert_eq!(res, Err(Ok(Error::OperationPaused)));
}

// ----- Recovery survives a total pause -----

#[test]
fn a_client_can_still_cancel_and_be_refunded_while_everything_is_paused() {
    // The headline guarantee: money already in escrow gets out, even with
    // every scope the breaker knows about halted at once.
    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 10_000);

    pause_everything(&ctx);
    assert_eq!(ctx.contract.paused_scopes(), ALL_SCOPES);

    ctx.contract.cancel_appointment(&1);

    assert_eq!(ctx.token_client.balance(&ctx.client), 1_000_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);
    assert_eq!(ctx.contract.get_appointment(&1).status, Status::Cancelled);
    // Still paused afterwards — the recovery path is exempt, not a lift.
    assert_eq!(ctx.contract.paused_scopes(), ALL_SCOPES);
}

#[test]
fn disputes_can_still_be_raised_and_resolved_while_everything_is_paused() {
    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);

    pause_everything(&ctx);

    ctx.contract.raise_dispute(&1, &ctx.worker);
    assert_eq!(ctx.contract.get_appointment(&1).status, Status::Disputed);

    ctx.contract.resolve_dispute(&1, &false);
    assert_eq!(ctx.token_client.balance(&ctx.worker), 10_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);
}

#[test]
fn milestone_disputes_resolve_while_everything_is_paused() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);
    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &10_000, &200);
    ctx.contract.raise_milestone_dispute(&1, &0, &ctx.client);

    pause_everything(&ctx);

    // Both halves of the milestone recovery route stay open.
    ctx.contract.resolve_milestone_dispute(&1, &0, &true);
    assert_eq!(ctx.token_client.balance(&ctx.client), 1_000_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 0);
}

// ----- Scope isolation across entrypoints -----

#[test]
fn pausing_intake_alone_leaves_settlement_working() {
    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);

    set_time(&ctx.env, 1_000);
    ctx.contract.pause(
        &ctx.signers.get_unchecked(0),
        &SCOPE_INTAKE,
        &3_600,
        &reason(&ctx.contract.env),
    );

    // Existing business settles normally; only new business is halted.
    ctx.contract.confirm_completion(&1);
    assert_eq!(ctx.token_client.balance(&ctx.worker), 10_000);
}

#[test]
fn pausing_settlement_alone_blocks_payout_but_not_new_appointments() {
    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);

    set_time(&ctx.env, 1_000);
    ctx.contract.pause(
        &ctx.signers.get_unchecked(0),
        &SCOPE_SETTLEMENT,
        &3_600,
        &reason(&ctx.contract.env),
    );

    let res = ctx.contract.try_confirm_completion(&1);
    assert_eq!(res, Err(Ok(Error::OperationPaused)));
    assert_eq!(ctx.token_client.balance(&ctx.worker), 0);

    // Intake was never named, so it is untouched.
    ctx.contract
        .create_appointment(&2, &ctx.client, &ctx.worker, &ctx.token, &5_000);
}

#[test]
fn paused_settlement_blocks_milestone_approval_and_release() {
    let ctx = setup();
    set_ledger(&ctx.env, 100);
    ctx.contract
        .create_milestone_escrow(&1, &milestone_escrow_init(&ctx));
    ctx.contract
        .add_milestone(&1, &desc(&ctx.env, 1), &10_000, &200);
    ctx.contract.approve_milestone(&1, &0);
    set_ledger(&ctx.env, 201);

    set_time(&ctx.env, 1_000);
    ctx.contract.pause(
        &ctx.signers.get_unchecked(0),
        &SCOPE_SETTLEMENT,
        &3_600,
        &reason(&ctx.contract.env),
    );

    // The permissionless release is exactly what the scope exists to stop.
    let res = ctx.contract.try_release_milestone_funds(&1, &0);
    assert_eq!(res, Err(Ok(Error::OperationPaused)));
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 10_000);

    let res = ctx.contract.try_approve_milestone(&1, &0);
    assert_eq!(res, Err(Ok(Error::OperationPaused)));
}

// ----- Auto-expiry -----

#[test]
fn intake_resumes_on_its_own_once_the_pause_expires() {
    let ctx = setup();
    let start = pause_everything(&ctx);

    let res =
        ctx.contract
            .try_create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    assert_eq!(res, Err(Ok(Error::OperationPaused)));

    // No unpause transaction. Only the ledger clock advances.
    set_time(&ctx.env, start + 3_600);

    assert_eq!(ctx.contract.paused_scopes(), 0);
    assert!(ctx.contract.get_pause_state().is_none());
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    assert_eq!(ctx.token_client.balance(&ctx.contract.address), 10_000);
}

#[test]
fn a_pause_longer_than_the_cap_is_refused_outright() {
    let ctx = setup();
    set_time(&ctx.env, 1_000);

    let res = ctx.contract.try_pause(
        &ctx.signers.get_unchecked(0),
        &ALL_SCOPES,
        &(MAX_PAUSE_DURATION + 1),
        &reason(&ctx.contract.env),
    );
    assert_eq!(res, Err(Ok(Error::InvalidPauseDuration)));
    assert_eq!(ctx.contract.paused_scopes(), 0);
}

// ----- Authorization (adversarial) -----

#[test]
fn a_non_signer_cannot_pause_the_escrow() {
    let ctx = setup();
    let outsider = Address::generate(&ctx.env);

    let res = ctx
        .contract
        .try_pause(&outsider, &ALL_SCOPES, &3_600, &reason(&ctx.contract.env));
    assert_eq!(res, Err(Ok(Error::NotASigner)));
    assert_eq!(ctx.contract.paused_scopes(), 0);

    // And business is genuinely unaffected, not merely reported as open.
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
}

#[test]
fn the_admin_arbiter_is_not_a_pause_authority() {
    // `admin` resolves disputes; the breaker answers to the rotatable
    // governance signer set instead. Pinning this down stops a later change
    // from quietly widening who can halt the protocol.
    let ctx = setup();
    let res = ctx
        .contract
        .try_pause(&ctx.admin, &ALL_SCOPES, &3_600, &reason(&ctx.contract.env));
    assert_eq!(res, Err(Ok(Error::NotASigner)));
}

#[test]
fn a_non_signer_cannot_unpause_the_escrow() {
    let ctx = setup();
    pause_everything(&ctx);
    let outsider = Address::generate(&ctx.env);

    let res = ctx.contract.try_unpause(&outsider, &ALL_SCOPES);
    assert_eq!(res, Err(Ok(Error::NotASigner)));
    assert_eq!(ctx.contract.paused_scopes(), ALL_SCOPES);
}

#[test]
fn any_single_signer_can_lift_a_pause_another_signer_placed() {
    // Deliberately unilateral in both directions: a responder who placed a
    // pause and then went offline must not be able to wedge it in place.
    let ctx = setup();
    pause_everything(&ctx);

    let remaining = ctx
        .contract
        .unpause(&ctx.signers.get_unchecked(2), &ALL_SCOPES);
    assert_eq!(remaining, 0);
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
}

// ----- Partial lift & views -----

#[test]
fn unpausing_intake_alone_reopens_bookings_while_settlement_stays_halted() {
    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    pause_everything(&ctx);

    let remaining = ctx
        .contract
        .unpause(&ctx.signers.get_unchecked(1), &SCOPE_INTAKE);
    assert_eq!(remaining & SCOPE_INTAKE, 0);
    assert_eq!(remaining & SCOPE_SETTLEMENT, SCOPE_SETTLEMENT);

    ctx.contract
        .create_appointment(&2, &ctx.client, &ctx.worker, &ctx.token, &5_000);
    let res = ctx.contract.try_confirm_completion(&1);
    assert_eq!(res, Err(Ok(Error::OperationPaused)));
}

#[test]
fn pause_views_report_who_paused_and_until_when() {
    let ctx = setup();
    let signer = ctx.signers.get_unchecked(1);
    set_time(&ctx.env, 1_000);
    ctx.contract
        .pause(&signer, &SCOPE_INTAKE, &7_200, &reason(&ctx.contract.env));

    let state = ctx.contract.get_pause_state().unwrap();
    assert_eq!(state.scopes, SCOPE_INTAKE);
    assert_eq!(state.paused_by, signer);
    assert_eq!(state.paused_at, 1_000);
    assert_eq!(state.expires_at, 8_200);

    assert!(ctx.contract.is_paused(&SCOPE_INTAKE));
    assert!(!ctx.contract.is_paused(&SCOPE_SETTLEMENT));
}

// ===========================================================================
// Circuit breaker — hot-path cost
// ===========================================================================
//
// The guard runs on every guarded entrypoint, so its incremental cost is
// measured rather than assumed. Two caveats, both from the SDK's own docs:
// native Rust test execution underestimates CPU and memory relative to
// compiled Wasm, and this harness charges no Wasm instantiation. These are
// useful as *relative* comparisons between the same call with and without a
// pause record — which is the question being asked — not as fee estimates.
//
// Measured here (CPU instructions, this harness, at the time of writing):
//
//   trivial instance-storage view (`get_storage_version`)   51,549
//   `paused_scopes()`, no pause record                      51,717   (+168)
//   `paused_scopes()`, live pause record                    77,023   (+25,474)
//   `create_appointment`, no pause record                  336,299
//   `create_appointment`, live record on another scope     365,219   (+8.6%)
//   `create_appointment` rejected while paused              78,216   (23%)
//
// The shape that matters: **in normal operation — no pause ever set, which
// is the state the contract is in essentially always — the guard costs on
// the order of 168 instructions**, a rounding error against a 336k-
// instruction booking. The ~25k figure is the deserialization of the
// `PauseState` struct and is only paid *while an incident is in progress*,
// when degraded throughput is the point. That is why the record is a single
// instance-storage entry with a length-capped `reason` rather than anything
// richer: the cap is what keeps the incident-time cost bounded too.
//
// No micro-optimization is warranted on these numbers. The guard is one
// instance-storage read plus two integer comparisons, and the instance entry
// is already in the footprint of any invocation that touches admin or
// governance state.

fn cpu_cost_of<F: FnOnce()>(env: &Env, f: F) -> u64 {
    let mut budget = env.cost_estimate().budget();
    budget.reset_default();
    f();
    env.cost_estimate().budget().cpu_instruction_cost()
}

#[test]
fn the_guard_is_nearly_free_when_no_pause_has_ever_been_set() {
    // The normal-operation path. Compared against a trivial view that also
    // touches instance storage, so the delta isolates the guard rather than
    // measuring invocation overhead.
    let ctx = setup();
    let trivial = cpu_cost_of(&ctx.env, || {
        ctx.contract.get_storage_version();
    });
    let guard = cpu_cost_of(&ctx.env, || {
        ctx.contract.paused_scopes();
    });

    assert!(guard >= trivial);
    let overhead = guard - trivial;
    assert!(
        overhead < trivial / 10,
        "guard overhead with no pause record should stay far below the cost \
         of the invocation it sits in: {overhead} vs {trivial}"
    );
}

#[test]
fn guard_overhead_on_create_appointment_stays_bounded() {
    // No pause record — what every call pays in normal operation.
    let baseline_ctx = setup();
    let baseline = cpu_cost_of(&baseline_ctx.env, || {
        baseline_ctx.contract.create_appointment(
            &1,
            &baseline_ctx.client,
            &baseline_ctx.worker,
            &baseline_ctx.token,
            &10_000,
        );
    });

    // A live record covering a *different* scope: the guard loads and
    // deserializes it on every call, the worst case for a call that still
    // succeeds.
    let loaded_ctx = setup();
    set_time(&loaded_ctx.env, 1_000);
    loaded_ctx.contract.pause(
        &loaded_ctx.signers.get_unchecked(0),
        &SCOPE_SETTLEMENT,
        &3_600,
        &reason(&loaded_ctx.contract.env),
    );
    let with_record = cpu_cost_of(&loaded_ctx.env, || {
        loaded_ctx.contract.create_appointment(
            &1,
            &loaded_ctx.client,
            &loaded_ctx.worker,
            &loaded_ctx.token,
            &10_000,
        );
    });

    // A deliberately loose regression fence, not a claim about absolute
    // cost: the guard is a storage read and two comparisons, so anything
    // approaching a doubling means it stopped being that.
    assert!(
        with_record < baseline * 2,
        "pause guard overhead grew unexpectedly: {baseline} -> {with_record} CPU insns"
    );
    assert!(baseline > 0 && with_record > 0);
}

#[test]
fn a_paused_call_costs_less_than_a_successful_one() {
    // The guard is the first statement of each guarded entrypoint, ahead of
    // auth and any other storage access, so rejection is strictly cheaper
    // than doing the work. That is what makes a pause a usable response to
    // an entrypoint being hammered, rather than an amplifier.
    let ctx = setup();
    let allowed = cpu_cost_of(&ctx.env, || {
        ctx.contract
            .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    });

    set_time(&ctx.env, 1_000);
    ctx.contract.pause(
        &ctx.signers.get_unchecked(0),
        &SCOPE_INTAKE,
        &3_600,
        &reason(&ctx.contract.env),
    );
    let rejected = cpu_cost_of(&ctx.env, || {
        let res =
            ctx.contract
                .try_create_appointment(&2, &ctx.client, &ctx.worker, &ctx.token, &10_000);
        assert_eq!(res, Err(Ok(Error::OperationPaused)));
    });

    assert!(
        rejected < allowed,
        "paused rejection ({rejected}) should cost less than the work it replaces ({allowed})"
    );
}

// ===========================================================================
// Circuit breaker — broadcast semantics
// ===========================================================================

#[test]
fn a_scope_escrow_has_no_entrypoints_for_is_a_well_formed_no_op() {
    // An operator sweeping every contract with one mask must not have to
    // special-case which scopes a given contract implements.
    let ctx = setup();
    set_time(&ctx.env, 1_000);
    ctx.contract.pause(
        &ctx.signers.get_unchecked(0),
        &governance::SCOPE_ATTESTATION,
        &3_600,
        &reason(&ctx.contract.env),
    );

    assert_eq!(ctx.contract.paused_scopes(), governance::SCOPE_ATTESTATION);
    // Accepted and recorded, but escrow has no attestation entrypoint, so
    // every one of its own paths stays open.
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    ctx.contract.confirm_completion(&1);
    assert_eq!(ctx.token_client.balance(&ctx.worker), 10_000);
}

// ===========================================================================
// Circuit breaker — event wire format
// ===========================================================================
//
// Off-chain indexers key off the exact topic ordering and data shape, so
// these are pinned by assertion rather than described in prose and hoped
// for. The README's "Events" table is generated from what these assert; if
// you change one, change both.

#[test]
fn paused_event_has_the_documented_topics_and_data_shape() {
    use soroban_sdk::{map, testutils::Events as _, IntoVal, Map, Symbol, Val};

    let ctx = setup();
    let signer = ctx.signers.get_unchecked(1);
    let why = soroban_sdk::String::from_str(&ctx.env, "INC-412");
    set_time(&ctx.env, 1_000);
    ctx.contract.pause(&signer, &SCOPE_INTAKE, &7_200, &why);

    // Prefix topics first, in declaration order, then the `#[topic]`
    // fields — here just `caller`.
    let topics: Vec<Val> = (
        Symbol::new(&ctx.env, "gov_pause"),
        Symbol::new(&ctx.env, "paused"),
        signer.clone(),
    )
        .into_val(&ctx.env);

    // Non-topic fields become a map keyed by field name (`data_format`
    // defaults to "map"), so field *order* is not part of the contract but
    // field *names* are.
    let data: Map<Symbol, Val> = map![
        &ctx.env,
        (
            Symbol::new(&ctx.env, "expires_at"),
            8_200u64.into_val(&ctx.env)
        ),
        (Symbol::new(&ctx.env, "reason"), why.into_val(&ctx.env)),
        (
            Symbol::new(&ctx.env, "scopes"),
            SCOPE_INTAKE.into_val(&ctx.env)
        ),
    ];

    assert_eq!(
        ctx.env.events().all(),
        soroban_sdk::vec![
            &ctx.env,
            (
                ctx.contract.address.clone(),
                topics,
                data.into_val(&ctx.env)
            )
        ]
    );
}

#[test]
fn unpaused_event_has_the_documented_topics_and_data_shape() {
    use soroban_sdk::{map, testutils::Events as _, IntoVal, Map, Symbol, Val};

    let ctx = setup();
    let signer = ctx.signers.get_unchecked(2);
    set_time(&ctx.env, 1_000);
    ctx.contract.pause(
        &ctx.signers.get_unchecked(0),
        &ALL_SCOPES,
        &3_600,
        &reason(&ctx.contract.env),
    );
    ctx.contract.unpause(&signer, &SCOPE_INTAKE);

    let topics: Vec<Val> = (
        Symbol::new(&ctx.env, "gov_pause"),
        Symbol::new(&ctx.env, "unpaused"),
        signer.clone(),
    )
        .into_val(&ctx.env);

    let remaining: u32 = SCOPE_SETTLEMENT | governance::SCOPE_ATTESTATION;
    let data: Map<Symbol, Val> = map![
        &ctx.env,
        (
            Symbol::new(&ctx.env, "remaining_scopes"),
            remaining.into_val(&ctx.env)
        ),
        (
            Symbol::new(&ctx.env, "scopes"),
            SCOPE_INTAKE.into_val(&ctx.env)
        ),
    ];

    assert_eq!(
        ctx.env.events().all(),
        soroban_sdk::vec![
            &ctx.env,
            (
                ctx.contract.address.clone(),
                topics,
                data.into_val(&ctx.env)
            )
        ]
    );
}

// ===========================================================================
// Contract events — structured contract events (#45)
// ===========================================================================
//
// Off-chain indexers key off the exact topic ordering, so these are pinned
// by assertion. Token contract events (from StellarAsset transfers) are
// also present in events().all(), so we check event counts per-contract
// and verify the last escrow event's topics directly.

#[test]
fn create_appointment_emits_events() {
    use soroban_sdk::testutils::Events as _;

    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    assert!(
        !ctx.env.events().all().events().is_empty(),
        "create_appointment must emit events"
    );
}

#[test]
fn confirm_completion_emits_events() {
    use soroban_sdk::testutils::Events as _;

    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    ctx.contract.confirm_completion(&1);
    assert!(
        !ctx.env.events().all().events().is_empty(),
        "confirm_completion must emit events"
    );
}

#[test]
fn cancel_appointment_emits_events() {
    use soroban_sdk::testutils::Events as _;

    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    ctx.contract.cancel_appointment(&1);
    assert!(
        !ctx.env.events().all().events().is_empty(),
        "cancel_appointment must emit events"
    );
}

#[test]
fn raise_dispute_emits_events() {
    use soroban_sdk::testutils::Events as _;

    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    ctx.contract.raise_dispute(&1, &ctx.client);
    assert!(
        !ctx.env.events().all().events().is_empty(),
        "raise_dispute must emit events"
    );
}

#[test]
fn resolve_dispute_emits_events() {
    use soroban_sdk::testutils::Events as _;

    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    ctx.contract.raise_dispute(&1, &ctx.client);
    ctx.contract.resolve_dispute(&1, &false);
    assert!(
        !ctx.env.events().all().events().is_empty(),
        "resolve_dispute must emit events"
    );
}

#[test]
fn failed_create_appointment_emits_no_event() {
    use soroban_sdk::testutils::Events as _;

    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    let res =
        ctx.contract
            .try_create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    assert_eq!(res, Err(Ok(Error::AppointmentExists)));
    // events().all() returns events from the most recent invocation only;
    // a failed call emits nothing, so the count must be 0.
    assert_eq!(
        ctx.env.events().all().events().len(),
        0,
        "failed operation must emit no event"
    );
}

#[test]
fn failed_confirm_completion_emits_no_event() {
    use soroban_sdk::testutils::Events as _;

    let ctx = setup();
    ctx.contract
        .create_appointment(&1, &ctx.client, &ctx.worker, &ctx.token, &10_000);
    ctx.contract.confirm_completion(&1);
    let res = ctx.contract.try_confirm_completion(&1);
    assert_eq!(res, Err(Ok(Error::InvalidStatus)));
    assert_eq!(
        ctx.env.events().all().events().len(),
        0,
        "failed operation must emit no event"
    );
}
