//! Perf input for the alpen-acct SP1 guest.
//!
//! Proves one account update over the checked-in mixed workload. The batch is
//! the workload's one chunk, its deposits arrive as inbox messages in the same
//! update, and its DA is posted to L1. The update carries the chunk proof that
//! `--generate-proof` saved, so under SP1 the guest runs a real Groth16 verify.
//! The input is assembled the way the sequencer's account prover does it
//! (`AcctSpec::fetch_input` in alpen-client).

use alloy_consensus::Header;
use alpen_acct_runtime::{ChunkInput, EePrivateInput};
use alpen_acct_types::{DepositMsgData, EeAccountState, UpdateExtraData, DEPOSIT_MSG_TYPE_ID};
use alpen_chain_types::{ChunkTransition, SubjectDepositData};
use alpen_proof_acct::{EeAcctProgram, EeAcctProofInput};
use alpen_test_utils_evm_workload::Workload;
use ssz::{Decode, Encode};
use strata_acct_types::{Hash, MessageEntry, MsgPayload, BRIDGE_GATEWAY_ACCT_ID};
use strata_codec::encode_to_vec;
use strata_msg_fmt::{Msg as _, OwnedMsg};
use strata_snark_acct_runtime::{Coinput, IInnerState, PrivateInput as UpdatePrivateInput};
use strata_snark_acct_types::{
    OutputMessage, OutputTransfer, ProofState, Seqno, UpdateOutputs, UpdateProofPubParams,
};
use tracing::info;
use zkaleido::{ExecutionSummary, ProofReceiptWithMetadata, ZkVmHost, ZkVmProgram};

use super::{
    alpen_chunk::{build_chunk, load_workload},
    da::build_batch_da,
};

/// Sequence number of the update. The DA blob carries it too.
const UPDATE_SEQ_NO: u64 = 1;

/// OL epoch the deposit messages were included in.
const DEPOSIT_EPOCH: u32 = 1;

/// Builds the update over `workload`'s chunk, carrying `chunk_proof`.
///
/// Panics if `chunk_proof` proves a different chunk than the workload's, which
/// means one of the two was regenerated without the other.
pub(super) fn prepare_input(
    workload: &Workload,
    chunk_proof: &ProofReceiptWithMetadata,
) -> EeAcctProofInput {
    let chunk = build_chunk(workload);
    let proven = ChunkTransition::from_ssz_bytes(chunk_proof.receipt().public_values().as_bytes())
        .expect("chunk proof public values are a chunk transition");
    assert_eq!(
        proven, chunk.transition,
        "the chunk proof is for a different chunk than the workload; regenerate it with \
         `just prover-proof`"
    );
    build_input(
        workload,
        &chunk.transition,
        chunk_proof.receipt().proof().as_bytes().to_vec(),
    )
}

fn build_input(
    workload: &Workload,
    transition: &ChunkTransition,
    chunk_proof: Vec<u8>,
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

    let da = build_batch_da(workload, transition, UPDATE_SEQ_NO);
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

    EeAcctProofInput {
        ee_private_input: EePrivateInput::new(
            Vec::new(),
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

pub(crate) fn gen_perf_report(
    host: &impl ZkVmHost,
    chunk_proof: &ProofReceiptWithMetadata,
) -> (String, ExecutionSummary) {
    info!("Generating execution summary for Alpen Acct");
    let input = prepare_input(&load_workload(), chunk_proof);
    let summary =
        <EeAcctProgram as ZkVmProgram>::execute(&input, host).expect("alpen-acct execution");
    (EeAcctProgram::name(), summary)
}

pub(crate) fn gen_proof(
    host: &impl ZkVmHost,
    chunk_proof: &ProofReceiptWithMetadata,
) -> ProofReceiptWithMetadata {
    info!("Generating proof for Alpen Acct");
    let input = prepare_input(&load_workload(), chunk_proof);
    <EeAcctProgram as ZkVmProgram>::prove(&input, host).expect("alpen-acct proof")
}

#[cfg(test)]
mod tests {
    use alpen_proof_chunk::EeChunkProgram;

    use super::*;
    use crate::programs::alpen_chunk::{perf_alpen_params, PERF_SPEC_VERSION};

    #[test]
    fn test_alpen_acct_native_execution() {
        let workload = load_workload();

        // Natively the chunk proof is a Schnorr signature over the transition,
        // checked against the chunk program's test key.
        let chunk_program = EeChunkProgram::new(perf_alpen_params(), PERF_SPEC_VERSION);
        let chunk = build_chunk(&workload);
        let chunk_proof =
            <EeChunkProgram as ZkVmProgram>::prove(&chunk.input, &chunk_program.native_host())
                .expect("native chunk proof");

        let input = prepare_input(&workload, &chunk_proof);
        let program = EeAcctProgram::new(
            EeChunkProgram::test_predicate_key(),
            perf_alpen_params(),
            PERF_SPEC_VERSION,
        );
        let output = program.execute(&input).expect("native execution");

        assert_eq!(
            output.new_state().next_inbox_msg_idx(),
            chunk.transition.inputs().subject_deposits().len() as u64
        );
        assert!(!output.outputs().messages().is_empty());
    }
}
