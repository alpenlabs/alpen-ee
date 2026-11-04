//! Alpen metrics: this binary's own instruments, the descriptions of every
//! Alpen metric, and the store sampler.
//!
//! Every Alpen crate emits through the `metrics` facade. The recorder is
//! reth's Prometheus one, installed first thing in `main` (reth would install
//! it at launch, too late for what starts earlier), which prefixes every name
//! with `reth_` and serves them on reth's `--metrics <addr>` endpoint. So an
//! instrument named `alpen_ee_blocks_built_total` here is scraped as
//! `reth_alpen_ee_blocks_built_total`, next to reth's own metrics and the
//! `strata_service_*` ones the service framework records.
//!
//! Histograms render as summaries (`quantile="0.5"` and so on, plus `_sum`
//! and `_count`) because reth's recorder sets no buckets.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(feature = "sequencer")]
use alpen_da_provider::metrics::describe as describe_da;
use alpen_engine_reth::metrics::describe as describe_engine;
use alpen_ol_tracker::metrics::describe as describe_ol_tracker;
#[cfg(feature = "sequencer")]
use alpen_sequencer::metrics::describe as describe_sequencer;
use alpen_storage_store::MDBX_ENVS;
use metrics::{describe_counter, describe_gauge, gauge, Unit};
use reth_tasks::TaskExecutor;
use tokio::{task::spawn_blocking, time::sleep};
use tracing::{debug, warn};

/// Block headers the gossip task signed and broadcast to peers.
pub(crate) const GOSSIP_BROADCASTS: &str = "alpen_ee_gossip_broadcasts_total";
/// Valid gossip packages received from the sequencer.
pub(crate) const GOSSIP_RECEIVED: &str = "alpen_ee_gossip_received_total";
/// Peers connected on the gossip subprotocol.
pub(crate) const GOSSIP_PEERS: &str = "alpen_ee_gossip_peers";

/// Proof tasks handed to the provers, by `kind` (`chunk`, `acct`).
#[cfg(feature = "sequencer")]
pub(crate) const PROOF_TASKS_SUBMITTED: &str = "alpen_ee_proof_tasks_submitted_total";
/// Proof receipts the provers stored, by `kind`.
#[cfg(feature = "sequencer")]
pub(crate) const PROOF_RECEIPTS: &str = "alpen_ee_proof_receipts_total";
/// Proof status checks by the lifecycle, by the `status` found.
#[cfg(feature = "sequencer")]
pub(crate) const PROOF_STATUS_CHECKS: &str = "alpen_ee_proof_status_checks_total";
/// DA fee rate in use, in wei per byte.
#[cfg(feature = "sequencer")]
pub(crate) const DA_FEE_RATE_WEI_PER_BYTE: &str = "alpen_ee_da_fee_rate_wei_per_byte";
/// `1` while the DA fee rate is stale, `0` while it is fresh.
#[cfg(feature = "sequencer")]
pub(crate) const DA_FEE_RATE_STALE: &str = "alpen_ee_da_fee_rate_stale";
/// DA fee rate refreshes, by `outcome` (`success`, `timeout`, `failure`).
#[cfg(feature = "sequencer")]
pub(crate) const DA_FEE_RATE_REFRESHES: &str = "alpen_ee_da_fee_rate_refreshes_total";
/// DA transactions the broadcaster still tracks: awaiting broadcast or finality depth.
/// Non-zero normally; sustained growth means L1 publication is backing up.
#[cfg(feature = "sequencer")]
pub(crate) const DA_TXS_PENDING: &str = "alpen_ee_da_txs_pending";
/// Rows in each store table, by `env` and `table`.
pub(crate) const STORE_ROWS: &str = "alpen_ee_store_rows";
/// Bytes each MDBX environment's data file occupies on disk, by `env`.
pub(crate) const STORE_ENV_BYTES: &str = "alpen_ee_store_env_bytes";

/// How often the store sampler counts rows and measures the data files.
const STORE_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// Row counts as `(environment, table, rows)`.
pub(crate) type TableCounts = Vec<(&'static str, &'static str, usize)>;

