//! Tests for invalid update conditions and error handling.
//!
//! These tests verify that the verification logic correctly rejects
//! malformed or invalid updates.

#![expect(unused_crate_dependencies, reason = "test dependencies")]

mod common;

use std::iter;

use alpen_acct_runtime::{
    AcctRuntimeError, BuilderError, ChunkChain, EeVerificationInput, UpdateBuilder,
};
use alpen_acct_types::{PendingInputEntry, UpdateExtraData};
use alpen_chain_types::{ExecInputs, ExecOutputs, SubjectDepositData};
use common::{
    build_update_operation, create_chunk_transition, create_deposit_message, create_initial_state,
    empty_exec_header_summary, simple_chunk, verify_update, verify_with_private_input,
};
use ssz::Encode;
use strata_acct_types::{AccountId, BitcoinAmount, Hash, SubjectId};
use strata_codec::encode_to_vec;
use strata_predicate::PredicateKey;
use strata_snark_acct_runtime::{
    Coinput, IInnerState, PrivateInput as SnarkPrivateInput, ProgramError,
};
use strata_snark_acct_types::{
    LedgerRefs, OutputTransfer, ProofState, Seqno, UpdateOperationData, UpdateOutputs,
    UpdateProofPubParams,
};

// ---- Coinput validation ----

#[test]
fn test_mismatched_coinput_count() {
    let (initial_state, snark_state) = create_initial_state();

    let dest = SubjectId::from([1u8; 32]);
    let value = BitcoinAmount::try_from(1000u64).unwrap();
    let source = AccountId::from([2u8; 32]);
    let message = create_deposit_message(dest, value, source, 1);

    let (operation, _coinputs, _snark_priv) =
        build_update_operation(vec![message], &[], &initial_state, &snark_state);

    // Too many coinputs
    let wrong_coinputs = [vec![], vec![]];
    let result = verify_update(&initial_state, &operation, &wrong_coinputs);
    assert!(matches!(
        result,
        Err(ProgramError::MismatchedCoinputCount {
            expected: 1,
            actual: 2,
        })
    ));

    // Too few coinputs
    let wrong_coinputs2: Vec<Vec<u8>> = vec![];
    let result2 = verify_update(&initial_state, &operation, &wrong_coinputs2);
    assert!(matches!(
        result2,
        Err(ProgramError::MismatchedCoinputCount {
            expected: 1,
            actual: 0,
        })
    ));
}

#[test]
fn test_nonempty_coinput_rejected() {
    let (initial_state, snark_state) = create_initial_state();

    let dest = SubjectId::from([1u8; 32]);
    let value = BitcoinAmount::try_from(1000u64).unwrap();
    let source = AccountId::from([2u8; 32]);
    let message = create_deposit_message(dest, value, source, 1);

    let (operation, _coinputs, _snark_priv) =
        build_update_operation(vec![message], &[], &initial_state, &snark_state);

    // Non-empty coinput should be rejected (EE requires empty coinputs)
    let nonempty_coinputs = vec![vec![1, 2, 3]];
    let result = verify_update(&initial_state, &operation, &nonempty_coinputs);
    assert!(
        matches!(
            result,
            Err(ProgramError::AtMessage { idx: 0, ref inner })
            if matches!(**inner, ProgramError::MalformedCoinput)
        ),
        "expected MalformedCoinput at message 0, got: {result:?}"
    );
}

// ---- Output / extra data validation (constructed manually to bypass builder validation) ----

