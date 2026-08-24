//! Integration tests for predicate-key rotation flowing through the queued
//! pending-input model: enqueue in true order relative to deposits, consume
//! via a chunk that declares the rotation, and verify both the unconditional
//! and verified update-application paths agree.

#![expect(unused_crate_dependencies, reason = "test dependencies")]

mod common;

use common::{
    apply_unconditionally, assert_verified_chunks_succeed, create_deposit_message,
    create_initial_state, create_predicate_update_message, create_vstate,
    empty_exec_header_summary, simple_chunk,
};
use strata_acct_types::{AccountId, BitcoinAmount, Hash, SubjectId};
use strata_ee_acct_runtime::{EeVerificationInput, UpdateBuilder};
use strata_ee_acct_types::{EnvError, PendingInputEntry};
use strata_ee_chain_types::{ExecOutputs, SequenceTracker};
use strata_predicate::{PredicateKey, PredicateTypeId};
use strata_simple_ee::SimpleExecutionEnvironment;
use strata_snark_acct_types::UpdateOutputs;

#[test]
fn test_deposit_then_rotation_single_chunk() {
    let (initial_state, snark_state) = create_initial_state();
    let ee = SimpleExecutionEnvironment;

    let dest = SubjectId::from([1u8; 32]);
    let value = BitcoinAmount::try_from(1000u64).unwrap();
    let source = AccountId::from([2u8; 32]);
    let deposit_msg = create_deposit_message(dest, value, source, 1);

    let new_key = PredicateKey::always_accept();
    let rotation_msg = create_predicate_update_message(&new_key, 1);

    let predicate_key = PredicateKey::always_accept();
    let vinput = EeVerificationInput::new(&ee, &predicate_key, &[], &[]);
    let mut builder =
        UpdateBuilder::new(snark_state, initial_state.clone(), vinput).expect("create builder");

    builder
        .add_messages(vec![deposit_msg, rotation_msg])
        .expect("add messages");

    // Both the deposit and the rotation are queued, in true order.
    assert_eq!(builder.remaining_input_count(), 2);
    let deposit = match &builder.remaining_pending_inputs()[0] {
        PendingInputEntry::Deposit(d) => d.clone(),
        other => panic!("expected a deposit, got {other:?}"),
    };
    assert!(matches!(
        &builder.remaining_pending_inputs()[1],
        PendingInputEntry::PredicateRotation(k) if k == &new_key
    ));

    // A chunk that consumes the deposit and declares the rotation.
    let mut outputs = ExecOutputs::new_empty();
    outputs.set_new_predicate(Some(new_key.clone()));
    let tip = Hash::new([0xAA; 32]);
    let chunk = simple_chunk(
        builder.cur_tip_blkid(),
        tip,
        Hash::zero(),
        empty_exec_header_summary(),
        vec![deposit],
        outputs,
    );

    builder
        .accept_chunk_transition(&chunk)
        .expect("accept chunk should succeed");

    // Both entries — the deposit and the rotation — are consumed together.
    assert_eq!(builder.remaining_input_count(), 0);

    let (operation, coinputs) = builder.build().expect("build should succeed");

    apply_unconditionally(&initial_state, &operation).expect("unconditional path should succeed");
    assert_verified_chunks_succeed(&initial_state, &operation, &coinputs, &[chunk], &ee);
}

#[test]
fn test_declared_rotation_without_queued_entry_fails() {
    let (initial_state, snark_state) = create_initial_state();
    let ee = SimpleExecutionEnvironment;

    let predicate_key = PredicateKey::always_accept();
    let vinput = EeVerificationInput::new(&ee, &predicate_key, &[], &[]);
    let mut builder =
        UpdateBuilder::new(snark_state, initial_state, vinput).expect("create builder");

    // No messages were ever added, so the pending queue is empty — the
    // chunk below claims a rotation that was never queued.
    let mut outputs = ExecOutputs::new_empty();
    outputs.set_new_predicate(Some(PredicateKey::always_accept()));

    let tip = Hash::new([0xBB; 32]);
    let chunk = simple_chunk(
        builder.cur_tip_blkid(),
        tip,
        Hash::zero(),
        empty_exec_header_summary(),
        vec![],
        outputs,
    );

    let result = builder.accept_chunk_transition(&chunk);
    assert!(result.is_err(), "declaring an unqueued rotation must fail");
}

/// Distinct rotation keys, so tests can tell which one an update ended up
/// declaring.
fn rotation_key(tag: u8) -> PredicateKey {
    PredicateKey::try_new(PredicateTypeId::AlwaysAccept, vec![tag])
        .expect("condition fits within the length limit")
}

