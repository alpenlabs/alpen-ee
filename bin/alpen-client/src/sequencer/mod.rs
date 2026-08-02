//! Sequencer-only startup: boot-state init, the DA/btcio pipeline, and the
//! batch/chunk builder services that only run with `--sequencer`.
//!
//! [`launch`] is the sole entry point once the reth node exists: it resolves
//! its own writer config, block-builder config, boot state, and DA reveal
//! signing keypair, so callers only need to know a node exists and the
//! `--sequencer` flag is set. [`initial_preconf_head`] is the one other
//! entry point, needed earlier — before the node is built — to seed the p2p
//! preconf head watch with the sequencer's real exec-chain tip.

mod da_pipeline;
mod gas_data_provider;
mod header_summary;
mod payload_builder;
mod prover;
mod provers;
mod services;

use std::{
    env::{self, VarError},
    sync::Arc,
};

use alpen_ee_common::{
    require_latest_batch, BlockNumHash, ConsensusHeads, OLFinalizedStatus, SequencerOLClient,
};
use alpen_ee_database::{EeDatabases, EeNodeStorage};
use alpen_ee_exec_chain::{init_exec_chain_state_from_storage, ExecChainState};
use alpen_ee_params::{AlpenEeGenesisBlockInfo, AlpenParams};
use alpen_ee_sequencer::{
    block_builder_task, build_ol_chain_tracker, create_batch_builder, create_batch_lifecycle_task,
    create_update_submitter_task, init_batch_builder_state, init_lifecycle_state,
    init_ol_chain_tracker_state,
    sealing_policy::{
        block_count_policy::{BlockCountDataProvider, BlockCountPolicy, FixedBlockCountSealing},
        gas_limit_policy::MaxGasSealing,
        or_policy::OrSealing,
    },
    BatchBuilderEvent, BatchBuilderState, BatchLifecycleState, BlockBuilderConfig,
    OLChainTrackerState,
};
use alpen_reth_node::AlpenEngineTypes;
use bitcoind_async_client::corepc_types::bitcoin::{
    key::Keypair,
    secp256k1::{Secp256k1, SecretKey},
};
use eyre::Context;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_provider::{BlockReader, HeaderProvider, StateProviderFactory};
use reth_tasks::TaskExecutor;
use strata_config::btcio::{fee_rate_to_sat_per_vb, FeePolicy, WriterConfig};
use strata_primitives::buf::Buf32;
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};
use tracing::{info, info_span, Instrument};

use self::{gas_data_provider::RethGasDataProvider, payload_builder::AlpenRethPayloadEngine};
use crate::{args::AdditionalConfig, ol::OLClientKind, service_executor::ServiceExecutor};

/// Environment variable for overriding the default EE block time.
const ALPEN_EE_BLOCK_TIME_MS_ENV_VAR: &str = "ALPEN_EE_BLOCK_TIME_MS";

/// Default capacity for the batch builder → chunk builder event channel.
const DEFAULT_BATCH_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Startup state that only the EE sequencer needs: the OL chain tracker,
/// exec chain, batch builder, and batch lifecycle states loaded from
/// storage once the node is up.
struct SequencerBootState {
    ol_chain_tracker: OLChainTrackerState,
    exec_chain: ExecChainState,
    batch_builder: BatchBuilderState<BlockCountPolicy>,
    batch_lifecycle: BatchLifecycleState,
}

/// The sequencer's exec-chain tip, used to seed the p2p preconf head watch
/// before the reth node (and therefore the engine-control task) is built.
///
/// Only loads the exec-chain piece of boot state; [`launch`] loads the full
/// [`SequencerBootState`] again once the node is up. Both reads are cheap,
/// local, read-only sled reads with nothing else touching storage in
/// between, so re-reading is simpler than threading a boot-state value
/// across the generic parts of node startup.
pub(crate) async fn initial_preconf_head(
    enabled: bool,
    storage: &EeNodeStorage,
) -> eyre::Result<Option<BlockNumHash>> {
    if !enabled {
        return Ok(None);
    }

    let exec_chain = init_exec_chain_state_from_storage(storage)
        .instrument(info_span!(
            "init_exec_chain_head_probe",
            component = "alpen"
        ))
        .await
        .context("exec chain state initialization should not fail")?;
    Ok(Some(exec_chain.tip_blocknumhash()))
}

