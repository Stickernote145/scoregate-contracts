//! Adversarial tests for co-signature replay across ledger sequences (#264).
//!
//! These tests verify that governance proposals (ParameterProposal,
//! PolicyBundleProposal, UpgradeProposal) cannot be re-executed or re-applied
//! after a prior execution, veto, or expiry — even when the admin re-presents
//! the exact same signed parameters at a later ledger sequence.

#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Bytes, BytesN, Env, Vec,
};

use crate::{
    constants::{DEFAULT_PARAM_CHANGE_DELAY_SECS, DEFAULT_UPGRADE_DELAY_SECS, MIN_COOLDOWN_SECS},
    parameter_governance::param_key_cooldown,
    types::ParameterProposalStatus,
    Error, ScoreGateScoreContract, ScoreGateScoreContractClient,
};

const START_TS: u64 = 1_700_000_000;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn setup<'a>() -> (Env, ScoreGateScoreContractClient<'a>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = START_TS);

    let contract_id = env.register_contract(None, ScoreGateScoreContract);
    let client = ScoreGateScoreContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let service = Address::generate(&env);
    client.initialize(&admin, &service);

    (env, client, admin, service)
}

fn setup_multisig<'a>(
) -> (Env, ScoreGateScoreContractClient<'a>, Address, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = START_TS);

    let contract_id = env.register_contract(None, ScoreGateScoreContract);
    let client = ScoreGateScoreContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let service = Address::generate(&env);
    client.initialize(&admin, &service);

    // Set up 2-of-3 admin multisig.
    let a1 = Address::generate(&env);
    let a2 = Address::generate(&env);
    let a3 = Address::generate(&env);
    client.add_admin_signer(&Vec::new(&env), &a1);
    client.add_admin_signer(&Vec::new(&env), &a2);
    client.add_admin_signer(&Vec::new(&env), &a3);
    client.set_admin_threshold(&Vec::new(&env), &2);

    (env, client, a1, a2, a3, service)
}

fn admin_signers(env: &Env, admin: &Address) -> Vec<Address> {
    Vec::from_array(env, [admin.clone()])
}

fn service_signers(env: &Env, service: &Address) -> Vec<Address> {
    Vec::from_array(env, [service.clone()])
}

fn two_signers(env: &Env, a: &Address, b: &Address) -> Vec<Address> {
    Vec::from_array(env, [a.clone(), b.clone()])
}

fn encode_u64(env: &Env, value: u64) -> Bytes {
    Bytes::from_array(env, &value.to_be_bytes())
}

fn advance(env: &Env, secs: u64) {
    env.ledger().with_mut(|l| l.timestamp += secs);
}

fn dummy_hash(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[0xabu8; 32])
}

// ── ParameterProposal: executed proposal cannot be re-executed ────────────────

/// A ParameterProposal that has been executed transitions to status `Executed`.
/// Submitting the same proposal ID again at a later ledger sequence (simulating
/// a replay of the original co-signatures) must be rejected.
#[test]
fn test_executed_parameter_proposal_cannot_be_replayed() {
    let (env, client, admin, _service) = setup();
    let value = encode_u64(&env, MIN_COOLDOWN_SECS);

    let proposal_id = client.propose_parameter_change(
        &admin_signers(&env, &admin),
        &param_key_cooldown(),
        &value,
    );

    // Advance past the time-lock and execute.
    advance(&env, DEFAULT_UPGRADE_DELAY_SECS);
    client.execute_parameter_change(&admin_signers(&env, &admin), &proposal_id);

    let record = client.get_parameter_proposal(&proposal_id);
    assert_eq!(record.status, ParameterProposalStatus::Executed);

    // Advance to a new ledger sequence (simulating a replay at a later point).
    advance(&env, DEFAULT_UPGRADE_DELAY_SECS);

    // Re-executing the same proposal ID must be rejected — even though the
    // parameters and admin signatures are identical.
    let result = client.try_execute_parameter_change(&admin_signers(&env, &admin), &proposal_id);
    assert_eq!(
        result,
        Err(Ok(Error::ParameterProposalAlreadyExecuted)),
        "Replay of an executed ParameterProposal must be rejected"
    );
}

