//! Perf inputs for the alpen-chunk SP1 guest.
//!
//! Proves a checked-in workload as one chunk. The `mixed` workload is 100 V2
//! blocks of transfers, token and pool calls, bridge-outs, precompile calls
//! and deposits; the `precompiles` workload is ten blocks that each call
//! every precompile. The inputs come from [`build_workload_chunk`], the
//! builder the chunk proof crate's own native test runs.

use std::sync::LazyLock;

use alpen_params::{AlpenParams, AlpenSpecId, DEV_PARAMS_JSON};
use alpen_proof_chunk::{
    test_utils::{build_workload_chunk, WorkloadChunk},
    EeChunkProgram,
};
use alpen_test_utils_evm_workload::{workload_dir, Workload, MIXED_WORKLOAD, PRECOMPILES_WORKLOAD};
use tracing::info;
use zkaleido::{ExecutionSummary, ProofReceiptWithMetadata, ZkVmHost, ZkVmProgram};

/// The spec version the workload's blocks are stamped with and the guests
/// prove under.
pub(super) const PERF_SPEC_VERSION: AlpenSpecId = AlpenSpecId::V2;

/// The dev-network params the workload was generated with, the same ones the
/// SP1 guests bake in.
pub(super) fn perf_alpen_params() -> AlpenParams {
    serde_json::from_str(DEV_PARAMS_JSON).expect("dev params should parse")
}

/// The checked-in workloads the chunk guest runs on.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PerfWorkload {
    /// A hundred blocks of ordinary traffic. The account guest proves over
    /// this one too, so it is the one with a saved chunk proof.
    Mixed,
    /// Ten blocks that each call every precompile.
    Precompiles,
}

impl PerfWorkload {
    fn name(self) -> &'static str {
        match self {
            Self::Mixed => MIXED_WORKLOAD,
            Self::Precompiles => PRECOMPILES_WORKLOAD,
        }
    }

    /// The name the report lists the run under. The baseline is matched by
    /// it, so it must stay stable.
    fn report_name(self) -> String {
        match self {
            Self::Mixed => EeChunkProgram::name(),
            Self::Precompiles => format!("{} (precompiles)", EeChunkProgram::name()),
        }
    }

    fn chunk(self) -> &'static WorkloadChunk {
        match self {
            Self::Mixed => &MIXED_CHUNK,
            Self::Precompiles => &PRECOMPILES_CHUNK,
        }
    }
}

fn load_workload(name: &str) -> Workload {
    let dir = workload_dir(name);
    Workload::load(&dir).unwrap_or_else(|e| panic!("load workload from {}: {e}", dir.display()))
}

/// The chunk over all of a workload's blocks. Building it replays every
/// block natively, so each is built once per run.
fn build_chunk(workload: &Workload) -> WorkloadChunk {
    build_workload_chunk(workload, &perf_alpen_params(), PERF_SPEC_VERSION)
}

/// The mixed workload, loaded once per run. The account guest's input is
/// built from it as well.
static MIXED_WORKLOAD_DATA: LazyLock<Workload> =
    LazyLock::new(|| load_workload(PerfWorkload::Mixed.name()));

static MIXED_CHUNK: LazyLock<WorkloadChunk> = LazyLock::new(|| build_chunk(&MIXED_WORKLOAD_DATA));

static PRECOMPILES_CHUNK: LazyLock<WorkloadChunk> =
    LazyLock::new(|| build_chunk(&load_workload(PerfWorkload::Precompiles.name())));

/// The mixed workload, which the account guest proves over.
pub(super) fn workload() -> &'static Workload {
    &MIXED_WORKLOAD_DATA
}

/// The chunk over the mixed workload, whose proof the account guest verifies.
pub(super) fn workload_chunk() -> &'static WorkloadChunk {
    &MIXED_CHUNK
}

pub(crate) fn gen_perf_report(
    host: &impl ZkVmHost,
    workload: PerfWorkload,
) -> (String, ExecutionSummary) {
    info!(?workload, "Generating execution summary for Alpen Chunk");
    let summary = <EeChunkProgram as ZkVmProgram>::execute(&workload.chunk().input, host)
        .expect("alpen-chunk execution");
    (workload.report_name(), summary)
}

pub(crate) fn gen_proof(host: &impl ZkVmHost) -> ProofReceiptWithMetadata {
    info!("Generating proof for Alpen Chunk");
    <EeChunkProgram as ZkVmProgram>::prove(&workload_chunk().input, host)
        .expect("alpen-chunk proof")
}
