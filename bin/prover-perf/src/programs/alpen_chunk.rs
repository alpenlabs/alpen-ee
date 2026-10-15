//! Perf input for the alpen-chunk SP1 guest.
//!
//! Proves the checked-in mixed workload as one chunk: 100 V1 blocks of
//! transfers, token and pool calls, bridge-outs, precompile calls and
//! deposits. The input comes from [`build_workload_chunk`], the builder the
//! chunk proof crate's own native test runs.

use alpen_params::{AlpenParams, AlpenSpecId, DEV_PARAMS_JSON};
use alpen_proof_chunk::{
    test_utils::{build_workload_chunk, WorkloadChunk},
    EeChunkProgram,
};
use alpen_test_utils_evm_workload::{workload_dir, Workload, MIXED_WORKLOAD};
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

/// Builds the chunk over all of `workload`'s blocks.
pub(super) fn build_chunk(workload: &Workload) -> WorkloadChunk {
    build_workload_chunk(workload, &perf_alpen_params(), PERF_SPEC_VERSION)
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