/// A ParameterProposal vetoed by service cannot be executed even if the admin
/// re-presents the same proposal ID and credentials at a later ledger sequence.
#[test]
fn test_vetoed_parameter_proposal_cannot_be_replayed() {
    let (env, client, admin, service) = setup();
    let value = encode_u64(&env, MIN_COOLDOWN_SECS);

    let proposal_id = client.propose_parameter_change(
        &admin_signers(&env, &admin),
        &param_key_cooldown(),
        &value,
    );

    // Veto during the veto window.
    client.veto_parameter_change(&service_signers(&env, &service), &proposal_id);

    let record = client.get_parameter_proposal(&proposal_id);
    assert_eq!(record.status, ParameterProposalStatus::Vetoed);

    // Advance well past the original time-lock — simulating a fresh ledger sequence.
    advance(&env, DEFAULT_UPGRADE_DELAY_SECS * 2);

    // Attempt execution of the vetoed proposal at the new sequence.
    let result = client.try_execute_parameter_change(&admin_signers(&env, &admin), &proposal_id);
    assert_eq!(
        result,
        Err(Ok(Error::ParameterProposalVetoed)),
        "Replay execution of a vetoed ParameterProposal must be rejected"
    );
}

/// A ParameterProposal that has expired cannot be executed at a later ledger
/// sequence — the `proposed_at + time_lock_secs * 2` expiry is absolute.
#[test]
fn test_expired_parameter_proposal_cannot_be_replayed() {
    let (env, client, admin, _service) = setup();
    let value = encode_u64(&env, MIN_COOLDOWN_SECS);

    let proposal_id = client.propose_parameter_change(
        &admin_signers(&env, &admin),
        &param_key_cooldown(),
        &value,
    );

    // Advance past the expiry window (time_lock_secs * 2).
    advance(&env, DEFAULT_UPGRADE_DELAY_SECS * 2 + 1);

    // Attempting to execute after expiry must be rejected.
    let result = client.try_execute_parameter_change(&admin_signers(&env, &admin), &proposal_id);
    assert_eq!(
        result,
        Err(Ok(Error::ParameterProposalExpired)),
        "Replay execution of an expired ParameterProposal must be rejected"
    );

    // Advance even further — the expiry gate must remain closed.
    advance(&env, DEFAULT_UPGRADE_DELAY_SECS * 4);
    let result = client.try_execute_parameter_change(&admin_signers(&env, &admin), &proposal_id);
    assert_eq!(
        result,
        Err(Ok(Error::ParameterProposalExpired)),
        "Re-executing a long-expired ParameterProposal must still be rejected"
    );
}

// ── ParameterProposal: proposal IDs are monotonic (no reuse) ─────────────────

/// After a proposal is consumed (executed or vetoed), the next call to
/// `propose_parameter_change` receives a strictly higher proposal ID — the
/// counter never resets or wraps back to an already-used ID.
#[test]
fn test_proposal_ids_are_strictly_monotonically_increasing() {
    let (env, client, admin, _service) = setup();
    let value = encode_u64(&env, MIN_COOLDOWN_SECS);

    let id1 = client.propose_parameter_change(
        &admin_signers(&env, &admin),
        &param_key_cooldown(),
        &value,
    );

    advance(&env, DEFAULT_UPGRADE_DELAY_SECS);
    client.execute_parameter_change(&admin_signers(&env, &admin), &id1);

    // Propose a second change with identical parameters.
    let id2 = client.propose_parameter_change(
        &admin_signers(&env, &admin),
        &param_key_cooldown(),
        &value,
    );

    assert!(id2 > id1, "Proposal IDs must be strictly monotonically increasing; got id1={id1}, id2={id2}");
}

// ── UpgradeProposal: co-signature accumulator cleared after quorum ────────────

/// After a 2-of-3 upgrade proposal accumulates quorum and the proposal is
/// stored, the approval accumulator is cleared.  Replaying the same two
/// signers at a later ledger sequence starts a fresh accumulation cycle — it
/// does not immediately propose a second upgrade without reaching quorum again.
#[test]
fn test_upgrade_cosig_accumulator_cleared_after_quorum() {
    let (env, client, a1, a2, _a3, _service) = setup_multisig();
    let hash = dummy_hash(&env);

    // First invocation: 1-of-2 — accumulates but doesn't store.
    let sig1 = Vec::from_array(&env, [a1.clone()]);
    client.propose_upgrade(&sig1, &hash);
    assert_eq!(client.get_upgrade_approval_count(), 1);

    // Second invocation: quorum met — proposal stored, accumulator cleared.
    let sig2 = Vec::from_array(&env, [a2.clone()]);
    client.propose_upgrade(&sig2, &hash);
    assert_eq!(client.get_upgrade_approval_count(), 0, "Accumulator must be cleared after quorum");

    let proposal = client.get_pending_upgrade();
    assert_eq!(proposal.new_wasm_hash, hash);

    // Veto the pending upgrade so we can test a second proposal cycle.
    client.veto_upgrade(&Vec::new(&env));

    // Advance past the veto's observable ledger position (new sequence N+k).
    advance(&env, DEFAULT_UPGRADE_DELAY_SECS);

    // Replaying the same two signers at the new sequence starts fresh — it
    // must accumulate to 1 before a second proposal is stored.
    let hash2 = BytesN::from_array(&env, &[0xbbu8; 32]);
    client.propose_upgrade(&sig1, &hash2);
    assert_eq!(
        client.get_upgrade_approval_count(), 1,
        "Replaying at a later sequence must restart accumulation, not skip to quorum"
    );
    assert!(
        client.try_get_pending_upgrade().is_err(),
        "Second proposal must not be stored until quorum is reached again"
    );
}

