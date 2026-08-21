//! EE chunk proof implementation wrapping `ee-chunk-runtime` with zkaleido proof IO.

use std::sync::Arc;

use alpen_ee_params::{AlpenParams, AlpenSpecId};
use alpen_reth_evm::evm::AlpenEvmFactory;
use reth_chainspec::ChainSpec;
use rkyv::rancor::Error as RkyvError;
use strata_ee_chunk_runtime::ArchivedPrivateInput;
use strata_evm_ee::EvmExecutionEnvironment;
use zkaleido::ZkVmEnvSerde;

mod program;

pub use program::{EeChunkProgram, EeChunkProofInput};

/// Guest entry point for EE chunk proof generation.
///
/// Verifies the chunk transition against `params`'s genesis and bridge
/// params using the EVM execution environment, and commits the resulting
/// [`strata_ee_chain_types::ChunkTransition`] as SSZ public output.
///
/// `params` is a trusted, out-of-band argument, not zkVM input: genesis and
/// bridge params are consensus-critical, so they're bound into this guest's
/// verifying key rather than trusted as prover-supplied private input. See
/// `provers/sp1/guest-alpen-chunk/src/main.rs` for the guest-side
/// construction path.
pub fn process_ee_chunk(zkvm: &impl ZkVmEnvSerde, params: &AlpenParams) {
    // TODO(STR-4002): pin to v0 until per-chunk version resolution is
    // threaded through the proof guests.
    let chain_spec: Arc<ChainSpec> = params.evm_spec().chain_spec(AlpenSpecId::V0).clone();
    let evm_factory = AlpenEvmFactory::from_bridge_params(params.bridge_params());
    let ee = EvmExecutionEnvironment::new(chain_spec, evm_factory);

    let buf = zkvm.read_buf();
    let input: &ArchivedPrivateInput = rkyv::access::<ArchivedPrivateInput, RkyvError>(&buf)
        .expect("failed to access rkyv archive");

    strata_ee_chunk_runtime::verify_input(&ee, input).expect("chunk verification failed");

    zkvm.commit_buf(input.chunk_transition_ssz());
}
