//! Builds chunk proof inputs from the generated EVM workloads.
//!
//! The input is assembled the way the sequencer's chunk prover does it
//! (`ChunkSpec::fetch_input` in alpen-client).

use alloy_consensus::Header;
use alloy_rlp::decode_exact;
use alpen_acct_types::{ExecBlock, ExecHeader, ExecPartialState, ExecutionEnvironment};
use alpen_chain_types::{ChunkTransition, ExecInputs, ExecOutputs, SubjectDepositData};
use alpen_chunk_runtime::{PrivateInput, RawBlockData, RawChunkData};
use alpen_evm_ee::{EvmBlock, EvmExecutionEnvironment, EvmHeader, EvmPartialState};
use alpen_params::{AlpenParams, AlpenSpecId};
use alpen_test_utils_evm_workload::{Workload, WorkloadBlock};
use reth_ethereum_primitives::Block;
use strata_acct_types::{BitcoinAmount, Hash, SubjectId};
use strata_codec::{decode_buf_exact, encode_to_vec};

use crate::EeChunkProofInput;

/// A chunk proof input and the transition it proves.
#[derive(Debug)]
pub struct WorkloadChunk {
    pub input: EeChunkProofInput,
    pub transition: ChunkTransition,
}

/// Builds the chunk over all of `workload`'s blocks, proven under `params` and
/// `spec_version`.
///
/// Panics if a block breaks a rule the guest checks.
pub fn build_workload_chunk(
    workload: &Workload,
    params: &AlpenParams,
    spec_version: AlpenSpecId,
) -> WorkloadChunk {
    let prev_header: Header =
        decode_exact(&workload.prev_header_rlp[..]).expect("decode prev header");
    let prev_evm_header = EvmHeader::new(prev_header);
    let prev_blkid: Hash = prev_evm_header.compute_block_id();

    // Production takes each block's outputs from its exec record. Replaying
    // the blocks here derives the same outputs, and running each check the
    // guest runs means a bad workload fails here with a clear error, not as
    // an opaque failure inside the guest.
    let ee = EvmExecutionEnvironment::new(params, spec_version);
    let mut state: EvmPartialState =
        decode_buf_exact(&workload.chunk_pre_state).expect("decode chunk pre-state");

    let mut chunk_inputs = ExecInputs::new_empty();
    let mut chunk_outputs = ExecOutputs::new_empty();
    let mut block_datas = Vec::with_capacity(workload.blocks.len());
    let mut parent = prev_evm_header.clone();
    for workload_block in &workload.blocks {
        let block = evm_block(workload_block);
        let inputs = block_inputs(workload_block);
        let header = block.get_header().clone();

        ee.verify_header(&parent, &header)
            .expect("workload header is valid");
        ee.verify_block_pre_execution(&block, &inputs)
            .expect("workload block is valid before execution");
        let output = ee
            .execute_block(&state, block.clone())
            .expect("workload block executes");
        let outputs = output.outputs().clone();
        ee.verify_block_post_execution(&header, &output, &outputs)
            .expect("workload block is valid after execution");
        ee.merge_write_into_state(&mut state, output.write_batch())
            .expect("merge block writes");
        assert_eq!(
            state.compute_state_root(),
            header.get_state_root(),
            "workload block's state root matches its header"
        );

        chunk_inputs.extend_from(&inputs);
        chunk_outputs.extend_from(&outputs);
        block_datas.push(
            RawBlockData::from_block::<EvmExecutionEnvironment>(&block, inputs, outputs)
                .expect("encode block"),
        );
        parent = header;
    }
    assert!(!block_datas.is_empty(), "workload has blocks");
    let tip = parent;

    let transition = ChunkTransition::new(
        prev_blkid,
        tip.compute_block_id(),
        tip.get_state_root(),
        tip.get_exec_header_summary(),
        chunk_inputs,
        chunk_outputs,
    );
    let private_input = PrivateInput::new(
        transition.clone(),
        RawChunkData::new(block_datas),
        encode_to_vec(&prev_evm_header).expect("encode prev header"),
        workload.chunk_pre_state.clone(),
    );

    WorkloadChunk {
        input: EeChunkProofInput { private_input },
        transition,
    }
}

fn evm_block(workload_block: &WorkloadBlock) -> EvmBlock {
    let block: Block = decode_exact(&workload_block.block_rlp[..]).expect("decode workload block");
    EvmBlock::new(block)
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