/// Once an upgrade is executed, attempting to execute the same (now-cleared)
/// pending upgrade slot again must fail with `NoPendingUpgrade`.
#[test]
fn test_executed_upgrade_slot_is_cleared_preventing_replay() {
    let (env, client, a1, a2, _a3, _service) = setup_multisig();
    let hash = dummy_hash(&env);

    // Reach quorum and store the proposal.
    client.propose_upgrade(&Vec::from_array(&env, [a1.clone()]), &hash);
    client.propose_upgrade(&Vec::from_array(&env, [a2.clone()]), &hash);

    // Advance past the time-lock and execute.
    advance(&env, DEFAULT_UPGRADE_DELAY_SECS + 1);
    // Note: execute_upgrade actually upgrades the WASM; in the test
    // environment the host allows it without a real new WASM, so we just
    // verify the storage-level replay guard.
    let _ = client.try_execute_upgrade(&Vec::new(&env));
    // Whether execute succeeds or fails (test env WASM constraints), the
    // slot must be cleared or the proposal must no longer be pending.
    // Either NoPendingUpgrade (cleared) or the upgrade actually succeeded.
    let second_attempt = client.try_execute_upgrade(&Vec::new(&env));
    assert_eq!(
        second_attempt,
        Err(Ok(Error::NoPendingUpgrade)),
        "Second execute_upgrade call must fail: proposal slot cleared after first execution"
    );
}

// ── ParameterProposal: multisig co-signature replay across sequences ──────────

/// In 2-of-3 multisig mode, after a parameter proposal has been executed
/// (at ledger sequence N), re-submitting the identical co-signatures at a
/// later sequence (N+k) must not create or execute a new proposal without
/// going through the full `propose_parameter_change` flow again.
#[test]
fn test_multisig_parameter_cosig_does_not_replay_as_new_proposal() {
    let (env, client, a1, a2, _a3, _service) = setup_multisig();
    let value = encode_u64(&env, MIN_COOLDOWN_SECS);

    // Propose (admin is in the multisig set so Vec::new delegates to legacy
    // path — use two_signers to drive the multisig path).
    let signers_12 = two_signers(&env, &a1, &a2);

    let proposal_id =
        client.propose_parameter_change(&signers_12, &param_key_cooldown(), &value);

    // Execute at ledger N + time_lock.
    advance(&env, DEFAULT_UPGRADE_DELAY_SECS);
    client.execute_parameter_change(&signers_12, &proposal_id);

    let executed = client.get_parameter_proposal(&proposal_id);
    assert_eq!(executed.status, ParameterProposalStatus::Executed);

    // Advance to a new ledger sequence N+k.
    advance(&env, DEFAULT_UPGRADE_DELAY_SECS);

    // Re-executing the same proposal ID with the same co-signers must fail.
    let replay_result =
        client.try_execute_parameter_change(&signers_12, &proposal_id);
    assert_eq!(
        replay_result,
        Err(Ok(Error::ParameterProposalAlreadyExecuted)),
        "Replay of co-signed execute at ledger N+k must be rejected"
    );
}

// ── PolicyBundleProposal: post-execution slot is empty ────────────────────────

/// After a PolicyBundleProposal is applied, the storage slot is cleared.
/// Re-applying the proposal at a later ledger sequence (simulating a replay)
/// must fail with `NoPendingPolicyBundle` (the slot is absent).
#[test]
fn test_policy_bundle_proposal_slot_cleared_after_apply() {
    let (env, client, admin, _service) = setup();

    // Propose a policy bundle.
    client.propose_policy_bundle(
        &admin_signers(&env, &admin),
        &80u32,    // risk_threshold
        &7_200u64, // cooldown_secs
    );

    // Advance past the policy-bundle time-lock (DEFAULT_PARAM_CHANGE_DELAY_SECS).
    advance(&env, DEFAULT_PARAM_CHANGE_DELAY_SECS + 1);
    client.apply_policy_bundle();

    // Attempting to apply again at a later sequence must fail — the slot is gone.
    advance(&env, DEFAULT_PARAM_CHANGE_DELAY_SECS);
    let replay_result = client.try_apply_policy_bundle();
    assert_eq!(
        replay_result,
        Err(Ok(Error::NoPendingUpgrade)),
        "Replay of apply_policy_bundle after slot cleared must be rejected"
    );
}
