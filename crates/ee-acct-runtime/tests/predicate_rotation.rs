//! Integration tests for predicate-key rotation flowing through the queued
//! pending-input model: enqueue in true order relative to deposits, consume
//! via a chunk that declares the rotation, and verify both the unconditional
//! and verified update-application paths agree.

#![expect(unused_crate_dependencies, reason = "test dependencies")]

mod common;

use common::{
    apply_unconditionally, assert_verified_chunks_succeed, create_deposit_message,
    create_initial_state, create_predicate_update_message, empty_exec_header_summary, simple_chunk,
};
use ssz::Encode;
use strata_acct_types::{AccountId, BitcoinAmount, Hash, MessageEntry, MsgPayload, SubjectId};
use strata_ee_acct_runtime::{EeVerificationInput, UpdateBuilder};
use strata_ee_acct_types::{PREDICATE_UPDATE_MSG_TYPE_ID, PendingInputEntry};
use strata_ee_chain_types::ExecOutputs;
use strata_msg_fmt::{Msg as MsgTrait, OwnedMsg};
use strata_predicate::PredicateKey;
use strata_simple_ee::SimpleExecutionEnvironment;

#[test]
fn test_deposit_then_rotation_single_chunk() {
    let (initial_state, snark_state) = create_initial_state();
    let ee = SimpleExecutionEnvironment;

    let dest = SubjectId::from([1u8; 32]);
    let value = BitcoinAmount::from(1000u64);
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

#[test]
fn test_non_admin_predicate_update_message_is_ignored() {
    let (initial_state, snark_state) = create_initial_state();
    let ee = SimpleExecutionEnvironment;

    let new_key = PredicateKey::always_accept();
    let raw = OwnedMsg::new(PREDICATE_UPDATE_MSG_TYPE_ID, new_key.as_ssz_bytes())
        .expect("create message");
    let msg = MessageEntry::new(
        AccountId::from([0x99; 32]), // not ADMIN_MSG_ACCT_ID
        1,
        MsgPayload::from_bytes(BitcoinAmount::ZERO, raw.to_vec())
            .expect("message payload bytes must fit within SSZ max length"),
    );

    let predicate_key = PredicateKey::always_accept();
    let vinput = EeVerificationInput::new(&ee, &predicate_key, &[], &[]);
    let mut builder =
        UpdateBuilder::new(snark_state, initial_state, vinput).expect("create builder");

    builder.add_message(msg).expect("add message");

    assert_eq!(
        builder.remaining_input_count(),
        0,
        "a predicate-update message from a non-admin source must never be queued"
    );
}
