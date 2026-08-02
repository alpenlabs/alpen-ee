//! The btcio DA pipeline: Bitcoin RPC client, tx broadcaster, chunked
//! envelope reveal task, and the blob provider batches read/write DA
//! payloads through.

use std::sync::Arc;

use alpen_ee_da_provider::{ChunkedEnvelopeDaProvider, DaBlobSource, StateDiffBlobProvider};
use alpen_ee_database::{EeDatabases, EeNodeStorage};
use alpen_ee_params::AlpenParams;
use bitcoind_async_client::{
    corepc_types::bitcoin::key::Keypair, traits::Wallet as _, Auth, Client as BtcClient,
};
use reth_provider::HeaderProvider;
use reth_tasks::TaskExecutor;
use strata_btcio::{
    broadcaster::BroadcasterBuilder, writer::chunked_envelope::create_chunked_envelope_task,
    BtcioParams,
};
use strata_config::btcio::WriterConfig;
use tokio::runtime::Handle;
use tracing::{info, info_span, Instrument};

use super::header_summary::RethHeaderSummaryProvider;
use crate::{
    args::{BtcioArgs, DaArgs},
    service_executor::ServiceExecutor,
};

/// Everything [`start`] needs to bring up the DA pipeline.
pub(crate) struct DaPipelineInputs<'a, P> {
    pub(crate) da_args: &'a DaArgs,
    pub(crate) btcio_args: &'a BtcioArgs,
    pub(crate) dbs: &'a EeDatabases,
    pub(crate) db_handle: Handle,
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
        da_args,
        btcio_args,
        dbs,
        db_handle,
        storage,
        node_provider,
        params,
        writer_config,
        sequencer_keypair,
    } = inputs;

    // clap `requires_all` on --sequencer guarantees all DA args are present.
    let magic_bytes = params.blob_spec().magic_bytes();
    let btc_url = da_args.btc_rpc_url.as_ref().expect("enforced by clap");
    let btc_user = da_args.btc_rpc_user.as_ref().expect("enforced by clap");
    let btc_pass = da_args.btc_rpc_password.as_ref().expect("enforced by clap");

    // Create BtcioParams directly from CLI args.
    let btcio_params = BtcioParams::new(
        da_args.l1_reorg_safe_depth,
        magic_bytes,
        da_args.genesis_l1_height,
    );

    // Bitcoin RPC client.
    let btc_client = Arc::new(
        BtcClient::new(
            btc_url.clone(),
            Auth::UserPass(btc_user.clone(), btc_pass.clone()),
            Some(btcio_args.retry_count),
            Some(btcio_args.retry_interval),
            None,
        )
        .map_err(|e| eyre::eyre!("creating Bitcoin RPC client: {e}"))?,
    );
    info!(
        target: "alpen-client", component = "alpen",
        retry_count = btcio_args.retry_count,
        retry_interval_ms = btcio_args.retry_interval,
        "btcio Bitcoin RPC retry policy configured",
    );

    // Sequencer address from bitcoin wallet.
    let sequencer_address = btc_client
        .get_new_address()
        .await
        .map_err(|e| eyre::eyre!("failed to get sequencer address: {e}"))?;

    // Wrap raw DBs in ops using the shared runtime handle.
    let broadcast_ops = Arc::new(dbs.broadcast_ops(db_handle.clone()));
    let envelope_ops = Arc::new(dbs.chunked_envelope_ops(db_handle));

    // Launch broadcaster service and create chunked envelope task.
    let broadcast_poll_interval = 5_000;

    let broadcast_handle = Arc::new(
        BroadcasterBuilder::new(btc_client.clone(), broadcast_ops.clone(), btcio_params)
            .with_broadcast_poll_interval_ms(broadcast_poll_interval)
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
