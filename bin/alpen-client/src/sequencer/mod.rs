//! Sequencer-only startup: boot-state init, the DA/btcio pipeline, and the
//! batch/chunk builder services that only run with `--sequencer`.

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
use crate::{
    args::{BtcioArgs, DaArgs, SequencerArgs},
    ol_client::OLClientKind,
    service_executor::ServiceExecutor,
};

/// Environment variable for overriding the default EE block time.
const ALPEN_EE_BLOCK_TIME_MS_ENV_VAR: &str = "ALPEN_EE_BLOCK_TIME_MS";

/// Default capacity for the batch builder → chunk builder event channel.
const DEFAULT_BATCH_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Startup state that only the EE sequencer needs.
///
/// Bundled into one value so it can be gated behind a single runtime
/// `--sequencer` check and carried as a single `Option`.
pub(crate) struct SequencerBootState {
    ol_chain_tracker: OLChainTrackerState,
    exec_chain: ExecChainState,
    batch_builder: BatchBuilderState<BlockCountPolicy>,
    batch_lifecycle: BatchLifecycleState,
}

impl SequencerBootState {
    /// The exec chain tip, used to seed the p2p preconf head watch.
    pub(crate) fn tip_blocknumhash(&self) -> BlockNumHash {
        self.exec_chain.tip_blocknumhash()
    }
}

/// Resolves the btcio writer config up front so flag misuse surfaces before
/// I/O, when running as a sequencer.
pub(crate) fn resolve_writer_config(
    sequencer_enabled: bool,
    btcio_args: &BtcioArgs,
) -> eyre::Result<Option<Arc<WriterConfig>>> {
    if !sequencer_enabled {
        return Ok(None);
    }
    let cfg = Arc::new(btcio_args.writer_config()?);
    log_writer_config(&cfg);
    Ok(Some(cfg))
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

/// Reads `SEQUENCER_PRIVATE_KEY`, required when running with `--sequencer`.
pub(crate) fn sequencer_privkey_from_env(sequencer_enabled: bool) -> eyre::Result<Option<Buf32>> {
    if !sequencer_enabled {
        return Ok(None);
    }

    let privkey_str = env::var("SEQUENCER_PRIVATE_KEY").map_err(|_| {
        eyre::eyre!(
            "SEQUENCER_PRIVATE_KEY environment variable is required when running with --sequencer"
        )
    })?;

    let privkey = privkey_str
        .parse::<Buf32>()
        .map_err(|e| eyre::eyre!("Failed to parse SEQUENCER_PRIVATE_KEY as hex: {e}"))?;

    Ok(Some(privkey))
}

pub(crate) fn sequencer_bitcoin_keypair(privkey: &Buf32) -> eyre::Result<Keypair> {
    let sk = SecretKey::from_slice(privkey.as_ref()).context("invalid sequencer private key")?;
    let secp = Secp256k1::signing_only();
    Ok(Keypair::from_secret_key(&secp, &sk))
}

/// Parses the EE block time override, when running as a sequencer.
pub(crate) fn block_builder_config_from_env(
    sequencer_enabled: bool,
) -> eyre::Result<BlockBuilderConfig> {
    let default_config = BlockBuilderConfig::default();
    if !sequencer_enabled {
        return Ok(default_config);
    }

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

/// Loads sequencer boot state (OL chain tracker, exec chain, batch builder,
/// batch lifecycle), when running as a sequencer.
pub(crate) async fn init_boot_state(
    sequencer_enabled: bool,
    storage: &EeNodeStorage,
    ol_client: &(impl SequencerOLClient + Send + Sync),
) -> eyre::Result<Option<SequencerBootState>> {
    if !sequencer_enabled {
        return Ok(None);
    }

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

    Ok(Some(SequencerBootState {
        ol_chain_tracker,
        exec_chain,
        batch_builder,
        batch_lifecycle,
    }))
}

/// Everything [`launch_sequencer_services`] needs, assembled in `main` after
/// the reth node has launched and sequencer boot state has been loaded.
///
/// `P` is the reth node's state/block/header provider type; kept generic
/// (rather than naming the concrete `FullNode<...>` type) since only three
/// call sites here need it.
pub(crate) struct SequencerLaunchCtx<'a, P> {
    pub(crate) node_provider: P,
    pub(crate) task_executor: TaskExecutor,
    pub(crate) payload_builder_handle: PayloadBuilderHandle<AlpenEngineTypes>,
    pub(crate) beacon_engine_handle: ConsensusEngineHandle<AlpenEngineTypes>,
    pub(crate) block_builder_config: BlockBuilderConfig,
    pub(crate) sequencer_args: &'a SequencerArgs,
    pub(crate) da_args: &'a DaArgs,
    pub(crate) btcio_args: &'a BtcioArgs,
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
    pub(crate) writer_config: Option<Arc<WriterConfig>>,
    pub(crate) sequencer_keypair: Option<Keypair>,
    pub(crate) boot_state: SequencerBootState,
}

/// Launches every service that only runs when `--sequencer` is set: the
/// exec chain / OL chain tracker, the DA (btcio) pipeline, the EE chunk +
/// acct provers, and the batch/chunk builder services.
pub(crate) async fn launch_sequencer_services<P>(
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
        block_builder_config,
        sequencer_args,
        da_args,
        btcio_args,
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
        writer_config,
        sequencer_keypair,
        boot_state:
            SequencerBootState {
                ol_chain_tracker: ol_chain_tracker_state,
                exec_chain: exec_chain_state,
                batch_builder: batch_builder_state,
                batch_lifecycle: batch_lifecycle_state,
            },
    } = ctx;

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
            da_args,
            btcio_args,
            dbs,
            db_handle,
            storage: storage.clone(),
            node_provider: node_provider.clone(),
            params: params.clone(),
            writer_config: writer_config
                .expect("writer_config resolved at startup when --sequencer is set"),
            sequencer_keypair: sequencer_keypair.ok_or_else(|| {
                eyre::eyre!("EE sequencer DA reveal signing needs sequencer Keypair")
            })?,
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
