//! Builds account proof inputs from the generated EVM workloads.
//!
//! The update covers the workload's one chunk, its deposits arrive as inbox
//! messages in the same update, and its DA is posted to L1. The input is
//! assembled the way the sequencer's account prover does it
//! (`AcctSpec::fetch_input` in alpen-client).

use alloy_consensus::Header;
use alpen_acct_runtime::{ChunkInput, EePrivateInput};
use alpen_acct_types::{DEPOSIT_MSG_TYPE_ID, DepositMsgData, EeAccountState, UpdateExtraData};
use alpen_chain_types::{ChunkTransition, SubjectDepositData};
use alpen_da_runtime::test_utils::build_batch_da;
use alpen_params::AlpenSpecId;
use alpen_test_utils_evm_workload::Workload;
use ssz::Encode;
use strata_acct_types::{BRIDGE_GATEWAY_ACCT_ID, Hash, MessageEntry, MsgPayload};
use strata_codec::encode_to_vec;
use strata_msg_fmt::{Msg as _, OwnedMsg};
use strata_snark_acct_runtime::{Coinput, IInnerState, PrivateInput as UpdatePrivateInput};
use strata_snark_acct_types::{
    OutputMessage, OutputTransfer, ProofState, Seqno, UpdateOutputs, UpdateProofPubParams,
};

use crate::EeAcctProofInput;

/// Sequence number of the update. The DA blob carries it too.
const UPDATE_SEQ_NO: u64 = 1;

/// OL epoch the deposit messages were included in.
const DEPOSIT_EPOCH: u32 = 1;

/// Builds the update over `workload`'s one chunk, proven by `transition` and
/// `chunk_proof`, under `spec_version`.
pub fn build_workload_update(
    workload: &Workload,
    transition: &ChunkTransition,
    chunk_proof: Vec<u8>,
    spec_version: AlpenSpecId,
) -> EeAcctProofInput {
    let prev_header: Header =
        alloy_rlp::decode_exact(&workload.prev_header_rlp[..]).expect("decode prev header");
    let pre_state = EeAccountState::new(
        transition.parent_exec_blkid(),
        Hash::from(prev_header.state_root.0),
        Vec::new(),
        Vec::new(),
    );

    // Every deposit the chunk mints arrives in this update, so the update
    // queues each as a pending input and the chunk consumes it.
    let messages: Vec<MessageEntry> = transition
        .inputs()
        .subject_deposits()
        .iter()
        .map(deposit_message)
        .collect();
    let post_state = EeAccountState::new(
        transition.tip_exec_blkid(),
        transition.tip_state_root(),
        Vec::new(),
        Vec::new(),
    );
    let extra_data = UpdateExtraData::new(
        transition.tip_exec_blkid(),
        transition.tip_state_root(),
        messages.len() as u32,
        0,
    );

    let mut outputs = UpdateOutputs::new_empty();
    outputs
        .try_extend_transfers(
            transition
                .outputs()
                .output_transfers()
                .iter()
                .map(|transfer| OutputTransfer::new(transfer.dest(), transfer.value())),
        )
        .expect("transfers fit the update");
    outputs
        .try_extend_messages(
            transition
                .outputs()
                .output_messages()
                .iter()
                .map(|message| OutputMessage::new(message.dest(), message.payload().clone())),
        )
        .expect("messages fit the update");

    let da = build_batch_da(workload, transition, UPDATE_SEQ_NO, spec_version);
    let message_count = messages.len() as u64;
    let pub_params = UpdateProofPubParams::new(
        Seqno::new(UPDATE_SEQ_NO),
        ProofState::new(pre_state.compute_state_root(), 0),
        ProofState::new(post_state.compute_state_root(), message_count),
        messages,
        da.ledger_refs,
        outputs,
        encode_to_vec(&extra_data).expect("encode extra data"),
    );
    // The EE program takes no message coinputs.
    let coinputs = pub_params
        .message_inputs()
        .iter()
        .map(|_| Coinput::new(Vec::new()))
        .collect();

    EeAcctProofInput::V2 {
        ee_private_input: EePrivateInput::new(
            workload.range_pre_state.clone(),
            vec![ChunkInput::new(transition.clone(), chunk_proof)],
        ),
        snark_acct_private_input: UpdatePrivateInput::new(
            pub_params,
            pre_state.as_ssz_bytes(),
            coinputs,
        ),
        da_witness: da.witness,
    }
}

/// The bridge's inbox message for `deposit`.
fn deposit_message(deposit: &SubjectDepositData) -> MessageEntry {
    let body = encode_to_vec(&DepositMsgData::new(deposit.dest())).expect("encode deposit");
    let message = OwnedMsg::new(DEPOSIT_MSG_TYPE_ID, body).expect("deposit message fits");
    let payload = MsgPayload::from_bytes(deposit.value(), message.to_vec()).expect("payload fits");
    MessageEntry::new(BRIDGE_GATEWAY_ACCT_ID, DEPOSIT_EPOCH, payload)
}