#[test]
fn test_output_mismatch_fails() {
    let (initial_state, _snark_state) = create_initial_state();

    // Manually construct operation data with outputs that don't match
    // accumulated chunks (there are none, so expected should be empty).
    let mut wrong_outputs = UpdateOutputs::new_empty();
    wrong_outputs
        .try_extend_transfers(iter::once(OutputTransfer::new(
            AccountId::from([99u8; 32]),
            BitcoinAmount::try_from(500u64).unwrap(),
        )))
        .unwrap();

    let extra_data = UpdateExtraData::new(
        initial_state.last_exec_blkid(),
        initial_state.last_exec_state_root(),
        0,
        0,
    );
    let extra_data_buf = encode_to_vec(&extra_data).unwrap();

    let operation = UpdateOperationData::new(
        1,
        ProofState::new(Hash::default(), 0),
        vec![],
        LedgerRefs::new_empty(),
        wrong_outputs,
        extra_data_buf,
    );

    let coinputs: Vec<Vec<u8>> = vec![];
    let result = verify_update(&initial_state, &operation, &coinputs);

    assert!(
        matches!(
            result,
            Err(ProgramError::Internal(
                AcctRuntimeError::OutputsMismatch { .. }
            ))
        ),
        "expected OutputsMismatch, got: {result:?}"
    );
}

#[test]
fn test_extra_data_tip_mismatch() {
    let (initial_state, _snark_state) = create_initial_state();

    // Manually construct operation data with a wrong tip.
    let wrong_tip = Hash::new([0xAA; 32]);
    let extra_data = UpdateExtraData::new(wrong_tip, initial_state.last_exec_state_root(), 0, 0);
    let extra_data_buf = encode_to_vec(&extra_data).unwrap();

    let operation = UpdateOperationData::new(
        1,
        ProofState::new(Hash::default(), 0),
        vec![],
        LedgerRefs::new_empty(),
        UpdateOutputs::new_empty(),
        extra_data_buf,
    );

    let coinputs: Vec<Vec<u8>> = vec![];
    let result = verify_update(&initial_state, &operation, &coinputs);

    assert!(
        matches!(
            result,
            Err(ProgramError::Internal(
                AcctRuntimeError::ExtraDataMismatch { .. }
            ))
        ),
        "expected ExtraDataMismatch, got: {result:?}"
    );
}

/// Builds an update with no messages, no outputs and the given extra data.
fn operation_with_extra_data(extra_data: &UpdateExtraData) -> UpdateOperationData {
    UpdateOperationData::new(
        1,
        ProofState::new(Hash::default(), 0),
        vec![],
        LedgerRefs::new_empty(),
        UpdateOutputs::new_empty(),
        encode_to_vec(extra_data).unwrap(),
    )
}

#[test]
fn test_extra_data_claimed_fincls_rejected() {
    let (initial_state, _snark_state) = create_initial_state();

    // No chunk processes forced inclusions, so claiming one must fail. The
    // queue is empty, so removing it would otherwise do nothing and pass.
    let extra_data = UpdateExtraData::new(
        initial_state.last_exec_blkid(),
        initial_state.last_exec_state_root(),
        0,
        1,
    );
    let operation = operation_with_extra_data(&extra_data);

    let result = verify_update(&initial_state, &operation, &[]);
    assert!(
        matches!(
            result,
            Err(ProgramError::Internal(
                AcctRuntimeError::ExtraDataMismatch { .. }
            ))
        ),
        "expected ExtraDataMismatch, got: {result:?}"
    );
}

#[test]
fn test_extra_data_processed_inputs_mismatch() {
    let (initial_state, _snark_state) = create_initial_state();

    // No chunk consumes an input, so claiming one must fail.
    let extra_data = UpdateExtraData::new(
        initial_state.last_exec_blkid(),
        initial_state.last_exec_state_root(),
        1,
        0,
    );
    let operation = operation_with_extra_data(&extra_data);

    let result = verify_update(&initial_state, &operation, &[]);
    assert!(
        matches!(
            result,
            Err(ProgramError::Internal(
                AcctRuntimeError::ExtraDataMismatch { .. }
            ))
        ),
        "expected ExtraDataMismatch, got: {result:?}"
    );
}