/// A rotation ends the update. Chaining another transition onto it would put
/// post-rotation blocks in an update proven against the predecessor predicate,
/// which is exactly what the rotation exists to prevent. Host-side sealing
/// stops an honest sequencer from building this, but the verifier has to reject
/// it on its own.
#[test]
fn test_transition_after_a_rotation_is_rejected() {
    let (initial_state, _snark_state) = create_initial_state();
    let ee = SimpleExecutionEnvironment;

    let new_key = rotation_key(0x01);
    let predicate_key = PredicateKey::always_accept();
    let mut vstate = create_vstate(
        &ee,
        &predicate_key,
        &initial_state,
        UpdateOutputs::new_empty(),
    );

    let pending = vec![PendingInputEntry::PredicateRotation(new_key.clone())];
    let mut tracker = SequenceTracker::new(&pending);

    // First transition consumes the queued rotation.
    let tip1 = Hash::new([0xA1; 32]);
    let mut outputs = ExecOutputs::new_empty();
    outputs.set_new_predicate(Some(new_key));
    let rotating = simple_chunk(
        initial_state.last_exec_blkid(),
        tip1,
        Hash::zero(),
        empty_exec_header_summary(),
        vec![],
        outputs,
    );
    vstate
        .process_decoded_transition(&rotating, &mut tracker)
        .expect("the rotating transition itself is fine");

    // A second, otherwise valid transition chains onto it.
    let following = simple_chunk(
        tip1,
        Hash::new([0xA2; 32]),
        Hash::zero(),
        empty_exec_header_summary(),
        vec![],
        ExecOutputs::new_empty(),
    );

    let result = vstate.process_decoded_transition(&following, &mut tracker);
    assert!(
        matches!(result, Err(EnvError::NonTerminalRotation)),
        "expected NonTerminalRotation, got: {result:?}"
    );
}

/// Two queued rotations cannot be drained by one update. Without the guard the
/// second key silently overwrites the first, so the update declares only the
/// last one while the pending queue loses both — the intermediate predicate
/// would never authorize anything.
#[test]
fn test_second_rotation_in_one_update_is_rejected() {
    let (initial_state, _snark_state) = create_initial_state();
    let ee = SimpleExecutionEnvironment;

    let first_key = rotation_key(0x01);
    let second_key = rotation_key(0x02);
    let predicate_key = PredicateKey::always_accept();
    let mut vstate = create_vstate(
        &ee,
        &predicate_key,
        &initial_state,
        UpdateOutputs::new_empty(),
    );

    let pending = vec![
        PendingInputEntry::PredicateRotation(first_key.clone()),
        PendingInputEntry::PredicateRotation(second_key.clone()),
    ];
    let mut tracker = SequenceTracker::new(&pending);

    let tip1 = Hash::new([0xB1; 32]);
    let mut first_outputs = ExecOutputs::new_empty();
    first_outputs.set_new_predicate(Some(first_key));
    let first = simple_chunk(
        initial_state.last_exec_blkid(),
        tip1,
        Hash::zero(),
        empty_exec_header_summary(),
        vec![],
        first_outputs,
    );
    vstate
        .process_decoded_transition(&first, &mut tracker)
        .expect("the first rotation is fine");

    let mut second_outputs = ExecOutputs::new_empty();
    second_outputs.set_new_predicate(Some(second_key));
    let second = simple_chunk(
        tip1,
        Hash::new([0xB2; 32]),
        Hash::zero(),
        empty_exec_header_summary(),
        vec![],
        second_outputs,
    );

    let result = vstate.process_decoded_transition(&second, &mut tracker);
    assert!(
        matches!(result, Err(EnvError::NonTerminalRotation)),
        "expected NonTerminalRotation, got: {result:?}"
    );
}

/// The builder enforces the same rule, so an honest sequencer finds out while
/// building rather than producing an update no verifier will accept.
#[test]
fn test_builder_rejects_a_chunk_after_a_rotation() {
    let (initial_state, snark_state) = create_initial_state();
    let ee = SimpleExecutionEnvironment;

    let new_key = rotation_key(0x01);
    let rotation_msg = create_predicate_update_message(&new_key, 1);

    let predicate_key = PredicateKey::always_accept();
    let vinput = EeVerificationInput::new(&ee, &predicate_key, &[], &[]);
    let mut builder =
        UpdateBuilder::new(snark_state, initial_state, vinput).expect("create builder");

    builder
        .add_messages(vec![rotation_msg])
        .expect("add messages");

    let tip1 = Hash::new([0xC1; 32]);
    let mut outputs = ExecOutputs::new_empty();
    outputs.set_new_predicate(Some(new_key));
    let rotating = simple_chunk(
        builder.cur_tip_blkid(),
        tip1,
        Hash::zero(),
        empty_exec_header_summary(),
        vec![],
        outputs,
    );
    builder
        .accept_chunk_transition(&rotating)
        .expect("the rotating chunk itself is fine");

    let following = simple_chunk(
        tip1,
        Hash::new([0xC2; 32]),
        Hash::zero(),
        empty_exec_header_summary(),
        vec![],
        ExecOutputs::new_empty(),
    );

    let result = builder.accept_chunk_transition(&following);
    assert!(
        result.is_err(),
        "a chunk after a rotation must not be accepted"
    );
}
