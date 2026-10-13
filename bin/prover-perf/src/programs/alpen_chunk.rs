//! Perf input for the alpen-chunk SP1 guest.
//!
//! Proves the checked-in mixed workload as one chunk: 100 V1 blocks of
//! transfers, token and pool calls, bridge-outs, precompile calls and
//! deposits. The input comes from [`build_workload_chunk`], the builder the
//! chunk proof crate's own native test runs.

use std::sync::LazyLock;

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

/// The checked-in workload, loaded once per run.
static WORKLOAD: LazyLock<Workload> = LazyLock::new(|| {
    let dir = workload_dir(MIXED_WORKLOAD);
    Workload::load(&dir).unwrap_or_else(|e| panic!("load workload from {}: {e}", dir.display()))
});

/// The chunk over all of the workload's blocks, built once per run.
///
/// Building it replays every block natively, and both guests' inputs need
/// it.
static CHUNK: LazyLock<WorkloadChunk> =
    LazyLock::new(|| build_workload_chunk(&WORKLOAD, &perf_alpen_params(), PERF_SPEC_VERSION));

pub(super) fn workload() -> &'static Workload {
    &WORKLOAD
}

pub(super) fn workload_chunk() -> &'static WorkloadChunk {
    &CHUNK
}

pub(crate) fn gen_perf_report(host: &impl ZkVmHost) -> (String, ExecutionSummary) {
    info!("Generating execution summary for Alpen Chunk");
    let summary = <EeChunkProgram as ZkVmProgram>::execute(&workload_chunk().input, host)
        .expect("alpen-chunk execution");
    (EeChunkProgram::name(), summary)
}

pub(crate) fn gen_proof(host: &impl ZkVmHost) -> ProofReceiptWithMetadata {
    info!("Generating proof for Alpen Chunk");
    <EeChunkProgram as ZkVmProgram>::prove(&workload_chunk().input, host)
        .expect("alpen-chunk proof")
}