/// A deposit that arrives with the pending input queue full can't be queued.
/// The verifier has to reject the update, so no proof can drop the deposit.
#[test]
fn test_deposit_into_full_queue_rejects_update() {
    let (mut initial_state, _snark_state) = create_initial_state();
    let filler = PendingInputEntry::Deposit(SubjectDepositData::new(
        SubjectId::from([9u8; 32]),
        BitcoinAmount::try_from(1u64).unwrap(),
    ));
    while initial_state.add_pending_input(filler.clone()) {}

    let message = create_deposit_message(
        SubjectId::from([1u8; 32]),
        BitcoinAmount::try_from(1000u64).unwrap(),
        AccountId::from([2u8; 32]),
        1,
    );

    // No post state exists, so the claimed one is arbitrary. Message
    // processing fails before it is checked.
    let extra_data = UpdateExtraData::new(
        initial_state.last_exec_blkid(),
        initial_state.last_exec_state_root(),
        0,
        0,
    );
    let pub_params = UpdateProofPubParams::new(
        Seqno::new(0),
        ProofState::new(initial_state.compute_state_root(), 0),
        ProofState::new(Hash::zero(), 1),
        vec![message],
        LedgerRefs::new_empty(),
        UpdateOutputs::new_empty(),
        encode_to_vec(&extra_data).unwrap(),
    );
    let snark_priv = SnarkPrivateInput::new(
        pub_params,
        initial_state.as_ssz_bytes(),
        vec![Coinput::new(Vec::new())],
    );

    let result = verify_with_private_input(&snark_priv, &[]);
    assert!(
        matches!(
            result,
            Err(ProgramError::AtMessage { idx: 0, ref inner })
                if matches!(**inner, ProgramError::Internal(AcctRuntimeError::PendingInputsFull))
        ),
        "expected PendingInputsFull at message 0, got: {result:?}"
    );
}

// ---- Builder chunk validation ----

#[test]
fn test_builder_rejects_wrong_parent() {
    let (initial_state, snark_state) = create_initial_state();

    let predicate_key = PredicateKey::always_accept();
    let vinput = EeVerificationInput::new(&predicate_key, &[]);
    let mut builder =
        UpdateBuilder::new(snark_state, initial_state, vinput).expect("create builder");

    // Chunk with wrong parent
    let wrong_parent = Hash::new([0xCC; 32]);
    let tip = Hash::new([0xDD; 32]);
    let chunk = simple_chunk(
        wrong_parent,
        tip,
        Hash::zero(),
        empty_exec_header_summary(),
        vec![],
        ExecOutputs::new_empty(),
    );

    let result = builder.accept_chunk_transition(&chunk);
    assert!(
        matches!(
            result,
            Err(BuilderError::Runtime(AcctRuntimeError::ChainLinkage { .. }))
        ),
        "expected ChainLinkage, got: {result:?}"
    );
}

#[test]
fn test_builder_rejects_wrong_deposit() {
    let (initial_state, snark_state) = create_initial_state();

    let dest = SubjectId::from([1u8; 32]);
    let value = BitcoinAmount::try_from(1000u64).unwrap();
    let source = AccountId::from([2u8; 32]);
    let message = create_deposit_message(dest, value, source, 1);

    let predicate_key = PredicateKey::always_accept();
    let vinput = EeVerificationInput::new(&predicate_key, &[]);
    let mut builder =
        UpdateBuilder::new(snark_state, initial_state, vinput).expect("create builder");

    builder.add_messages(vec![message]).expect("add messages");

    assert_eq!(builder.remaining_input_count(), 1);

    // Chunk with a different deposit than what's pending
    let wrong_dest = SubjectId::from([99u8; 32]);
    let wrong_deposit = SubjectDepositData::new(wrong_dest, value);
    let chunk = simple_chunk(
        builder.cur_tip_blkid(),
        Hash::new([0xEE; 32]),
        Hash::zero(),
        empty_exec_header_summary(),
        vec![wrong_deposit],
        ExecOutputs::new_empty(),
    );

    let result = builder.accept_chunk_transition(&chunk);
    assert!(
        matches!(
            result,
            Err(BuilderError::Runtime(AcctRuntimeError::DepositMismatch {
                position: 0
            }))
        ),
        "expected DepositMismatch, got: {result:?}"
    );
}

