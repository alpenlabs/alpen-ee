//! Perf input for the alpen-chunk SP1 guest.
//!
//! Proves the checked-in mixed workload as one chunk: 100 V1 blocks of
//! transfers, token and pool calls, bridge-outs, precompile calls and
//! deposits. The input is assembled the way the sequencer's chunk prover does
//! it (`ChunkSpec::fetch_input` in alpen-client).

use alloy_consensus::Header;
use alpen_acct_types::{ExecBlock, ExecHeader, ExecPayload, ExecutionEnvironment};
use alpen_chain_types::{
    ChunkTransition, ExecInputs, ExecOutputs, OutputMessage, OutputTransfer, SubjectDepositData,
};
use alpen_chunk_runtime::{PrivateInput, RawBlockData, RawChunkData};
use alpen_evm_ee::{EvmBlock, EvmBlockBody, EvmExecutionEnvironment, EvmHeader, EvmPartialState};
use alpen_params::{AlpenParams, AlpenSpecId, DEV_PARAMS_JSON};
use alpen_proof_chunk::{EeChunkProgram, EeChunkProofInput};
use alpen_test_utils_evm_workload::{workload_dir, Workload, WorkloadBlock, MIXED_WORKLOAD};
use reth_ethereum_primitives::Block;
use strata_acct_types::{BitcoinAmount, Hash, SubjectId};
use strata_codec::{decode_buf_exact, encode_to_vec};
use tracing::info;
use zkaleido::{ExecutionSummary, ProofReceiptWithMetadata, ZkVmHost, ZkVmProgram};

/// The spec version the workload's blocks are stamped with and the guests
/// prove under.
pub(super) const PERF_SPEC_VERSION: AlpenSpecId = AlpenSpecId::V1;

/// The dev-network params the workload was generated with, the same ones the
/// SP1 guests bake in.
pub(super) fn perf_alpen_params() -> AlpenParams {
    serde_json::from_str(DEV_PARAMS_JSON).expect("dev params should parse")
}

pub(super) fn load_workload() -> Workload {
    let dir = workload_dir(MIXED_WORKLOAD);
    Workload::load(&dir).unwrap_or_else(|e| panic!("load workload from {}: {e}", dir.display()))
}

/// A chunk proof input and the transition it proves.
pub(super) struct Chunk {
    pub(super) input: EeChunkProofInput,
    pub(super) transition: ChunkTransition,
}

/// Builds the chunk over all of `workload`'s blocks.
pub(super) fn build_chunk(workload: &Workload) -> Chunk {
    let prev_header: Header =
        alloy_rlp::decode_exact(&workload.prev_header_rlp[..]).expect("decode prev header");
    let parent_evm_header = EvmHeader::new(prev_header);
    let parent_blkid: Hash = parent_evm_header.compute_block_id();

    // Production takes each block's outputs from its exec record. Replaying
    // the blocks here derives the same outputs and checks the workload
    // executes before any guest sees it.
    let params = perf_alpen_params();
    let ee = EvmExecutionEnvironment::new(&params, PERF_SPEC_VERSION);
    let mut state: EvmPartialState =
        decode_buf_exact(&workload.chunk_pre_state).expect("decode chunk pre-state");

    let mut chunk_inputs = ExecInputs::new_empty();
    let mut chunk_outputs = ExecOutputs::new_empty();
    let mut block_datas = Vec::with_capacity(workload.blocks.len());
    let mut tip = None;
    for workload_block in &workload.blocks {
        let block = evm_block(workload_block);
        let inputs = block_inputs(workload_block);

        let header_intrinsics = block.get_header().get_intrinsics();
        let payload = ExecPayload::new(&header_intrinsics, block.get_body());
        let output = ee
            .execute_block_body(&state, &payload, &inputs)
            .expect("workload block executes");
        ee.merge_write_into_state(&mut state, output.write_batch())
            .expect("merge block writes");
        ee.update_partial_state_after_block(&mut state, block.get_header())
            .expect("update state after block");
        let outputs = output.outputs().clone();

        extend_exec_inputs(&mut chunk_inputs, &inputs);
        extend_exec_outputs(&mut chunk_outputs, &outputs);
        block_datas.push(
            RawBlockData::from_block::<EvmExecutionEnvironment>(&block, inputs, outputs)
                .expect("encode block"),
        );
        tip = Some(block);
    }
    let tip = tip.expect("workload has blocks");

    let transition = ChunkTransition::new(
        parent_blkid,
        tip.get_header().compute_block_id(),
        tip.get_header().get_state_root(),
        tip.get_header().get_exec_header_summary(),
        chunk_inputs,
        chunk_outputs,
    );
    let private_input = PrivateInput::new(
        transition.clone(),
        RawChunkData::new(block_datas, parent_blkid),
        encode_to_vec(&parent_evm_header).expect("encode prev header"),
        workload.chunk_pre_state.clone(),
    );

    Chunk {
        input: EeChunkProofInput { private_input },
        transition,
    }
}