fn log_writer_config(cfg: &WriterConfig) {
    match cfg.fee_policy() {
        FeePolicy::BitcoinD { conf_target } => {
            info!(target: "alpen-client",
            component = "alpen",
            policy = "bitcoind",
            conf_target, "btcio writer configured",);
        }
        FeePolicy::Fixed { fee_rate } => {
            info!(
                target: "alpen-client",
                component = "alpen",
                policy = "fixed",
                fee_rate_sat_vb = fee_rate_to_sat_per_vb(*fee_rate),
                "btcio writer configured",
            );
        }
        FeePolicy::MempoolExplorer {
            policy,
            mempool_base_url,
            fallback_conf_target,
        } => {
            info!(
                target: "alpen-client",
                component = "alpen",
                policy = "mempool",
                tier = ?policy,
                base_url = %mempool_base_url,
                fallback_conf_target,
                "btcio writer configured",
            );
        }
    }
}

fn sequencer_bitcoin_keypair(privkey: &Buf32) -> eyre::Result<Keypair> {
    let sk = SecretKey::from_slice(privkey.as_ref()).context("invalid sequencer private key")?;
    let secp = Secp256k1::signing_only();
    Ok(Keypair::from_secret_key(&secp, &sk))
}

/// Parses the EE block time override.
fn block_builder_config_from_env() -> eyre::Result<BlockBuilderConfig> {
    let default_config = BlockBuilderConfig::default();

    let blocktime_ms = match env::var(ALPEN_EE_BLOCK_TIME_MS_ENV_VAR) {
        Ok(raw_value) => {
            let blocktime_ms = raw_value.parse::<u64>().wrap_err_with(|| {
                format!(
                    "Failed to parse {ALPEN_EE_BLOCK_TIME_MS_ENV_VAR} as a positive integer milliseconds value: {raw_value}"
                )
            })?;
            if blocktime_ms == 0 {
                eyre::bail!("{ALPEN_EE_BLOCK_TIME_MS_ENV_VAR} must be greater than zero");
            }
            info!(
                target: "alpen-client",
                component = "alpen",
                blocktime_ms,
                env_var = ALPEN_EE_BLOCK_TIME_MS_ENV_VAR,
                "Using EE block time override from environment"
            );
            blocktime_ms
        }
        Err(VarError::NotPresent) => {
            let default_blocktime_ms = default_config.blocktime_ms();
            info!(
                target: "alpen-client",
                component = "alpen",
                blocktime_ms = default_blocktime_ms,
                "Using default EE block time"
            );
            return Ok(default_config);
        }
        Err(VarError::NotUnicode(_)) => {
            eyre::bail!("{ALPEN_EE_BLOCK_TIME_MS_ENV_VAR} must contain valid unicode");
        }
    };

    Ok(default_config.with_blocktime_ms(blocktime_ms))
}

/// Loads sequencer boot state: OL chain tracker, exec chain, batch builder,
/// and batch lifecycle.
async fn init_boot_state(
    storage: &EeNodeStorage,
    ol_client: &(impl SequencerOLClient + Send + Sync),
) -> eyre::Result<SequencerBootState> {
    let ol_chain_tracker = init_ol_chain_tracker_state(storage, ol_client)
        .instrument(info_span!("init_ol_chain_tracker", component = "alpen"))
        .await
        .context("ol chain tracker state initialization should not fail")?;
    let exec_chain = init_exec_chain_state_from_storage(storage)
        .instrument(info_span!("init_exec_chain", component = "alpen"))
        .await
        .context("exec chain state initialization should not fail")?;
    let batch_builder = init_batch_builder_state(storage)
        .instrument(info_span!("init_batch_builder", component = "alpen"))
        .await
        .context("batch builder state initialization should not fail")?;
    let batch_lifecycle = init_lifecycle_state(storage)
        .instrument(info_span!("init_lifecycle", component = "alpen"))
        .await
        .context("batch lifecycle state initialization should not fail")?;

    Ok(SequencerBootState {
        ol_chain_tracker,
        exec_chain,
        batch_builder,
        batch_lifecycle,
    })
}

/// Everything [`launch`] needs, assembled once the reth node has launched.
///
/// `P` is the reth node's state/block/header provider type; kept generic
/// (rather than naming the concrete `FullNode<...>` type) since only three
/// call sites here need it.
pub(crate) struct SequencerLaunchCtx<'a, P> {
    pub(crate) node_provider: P,
    pub(crate) task_executor: TaskExecutor,
    pub(crate) payload_builder_handle: PayloadBuilderHandle<AlpenEngineTypes>,
    pub(crate) beacon_engine_handle: ConsensusEngineHandle<AlpenEngineTypes>,
    pub(crate) ext: &'a AdditionalConfig,
    pub(crate) storage: Arc<EeNodeStorage>,
    pub(crate) dbs: &'a EeDatabases,
    pub(crate) db_handle: Handle,
    pub(crate) preconf_tx: watch::Sender<BlockNumHash>,
    pub(crate) preconf_rx: watch::Receiver<BlockNumHash>,
    pub(crate) consensus_watcher: watch::Receiver<ConsensusHeads>,
    pub(crate) status_watcher: watch::Receiver<OLFinalizedStatus>,
    pub(crate) ol_client: Arc<OLClientKind>,
    pub(crate) genesis_info: AlpenEeGenesisBlockInfo,
    pub(crate) params: Arc<AlpenParams>,
    /// Parsed `SEQUENCER_PRIVATE_KEY`, resolved unconditionally at startup
    /// (gossip signing needs it too), and guaranteed `Some` whenever
    /// `--sequencer` is set.
    pub(crate) sequencer_privkey: Buf32,
}