#[test]
fn test_builder_advances_tip() {
    let (initial_state, snark_state) = create_initial_state();

    let dest1 = SubjectId::from([1u8; 32]);
    let dest2 = SubjectId::from([2u8; 32]);
    let value = BitcoinAmount::try_from(500u64).unwrap();
    let source = AccountId::from([3u8; 32]);

    let msg1 = create_deposit_message(dest1, value, source, 1);
    let msg2 = create_deposit_message(dest2, value, source, 1);

    let predicate_key = PredicateKey::always_accept();
    let vinput = EeVerificationInput::new(&predicate_key, &[]);
    let mut builder =
        UpdateBuilder::new(snark_state, initial_state, vinput).expect("create builder");

    builder
        .add_messages(vec![msg1, msg2])
        .expect("add messages");

    let initial_tip = builder.cur_tip_blkid();

    // First chunk
    let d1 = match &builder.remaining_pending_inputs()[0] {
        PendingInputEntry::Deposit(d) => d.clone(),
        other => panic!("expected a deposit, got {other:?}"),
    };
    let tip1 = Hash::new([0xAA; 32]);
    let chunk1 = simple_chunk(
        initial_tip,
        tip1,
        Hash::zero(),
        empty_exec_header_summary(),
        vec![d1],
        ExecOutputs::new_empty(),
    );
    builder.accept_chunk_transition(&chunk1).expect("chunk 1");

    assert_eq!(builder.cur_tip_blkid(), tip1);
    assert_ne!(builder.cur_tip_blkid(), initial_tip);
    assert_eq!(builder.remaining_input_count(), 1);

    // Second chunk — parent must be tip1
    let d2 = match &builder.remaining_pending_inputs()[0] {
        PendingInputEntry::Deposit(d) => d.clone(),
        other => panic!("expected a deposit, got {other:?}"),
    };
    let tip2 = Hash::new([0xBB; 32]);
    let chunk2 = simple_chunk(
        tip1,
        tip2,
        Hash::zero(),
        empty_exec_header_summary(),
        vec![d2],
        ExecOutputs::new_empty(),
    );
    builder.accept_chunk_transition(&chunk2).expect("chunk 2");

    assert_eq!(builder.cur_tip_blkid(), tip2);
    assert_eq!(builder.remaining_input_count(), 0);
}

// ---- process_decoded_transition tests (verification-side) ----

#[test]
fn test_process_decoded_transition_happy_path() {
    let (initial_state, _snark_state) = create_initial_state();

    let parent = initial_state.last_exec_blkid();
    let tip = Hash::new([0xBB; 32]);

    let transition = create_chunk_transition(
        parent,
        tip,
        Hash::zero(),
        empty_exec_header_summary(),
        ExecInputs::new_empty(),
        ExecOutputs::new_empty(),
    );
    let mut chain = ChunkChain::new(&initial_state);

    let pending: Vec<PendingInputEntry> = vec![];

    let result = chain.apply(&transition, &pending);
    assert!(result.is_ok(), "happy path should succeed: {result:?}");
    assert_eq!(chain.tip_blkid(), tip);
}

#[test]
fn test_chain_linkage_mismatch() {
    let (initial_state, _snark_state) = create_initial_state();

    let wrong_parent = Hash::new([0xCC; 32]);
    let tip = Hash::new([0xDD; 32]);

    let transition = create_chunk_transition(
        wrong_parent,
        tip,
        Hash::zero(),
        empty_exec_header_summary(),
        ExecInputs::new_empty(),
        ExecOutputs::new_empty(),
    );
    let mut chain = ChunkChain::new(&initial_state);

    let pending: Vec<PendingInputEntry> = vec![];

    let result = chain.apply(&transition, &pending);
    assert!(
        matches!(
            result,
            Err(AcctRuntimeError::ChainLinkage { tip, parent })
                if tip == initial_state.last_exec_blkid() && parent == wrong_parent
        ),
        "expected ChainLinkage, got: {result:?}"
    );
}

