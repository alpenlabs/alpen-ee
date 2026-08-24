//! The btcio DA pipeline: Bitcoin RPC client, tx broadcaster, chunked
//! envelope reveal task, and the blob provider batches read/write DA
//! payloads through.

use std::sync::Arc;

use alpen_ee_da_provider::{ChunkedEnvelopeDaProvider, DaBlobSource, StateDiffBlobProvider};
use alpen_ee_database::{EeNodeStorage, SequencerDatabases};
use alpen_ee_params::AlpenParams;
use bitcoind_async_client::{
    corepc_types::bitcoin::key::Keypair,
    traits::{Reader, Wallet as _},
    Auth, Client as BtcClient,
};
use reth_provider::HeaderProvider;
use reth_tasks::TaskExecutor;
use strata_btcio::{
    broadcaster::BroadcasterBuilder, writer::chunked_envelope::create_chunked_envelope_task,
    BtcioParams,
};
use strata_config::{
    btcio::{BroadcasterConfig, WriterConfig},
    BitcoindConfig,
};
use strata_primitives::L1Height;
use tokio::runtime::Handle;
use tracing::{info, info_span, Instrument};

use super::header_summary::RethHeaderSummaryProvider;
use crate::service_executor::ServiceExecutor;

// Mirrors bitcoind-async-client's upstream defaults, applied when
// `BitcoindConfig.retry_count`/`retry_interval` are left unset in
// `[sequencer.bitcoind]`.
const DEFAULT_BTCIO_RETRY_COUNT: u16 = 3;
const DEFAULT_BTCIO_RETRY_INTERVAL_MS: u64 = 1_000;

/// Everything [`start`] needs to bring up the DA pipeline.
pub(crate) struct DaPipelineInputs<'a, P> {
    pub(crate) bitcoind: &'a BitcoindConfig,
    pub(crate) broadcaster: &'a BroadcasterConfig,
    /// Rollup-to-L1 facts, not sequencer config — see `AlpenClientConfig`'s
    /// doc comment for why these come from outside `SequencerConfig`.
    pub(crate) l1_reorg_safe_depth: u32,
    pub(crate) genesis_l1_height: L1Height,
    pub(crate) dbs: &'a SequencerDatabases,
    pub(crate) storage: Arc<EeNodeStorage>,
    pub(crate) node_provider: P,
    pub(crate) params: Arc<AlpenParams>,
    pub(crate) writer_config: Arc<WriterConfig>,
    pub(crate) sequencer_keypair: Keypair,
}

/// Handles the rest of sequencer startup needs after the DA pipeline is up.
pub(crate) struct DaPipeline {
    pub(crate) batch_da_provider: Arc<ChunkedEnvelopeDaProvider>,
    /// The Bitcoin RPC client. Also needed by the account prover, which
    /// resolves its L1 confirmation depth through it.
    pub(crate) btc_client: Arc<BtcClient>,
}

/// Brings up the Bitcoin RPC client, the tx broadcaster, and the chunked
/// envelope reveal task, then wires them into a [`ChunkedEnvelopeDaProvider`]
/// batches read/write DA payloads through.
pub(crate) async fn start<P>(
    service_executor: &ServiceExecutor,
    task_executor: &TaskExecutor,
    inputs: DaPipelineInputs<'_, P>,
) -> eyre::Result<DaPipeline>
where
    P: HeaderProvider<Header = reth_primitives::Header> + Send + Sync + 'static,
{
    let DaPipelineInputs {
        bitcoind,
        broadcaster,
        l1_reorg_safe_depth,
        genesis_l1_height,
        dbs,
        storage,
        node_provider,
        params,
        writer_config,
        sequencer_keypair,
    } = inputs;

    let magic_bytes = params.blob_spec().magic_bytes();
    let btcio_params = BtcioParams::new(l1_reorg_safe_depth, magic_bytes, genesis_l1_height);

    let retry_count = bitcoind.retry_count.unwrap_or(DEFAULT_BTCIO_RETRY_COUNT);
    let retry_interval = bitcoind
        .retry_interval
        .unwrap_or(DEFAULT_BTCIO_RETRY_INTERVAL_MS);

    // Bitcoin RPC client.
    let btc_client = Arc::new(
        BtcClient::new(
            bitcoind.rpc_url.clone(),
            Auth::UserPass(bitcoind.rpc_user.clone(), bitcoind.rpc_password.clone()),
            Some(retry_count),
            Some(retry_interval),
            None,
        )
        .map_err(|e| eyre::eyre!("creating Bitcoin RPC client: {e}"))?,
    );
    info!(
        target: "alpen-client", component = "alpen",
        retry_count, retry_interval_ms = retry_interval,
        "btcio Bitcoin RPC retry policy configured",
    );

    // Fail fast if the connected bitcoind is on a different network than
    // configured — today alpen-client has no other check that it's pointed
    // at the right chain, mirroring bin/strata's own startup network check.
    let live_network = btc_client
        .network()
        .await
        .map_err(|e| eyre::eyre!("querying Bitcoin RPC network: {e}"))?;
    eyre::ensure!(
        live_network == bitcoind.network,
        "sequencer.bitcoind.network is configured as {:?}, but the connected \
         bitcoind reports {live_network:?}",
        bitcoind.network,
    );

    // Sequencer address from bitcoin wallet.
    let sequencer_address = btc_client
        .get_new_address()
        .await
        .map_err(|e| eyre::eyre!("failed to get sequencer address: {e}"))?;

    // Wrap raw DBs in ops using the runtime this task is already running on,
    // the same one `node::bootstrap_node` builds the node storage against.
    let db_handle = Handle::current();
    let broadcast_ops = Arc::new(dbs.broadcast_ops(db_handle.clone()));
    let envelope_ops = Arc::new(dbs.chunked_envelope_ops(db_handle));

    // Launch broadcaster service and create chunked envelope task.
    let broadcast_handle = Arc::new(
        BroadcasterBuilder::new(
            btc_client.clone(),
            broadcast_ops.clone(),
            btcio_params,
            broadcaster.max_fee_rate(),
        )
        .with_broadcast_poll_interval_ms(broadcaster.poll_interval_ms)
        .launch(service_executor)
        .await
        .map_err(|e| eyre::eyre!("starting broadcaster service: {e}"))?,
    );

    let (envelope_handle, envelope_watcher_task) = create_chunked_envelope_task(
        btc_client.clone(),
        writer_config,
        btcio_params,
        sequencer_address,
        sequencer_keypair,
        envelope_ops,
        broadcast_handle.clone(),
    )
    .map_err(|e| eyre::eyre!("creating chunked envelope task: {e}"))?;

    let header_summary = Arc::new(RethHeaderSummaryProvider::new(node_provider));

    let blob_provider: Arc<dyn DaBlobSource> = Arc::new(StateDiffBlobProvider::new(
        storage,
        dbs.witness_db(),
        header_summary,
        dbs.da_context_db(),
    ));

    let batch_da_provider = Arc::new(ChunkedEnvelopeDaProvider::new(
        blob_provider,
        envelope_handle,
        broadcast_ops,
        btc_client.clone(),
        magic_bytes,
    )?);

    // Spawn btcio tasks.
    task_executor.spawn_critical(
        "chunked_envelope_watcher",
        envelope_watcher_task
            .instrument(info_span!("chunked_envelope_watcher", component = "alpen")),
    );

    info!(target: "alpen-client", component = "alpen", "btcio DA pipeline started");

    Ok(DaPipeline {
        batch_da_provider,
        btc_client,
    })
}
