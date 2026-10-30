//! EE chunk proof implementation wrapping `chunk-runtime` with zkaleido proof IO.

use alpen_chunk_runtime::ArchivedPrivateInput;
use alpen_evm_ee::EvmExecutionEnvironment;
use alpen_params::{AlpenParams, AlpenSpecId};
use rkyv::rancor::Error as RkyvError;
use zkaleido::ZkVmEnvSerde;

#[cfg(not(target_os = "zkvm"))]
mod program;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
#[cfg(not(target_os = "zkvm"))]
pub mod v1;
#[cfg(not(target_os = "zkvm"))]
pub mod v2;

#[cfg(not(target_os = "zkvm"))]
pub use program::{EeChunkProgram, EeChunkProofInput, process_ee_chunk_native};

/// Guest entry point for EE chunk proof generation.
///
/// Verifies the chunk transition against `params`'s genesis and bridge
/// params using the EVM execution environment, and commits the resulting
/// [`alpen_chain_types::ChunkTransition`] as SSZ public output.
///
/// `params` and `spec_version` are trusted, out-of-band arguments, not zkVM
/// input: genesis and bridge params are consensus-critical, and a
/// host-supplied spec version would let a prover pick which rules its chunk
/// is checked under -- claiming an older version for a newer chunk to dodge
/// a hardfork's rules. Both are bound into this guest's verifying key
/// instead, so the predicate rotation that activates a version is also what
/// authorizes proving under it. See `provers/sp1/guest-alpen-chunk/src/main.rs`
/// for the guest-side construction path.
///
/// One version per chunk is well-defined because a rotation-consuming block
/// always ends its group -- see `sealing_policy::rotation_policy` -- so no
/// chunk straddles an activation boundary.
pub fn process_ee_chunk(zkvm: &impl ZkVmEnvSerde, params: &AlpenParams, spec_version: AlpenSpecId) {
    let buf = zkvm.read_buf();
    let input: &ArchivedPrivateInput = rkyv::access::<ArchivedPrivateInput, RkyvError>(&buf)
        .expect("failed to access rkyv archive");

    verify_and_commit(zkvm, params, spec_version, input);
}

/// Verifies the chunk transition in `input` and commits it as public output.
fn verify_and_commit(
    zkvm: &impl ZkVmEnvSerde,
    params: &AlpenParams,
    spec_version: AlpenSpecId,
    input: &ArchivedPrivateInput,
) {
    let ee = EvmExecutionEnvironment::new(params, spec_version);
    alpen_chunk_runtime::verify_input(&ee, input).expect("chunk verification failed");

    zkvm.commit_buf(input.chunk_transition_ssz());
}