#[test]
fn test_deposit_mismatch_in_chunk() {
    let dest = SubjectId::from([1u8; 32]);
    let value = BitcoinAmount::try_from(1000u64).unwrap();
    let deposit = SubjectDepositData::new(dest, value);

    let initial_state = alpen_acct_types::EeAccountState::new(
        Hash::new([0u8; 32]),
        Hash::zero(),
        vec![PendingInputEntry::Deposit(deposit)],
        Vec::new(),
    );

    let parent = initial_state.last_exec_blkid();
    let tip = Hash::new([0xEE; 32]);

    let wrong_dest = SubjectId::from([2u8; 32]);
    let wrong_deposit = SubjectDepositData::new(wrong_dest, value);
    let mut inputs = ExecInputs::new_empty();
    inputs.add_subject_deposit(wrong_deposit);

    let transition = create_chunk_transition(
        parent,
        tip,
        Hash::zero(),
        empty_exec_header_summary(),
        inputs,
        ExecOutputs::new_empty(),
    );
    let mut chain = ChunkChain::new(&initial_state);

    let pending = initial_state.pending_inputs().to_vec();

    let result = chain.apply(&transition, &pending);
    assert!(
        matches!(
            result,
            Err(AcctRuntimeError::DepositMismatch { position: 0 })
        ),
        "expected DepositMismatch, got: {result:?}"
    );
}

#[test]
fn test_input_count_mismatch() {
    let initial_state = alpen_acct_types::EeAccountState::new(
        Hash::new([0u8; 32]),
        Hash::zero(),
        Vec::new(),
        Vec::new(),
    );

    let parent = initial_state.last_exec_blkid();
    let tip = Hash::new([0xFF; 32]);

    let dest = SubjectId::from([1u8; 32]);
    let value = BitcoinAmount::try_from(1000u64).unwrap();
    let deposit = SubjectDepositData::new(dest, value);
    let mut inputs = ExecInputs::new_empty();
    inputs.add_subject_deposit(deposit);

    let transition = create_chunk_transition(
        parent,
        tip,
        Hash::zero(),
        empty_exec_header_summary(),
        inputs,
        ExecOutputs::new_empty(),
    );
    let mut chain = ChunkChain::new(&initial_state);

    let pending: Vec<PendingInputEntry> = vec![];

    let result = chain.apply(&transition, &pending);
    assert!(
        matches!(
            result,
            Err(AcctRuntimeError::DepositMismatch { position: 0 })
        ),
        "expected DepositMismatch, got: {result:?}"
    );
}

#[test]
fn test_deposit_mismatch_reports_queue_position() {
    let value = BitcoinAmount::try_from(1000u64).unwrap();
    let first = SubjectDepositData::new(SubjectId::from([1u8; 32]), value);
    let second = SubjectDepositData::new(SubjectId::from([2u8; 32]), value);
    let pending = vec![
        PendingInputEntry::Deposit(first.clone()),
        PendingInputEntry::Deposit(second),
    ];
    let initial_state = alpen_acct_types::EeAccountState::new(
        Hash::new([0u8; 32]),
        Hash::zero(),
        pending.clone(),
        Vec::new(),
    );
    let mut chain = ChunkChain::new(&initial_state);

    let tip1 = Hash::new([0xA1; 32]);
    let chunk1 = simple_chunk(
        initial_state.last_exec_blkid(),
        tip1,
        Hash::zero(),
        empty_exec_header_summary(),
        vec![first.clone()],
        ExecOutputs::new_empty(),
    );
    chain
        .apply(&chunk1, &pending)
        .expect("first deposit matches");

    // The second chunk repeats the first deposit, so it breaks at the second
    // queue entry, not at the first entry of its own deposits.
    let chunk2 = simple_chunk(
        tip1,
        Hash::new([0xA2; 32]),
        Hash::zero(),
        empty_exec_header_summary(),
        vec![first],
        ExecOutputs::new_empty(),
    );
    let result = chain.apply(&chunk2, &pending);
    assert!(
        matches!(
            result,
            Err(AcctRuntimeError::DepositMismatch { position: 1 })
        ),
        "expected DepositMismatch at 1, got: {result:?}"
    );
    assert_eq!(chain.tip_blkid(), tip1, "a rejected chunk leaves the chain");
    assert_eq!(chain.inputs_consumed(), 1);
}