fn evm_block(workload_block: &WorkloadBlock) -> EvmBlock {
    let block: Block =
        alloy_rlp::decode_exact(&workload_block.block_rlp[..]).expect("decode workload block");
    EvmBlock::new(
        EvmHeader::new(block.header),
        EvmBlockBody::from_alloy_body(block.body),
    )
}

fn block_inputs(workload_block: &WorkloadBlock) -> ExecInputs {
    let mut inputs = ExecInputs::new_empty();
    for deposit in &workload_block.deposits {
        inputs.add_subject_deposit(SubjectDepositData::new(
            SubjectId::from(deposit.dest_subject),
            BitcoinAmount::try_from(deposit.sats).expect("deposit amount fits"),
        ));
    }
    inputs
}

// TODO(STR-3553): these mirror the chunk-level aggregation helpers in
// alpen-client's `spec_chunk.rs`. Use the upstream `ExecOutputs::extend_from`
// in both places once it lands.
fn extend_exec_inputs(dst: &mut ExecInputs, src: &ExecInputs) {
    for deposit in src.subject_deposits() {
        dst.add_subject_deposit(deposit.clone());
    }
}

fn extend_exec_outputs(dst: &mut ExecOutputs, src: &ExecOutputs) {
    for transfer in src.output_transfers() {
        dst.add_transfer(OutputTransfer::new(transfer.dest(), transfer.value()));
    }
    for message in src.output_messages() {
        dst.add_message(OutputMessage::new(
            message.dest(),
            message.payload().clone(),
        ));
    }
    if let Some(new_predicate) = src.new_predicate() {
        dst.set_new_predicate(Some(new_predicate.clone()));
    }
}

pub(crate) fn gen_perf_report(host: &impl ZkVmHost) -> (String, ExecutionSummary) {
    info!("Generating execution summary for Alpen Chunk");
    let chunk = build_chunk(&load_workload());
    let summary = <EeChunkProgram as ZkVmProgram>::execute(&chunk.input, host)
        .expect("alpen-chunk execution");
    (EeChunkProgram::name(), summary)
}

pub(crate) fn gen_proof(host: &impl ZkVmHost) -> ProofReceiptWithMetadata {
    info!("Generating proof for Alpen Chunk");
    let chunk = build_chunk(&load_workload());
    <EeChunkProgram as ZkVmProgram>::prove(&chunk.input, host).expect("alpen-chunk proof")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpen_chunk_native_execution() {
        let workload = load_workload();
        let chunk = build_chunk(&workload);
        let output = EeChunkProgram::new(perf_alpen_params(), PERF_SPEC_VERSION)
            .execute(&chunk.input)
            .expect("native execution");
        assert_eq!(output, chunk.transition);
        assert_eq!(
            output.inputs().subject_deposits().len(),
            workload
                .blocks
                .iter()
                .map(|block| block.deposits.len())
                .sum::<usize>()
        );
        assert!(!output.outputs().output_messages().is_empty());
    }
}