/// Launches every service that only runs when `--sequencer` is set: the
/// exec chain / OL chain tracker, the DA (btcio) pipeline, the EE chunk +
/// acct provers, and the batch/chunk builder services.
///
/// Resolves the writer config, block-builder config, boot state, and the DA
/// reveal signing keypair itself, so callers only need to know that a node
/// exists and the sequencer flag is set.
pub(crate) async fn launch<P>(
    service_executor: &ServiceExecutor,
    ctx: SequencerLaunchCtx<'_, P>,
) -> eyre::Result<()>
where
    P: StateProviderFactory
        + BlockReader<Block = reth_primitives::Block>
        + HeaderProvider<Header = reth_primitives::Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let SequencerLaunchCtx {
        node_provider,
        task_executor,
        payload_builder_handle,
        beacon_engine_handle,
        ext,
        storage,
        dbs,
        db_handle,
        preconf_tx,
        preconf_rx,
        consensus_watcher,
        status_watcher,
        ol_client,
        genesis_info,
        params,
        sequencer_privkey,
    } = ctx;

    let writer_config = Arc::new(ext.btcio.writer_config()?);
    log_writer_config(&writer_config);
    let block_builder_config = block_builder_config_from_env()?;
    let sequencer_keypair = sequencer_bitcoin_keypair(&sequencer_privkey)?;

    let SequencerBootState {
        ol_chain_tracker: ol_chain_tracker_state,
        exec_chain: exec_chain_state,
        batch_builder: batch_builder_state,
        batch_lifecycle: batch_lifecycle_state,
    } = init_boot_state(storage.as_ref(), ol_client.as_ref()).await?;

    let sequencer_args = &ext.sequencer;

    let payload_engine = Arc::new(AlpenRethPayloadEngine::new(
        payload_builder_handle,
        beacon_engine_handle,
        sequencer_args.beneficiary_address,
        storage.clone(),
    ));

    let exec_chain_handle = services::exec_chain::start_exec_chain_service(
        exec_chain_state,
        preconf_tx.clone(),
        storage.clone(),
        consensus_watcher.clone(),
        service_executor,
    )
    .instrument(info_span!("start_exec_chain", component = "alpen"))
    .await
    .map_err(|e| eyre::eyre!("failed to start exec chain service: {e}"))?;

    let (ol_chain_tracker, ol_chain_tracker_task) = build_ol_chain_tracker(
        ol_chain_tracker_state,
        status_watcher.clone(),
        ol_client.clone(),
        storage.clone(),
    );

    let (latest_batch, _) = require_latest_batch(storage.as_ref())
        .instrument(info_span!("require_latest_batch", component = "alpen"))
        .await?;

    let batch_sealing_policy =
        FixedBlockCountSealing::new(sequencer_args.batch_sealing_block_count);
    let block_data_provider = Arc::new(BlockCountDataProvider);

    // Per-block proof witnesses are captured inline during payload
    // build and persisted by `AlpenRethPayloadEngine`, and the
    // chunk prover's `ChunkSpec::fetch_input` assembles a chunk
    // proof input from those per-block records. There is no
    // chunk-seal extraction step and no chunk-spanning multiproof.

    // Channel from batch builder → chunk builder.
    let (batch_event_tx, batch_event_rx) = mpsc::channel::<BatchBuilderEvent>(
        sequencer_args
            .batch_event_channel_capacity
            .unwrap_or(DEFAULT_BATCH_EVENT_CHANNEL_CAPACITY),
    );

    let (batch_builder_handle, batch_builder_task) = create_batch_builder(
        latest_batch.id(),
        BlockNumHash::new(genesis_info.blockhash().0.into(), genesis_info.blocknum()),
        batch_builder_state,
        preconf_rx,
        block_data_provider,
        batch_sealing_policy,
        storage.clone(),
        storage.clone(),
        exec_chain_handle.clone(),
        Some(batch_event_tx),
    );

    let da_pipeline = da_pipeline::start(
        service_executor,
        &task_executor,
        da_pipeline::DaPipelineInputs {
            da_args: &ext.da,
            btcio_args: &ext.btcio,
            dbs,
            db_handle,
            storage: storage.clone(),
            node_provider: node_provider.clone(),
            params: params.clone(),
            writer_config,
            sequencer_keypair,
        },
    )
    .await?;

    let batch_prover = provers::launch(
        service_executor,
        dbs,
        ol_client.as_ref(),
        provers::EeProverInputs {
            storage: storage.clone(),
            node_provider: node_provider.clone(),
            btc_client: da_pipeline.btc_client,
            dev_native_prover: sequencer_args.dev_native_prover,
            sp1_deadline_secs: sequencer_args.sp1_proof_deadline_secs,
            params: params.clone(),
        },
    )
    .await?;

    let batch_da_provider = da_pipeline.batch_da_provider;

    let (batch_lifecycle_handle, batch_lifecycle_task) = create_batch_lifecycle_task(
        None,
        batch_lifecycle_state,
        batch_builder_handle.latest_batch_watcher(),
        batch_da_provider,
        batch_prover.clone(),
        storage.clone(),
    );

    let update_submitter_task = create_update_submitter_task(
        ol_client,
        storage.clone(),
        storage.clone(),
        batch_prover,
        batch_lifecycle_handle.latest_proof_ready_watcher(),
        status_watcher,
    );

    task_executor.spawn_critical(
        "ol_chain_tracker",
        ol_chain_tracker_task.instrument(info_span!("ol_chain_tracker", component = "alpen")),
    );
    // Per-block proof witnesses are captured inline during payload
    // build (in the EE node's `try_build_payload`) and persisted by
    // the payload engine (`AlpenRethPayloadEngine`) before the
    // payload is returned, so the block builder runs no separate
    // witness step. The chunk prover's `ChunkSpec::fetch_input`
    // assembles a chunk proof input from those per-block records.
    task_executor.spawn_critical(
        "block_assembly",
        block_builder_task(
            block_builder_config,
            exec_chain_handle,
            ol_chain_tracker,
            payload_engine,
            storage.clone(),
        )
        .instrument(info_span!("block_assembly", component = "alpen")),
    );

    // --- Chunk builder service ---
    let chunk_block_count = sequencer_args
        .chunk_sealing_block_count
        .unwrap_or(sequencer_args.batch_sealing_block_count);
    let genesis_blocknumhash =
        BlockNumHash::new(genesis_info.blockhash().0.into(), genesis_info.blocknum());

    // Validate --chunk-sealing-gas-limit if configured.
    //
    // EIP-1559 lets the per-block gas limit drift from genesis by
    // ±1/1024 per block, so the actual block gas limit at runtime
    // may be slightly higher than genesis. We use 2× the genesis
    // gas limit as a conservative floor to accommodate this drift
    // while still catching obvious misconfigurations.
    if let Some(configured) = sequencer_args.chunk_sealing_gas_limit {
        let genesis_gas_limit = params.evm_spec().genesis().gas_limit;
        let min_chunk_gas = genesis_gas_limit.saturating_mul(2);
        eyre::ensure!(
            configured >= min_chunk_gas,
            "--chunk-sealing-gas-limit ({configured}) is below the minimum \
             ({min_chunk_gas}, 2× genesis block gas limit {genesis_gas_limit}). \
             A single block can use up to the per-block gas limit, so the chunk \
             budget must be large enough to always fit at least one block.",
        );
    }

    // u64::MAX effectively disables the gas policy while keeping a
    // single monomorphic code path (no dyn / enum branching).
    let chunk_gas_limit = sequencer_args.chunk_sealing_gas_limit.unwrap_or(u64::MAX);
    let chunk_sealing_policy = OrSealing::new(
        FixedBlockCountSealing::new(chunk_block_count),
        MaxGasSealing::new(chunk_gas_limit),
    );

    services::chunk_builder::start_chunk_builder_service(
        genesis_blocknumhash,
        storage.clone(),
        storage.clone(),
        storage.clone(),
        chunk_sealing_policy,
        RethGasDataProvider::new(node_provider.clone()),
        batch_event_rx,
        service_executor,
    )
    .await
    .map_err(|e| eyre::eyre!("failed to launch chunk builder service: {e}"))?;

    task_executor.spawn_critical(
        "ee_batch_builder",
        batch_builder_task.instrument(info_span!("ee_batch_builder", component = "alpen")),
    );
    task_executor.spawn_critical(
        "ee_batch_lifecycle",
        batch_lifecycle_task.instrument(info_span!("ee_batch_lifecycle", component = "alpen")),
    );
    task_executor.spawn_critical(
        "ee_update_submitter",
        update_submitter_task.instrument(info_span!("ee_update_submitter", component = "alpen")),
    );

    Ok(())
}