/// Registers the descriptions of every Alpen metric, this binary's and the
/// crates'. `main` calls it right after installing the recorder, since
/// descriptions recorded before a recorder exists are dropped.
pub(crate) fn describe_all() {
    describe_engine();
    describe_ol_tracker();
    #[cfg(feature = "sequencer")]
    {
        describe_sequencer();
        describe_da();
        describe_counter!(
            PROOF_TASKS_SUBMITTED,
            "Proof tasks handed to the provers, by kind"
        );
        describe_counter!(PROOF_RECEIPTS, "Proof receipts the provers stored, by kind");
        describe_counter!(
            PROOF_STATUS_CHECKS,
            "Proof status checks by the lifecycle, by status"
        );
        describe_gauge!(
            DA_FEE_RATE_WEI_PER_BYTE,
            "DA fee rate in use, in wei per byte"
        );
        describe_gauge!(DA_FEE_RATE_STALE, "1 while the DA fee rate is stale");
        describe_counter!(DA_FEE_RATE_REFRESHES, "DA fee rate refreshes, by outcome");
        describe_gauge!(
            DA_TXS_PENDING,
            "DA transactions the broadcaster still tracks, awaiting broadcast or finality depth"
        );
    }
    describe_counter!(
        GOSSIP_BROADCASTS,
        "Block headers signed and broadcast to peers"
    );
    describe_counter!(
        GOSSIP_RECEIVED,
        "Valid gossip packages received from the sequencer"
    );
    describe_gauge!(GOSSIP_PEERS, "Peers connected on the gossip subprotocol");
    describe_gauge!(STORE_ROWS, "Rows in each store table");
    describe_gauge!(
        STORE_ENV_BYTES,
        Unit::Bytes,
        "Bytes each MDBX environment's data file occupies on disk"
    );
}

/// Spawns the task that samples the store every [`STORE_SAMPLE_INTERVAL`]:
/// `counts` gives each table's rows (O(1) each through MDBX's table stat),
/// and the data file of each environment under `mdbx_dir` is measured.
///
/// The sample itself is synchronous MDBX and filesystem work, so it runs on
/// the blocking pool: on a runtime worker it held up the timer driver, and
/// with it the block builder's tick, for seconds at a time.
pub(crate) fn spawn_store_sampler(
    task_executor: &TaskExecutor,
    mdbx_dir: PathBuf,
    counts: impl Fn() -> eyre::Result<TableCounts> + Send + Sync + 'static,
) {
    let counts: Arc<dyn Fn() -> eyre::Result<TableCounts> + Send + Sync> = Arc::new(counts);
    task_executor.spawn_task(async move {
        loop {
            let (dir, counts) = (mdbx_dir.clone(), counts.clone());
            if let Err(error) = spawn_blocking(move || sample_store(&dir, &*counts)).await {
                warn!(%error, "store sampler panicked");
            }
            sleep(STORE_SAMPLE_INTERVAL).await;
        }
    });
}

fn sample_store(mdbx_dir: &Path, counts: &(dyn Fn() -> eyre::Result<TableCounts> + Sync)) {
    let started = Instant::now();
    match counts() {
        Ok(rows) => {
            for (env, table, n) in rows {
                gauge!(STORE_ROWS, "env" => env, "table" => table).set(n as f64);
            }
        }
        Err(error) => warn!(%error, "store row sampling failed"),
    }
    for env in MDBX_ENVS {
        if let Some(bytes) = data_file_bytes(&mdbx_dir.join(env).join("mdbx.dat")) {
            gauge!(STORE_ENV_BYTES, "env" => env).set(bytes as f64);
        }
    }
    debug!(elapsed = ?started.elapsed(), "sampled the store");
}

/// Bytes the file occupies on disk: MDBX data files are sparse, so their
/// apparent length says nothing about what is used.
fn data_file_bytes(path: &Path) -> Option<u64> {
    let metadata = fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.blocks() * 512)
    }
    #[cfg(not(unix))]
    {
        Some(metadata.len())
    }
}
