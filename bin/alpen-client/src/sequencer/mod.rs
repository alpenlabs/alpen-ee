//! Sequencer startup: builds, launches, and runs the reth node, then starts
//! the services that only run in sequencer mode.
//!
//! [`run`] is the sole entry point, the counterpart to
//! [`crate::full_node::run`]. It owns the whole sequencer path: the
//! sequencer-only genesis steps, the reth builder chain (including the DA
//! and witness ExExes), the chainstate sync, and [`start_services`], which
//! brings up the DA/btcio pipeline, the provers, and the batch/chunk
//! builders.

mod bitcoin_fee_rate;
pub(crate) mod da_fee_rate;
mod da_pipeline;
mod gas_data_provider;
mod header_summary;
mod payload_builder;
mod prover;
mod provers;
mod services;

use std::sync::Arc;

use alpen_ee_common::{require_latest_batch, BlockNumHash, SequencerOLClient};
use alpen_ee_database::{EeDb, EeNodeStorage, SequencerDatabases};
use alpen_ee_engine::{sync_chainstate_to_engine, AlpenRethExecEngine};
use alpen_ee_exec_chain::{init_exec_chain_state_from_storage, ExecChainState};
use alpen_ee_genesis::{ensure_batch_genesis, ensure_finalized_exec_chain_genesis};
use alpen_ee_rpc_server::{AlpenEeRpcServer, EeRpcServer};
use alpen_ee_sequencer::{
    block_builder_task, build_ol_chain_tracker, create_batch_builder, create_batch_lifecycle_task,
    create_update_submitter_task, init_batch_builder_state, init_lifecycle_state,
    init_ol_chain_tracker_state,
    sealing_policy::{
        block_count_policy::{BlockCountDataProvider, BlockCountPolicy, FixedBlockCountSealing},
        gas_limit_policy::MaxGasSealing,
        or_policy::{ComposedDataProvider, ComposedPolicy, OrSealing},
        rotation_policy::{RotationDataProvider, RotationPolicy, SealOnRotation},
    },
    BatchBuilderEvent, BatchBuilderState, BatchLifecycleState, BlockBuilderConfig,
    OLChainTrackerState,
};
use alpen_reth_evm::evm::AlpenEvmFactory;
use alpen_reth_exex::{AccessedStateGenerator, StateDiffGenerator};
use alpen_reth_node::{
    AlpenEngineTypes, AlpenEthereumNode, AlpenGossipProtocolHandler, AlpenGossipState,
    AlpenNodeMode,
};
use alpen_reth_rpc::AlpenFeeApiServer;
use bitcoind_async_client::{
    corepc_types::bitcoin::{
        key::Keypair,
        secp256k1::{Secp256k1, SecretKey},
    },
    Client as BtcClient,
};
use eyre::Context;
use reth_chainspec::ChainSpec;
use reth_network::{protocol::IntoRlpxSubProtocol, NetworkProtocols};
use reth_node_builder::{NodeBuilder, NodeTypesWithDB, WithLaunchContext};
use reth_payload_builder::PayloadBuilderHandle;
use reth_provider::{
    providers::{BlockchainProvider, ProviderNodeTypes},
    BlockReader, HeaderProvider, StateProviderFactory,
};
use strata_config::btcio::{fee_rate_to_sat_per_vb, FeePolicy, WriterConfig};
use strata_identifiers::EpochCommitment;
use strata_primitives::buf::Buf32;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, info_span, Instrument};

use self::{gas_data_provider::RethGasDataProvider, payload_builder::AlpenRethPayloadEngine};
use crate::{
    config::SequencerMode,
    gossip::GossipConfig,
    node::{LaunchedNode, NodeBootstrap},
    ol::OLClientKind,
    service_executor::ServiceExecutor,
};

/// What the sequencer path needs from [`crate::node`]'s bootstrap that a
/// full node has no use for: the sled handle its extra databases come from,
/// the OL client behind the tracker, and the genesis epoch both were seeded
/// from.
///
/// Held behind the `sequencer` feature on [`NodeBootstrap`] so a
/// full-node-only build never carries them.
pub(crate) struct BootstrapResources {
    pub(crate) service_executor: ServiceExecutor,
    pub(crate) db: EeDb,
    pub(crate) ol_client: Arc<OLClientKind>,
    pub(crate) genesis_epoch: EpochCommitment,
}

/// Batch sealing pairs the configured block-count cadence with the protocol
/// rule that a predicate rotation ends its batch.
type BatchPolicy = ComposedPolicy<BlockCountPolicy, RotationPolicy>;

/// Startup state that only the EE sequencer needs: the OL chain tracker,
/// exec chain, batch builder, and batch lifecycle states loaded from
/// storage once the node is up.
struct SequencerBootState {
    ol_chain_tracker: OLChainTrackerState,
    exec_chain: ExecChainState,
    batch_builder: BatchBuilderState<BatchPolicy>,
    batch_lifecycle: BatchLifecycleState,
}

/// Bitcoin resources constructed once and shared by fee lookup and DA publication.
struct BtcioResources {
    client: Arc<BtcClient>,
    writer_config: Arc<WriterConfig>,
}

/// The sequencer's exec-chain tip, used to seed the p2p preconf head watch
/// before the reth node (and therefore the engine-control task) is built.
///
/// Only loads the exec-chain piece of boot state; [`start_services`] loads
/// the full [`SequencerBootState`] again once the node is up. Both reads are cheap,
/// local, read-only sled reads with nothing else touching storage in
/// between, so re-reading is simpler than threading a boot-state value
/// across the generic parts of node startup.
async fn initial_preconf_head(storage: &EeNodeStorage) -> eyre::Result<BlockNumHash> {
    let exec_chain = init_exec_chain_state_from_storage(storage)
        .instrument(info_span!(
            "init_exec_chain_head_probe",
            component = "alpen"
        ))
        .await
        .context("exec chain state initialization should not fail")?;
    Ok(exec_chain.tip_blocknumhash())
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
            mempool_base_url: _,
            fallback_conf_target,
        } => {
            info!(
                target: "alpen-client",
                component = "alpen",
                policy = "mempool",
                tier = ?policy,
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

/// Derives the sequencer's gossip pubkey from its private key, rather than
/// taking it as separate config — a sequencer can't be told a pubkey that
/// disagrees with its private key if there's no second value to disagree.
pub(crate) fn sequencer_gossip_pubkey(privkey: &Buf32) -> eyre::Result<Buf32> {
    let keypair = sequencer_bitcoin_keypair(privkey)?;
    let (x_only_pubkey, _parity) = keypair.x_only_public_key();
    Ok(Buf32(x_only_pubkey.serialize()))
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

/// Builds, launches, and runs a sequencer node.
///
/// The sequencer-only genesis steps come first: they have to land before the
/// exec-chain tip can be read back out, and that tip seeds the preconf watch
/// the engine-control task starts from — so both must happen before the reth
/// node is built.
pub(crate) async fn run(
    builder: WithLaunchContext<NodeBuilder<Arc<reth_db::DatabaseEnv>, ChainSpec>>,
    common: NodeBootstrap,
    mode: &SequencerMode,
    privkey: Buf32,
) -> eyre::Result<()> {
    // Creating these also creates their sled trees, which is why it happens
    // here and not in bootstrap: a full node should never materialize them.
    let sequencer_dbs = common
        .sequencer
        .db
        .sequencer_databases()
        .context("failed to open sequencer databases")?;

    // Account-state genesis is common to every node and already done during
    // bootstrap; exec-chain and batch genesis only matter once a sequencer
    // is producing blocks.
    ensure_finalized_exec_chain_genesis(
        common.params.as_ref(),
        common.sequencer.genesis_epoch.to_block_commitment(),
        common.storage.as_ref(),
    )
    .instrument(info_span!("ensure_exec_chain_genesis", component = "alpen"))
    .await
    .context("genesis should not fail")?;
    ensure_batch_genesis(common.params.as_ref(), common.storage.as_ref())
        .instrument(info_span!("ensure_batch_genesis", component = "alpen"))
        .await
        .context("genesis should not fail")?;

    // The sequencer's real exec-chain tip, readable only after the genesis
    // steps above. Seeding the preconf channel with anything else would
    // start the engine-control task from the wrong fork-choice head.
    let initial_preconf_head = initial_preconf_head(common.storage.as_ref()).await?;

    let sequencer_config = &mode.config;
    let writer_config = Arc::new(WriterConfig {
        l1_fee_policy_config: sequencer_config.l1_fee_policy.clone(),
        ..Default::default()
    });
    log_writer_config(&writer_config);
    let btc_client = da_pipeline::connect_bitcoin(&sequencer_config.bitcoind).await?;
    let da_fee_rate_controller = da_fee_rate::controller_from_config(
        &sequencer_config.da_fee_rate,
        btc_client.clone(),
        sequencer_config.l1_fee_policy.clone(),
    )?;
    let da_fee_rate_handle = da_fee_rate_controller.start(&common.sequencer.service_executor);
    let btcio = BtcioResources {
        client: btc_client,
        writer_config,
    };

    let evm_factory = AlpenEvmFactory::from_bridge_params(common.params.bridge_params());
    let node = AlpenEthereumNode::new(
        evm_factory,
        common.params.evm_spec().clone(),
        AlpenNodeMode::sequencer(),
        da_fee_rate_handle,
    );

    let consensus_watcher = common.ol_tracker.consensus_watcher();

    // Create gossip channel before building the node so we can register it early
    let (gossip_tx, gossip_rx) = mpsc::unbounded_channel();
    let (preconf_tx, preconf_rx) = watch::channel(initial_preconf_head);

    let handle = builder
        .node(node)
        // Register Alpen gossip RLPx subprotocol
        .on_component_initialized(move |node| {
            // Add the custom RLPx subprotocol before node fully starts
            // See: crates/reth/node/src/gossip/
            let handler = AlpenGossipProtocolHandler::new(AlpenGossipState::new(gossip_tx));
            node.components
                .network
                .add_rlpx_sub_protocol(handler.into_rlpx_sub_protocol());
            info!(target: "alpen-gossip", component = "alpen", "Registered Alpen gossip RLPx subprotocol");
            Ok(())
        })
        // Install state diff exex for sequencer DA.
        // The exex persists per-block state diffs that the blob provider reads.
        .install_exex("state_diffs", {
            let state_diff_db = sequencer_dbs.witness_db();
            |ctx| async { Ok(StateDiffGenerator::new(ctx, state_diff_db).start()) }
        })
        // Per-block accessed-state capture. The CHUNK proof's witness is
        // now produced inline during payload build (see the EE node's
        // `try_build_payload` / `AlpenRethPayloadEngine`); this exex
        // remains only to feed the ACCOUNT proof's batch-range witness
        // (`RangeWitnessExtractor` reads `AccessedStateStore`).
        // TODO(STR-4157): retire this exex once the account proof's
        // witness is assembled from inline per-block witnesses too.
        .install_exex("accessed_state", {
            let accessed_state_store = common.storage.clone();
            |ctx| async { Ok(AccessedStateGenerator::new(ctx, accessed_state_store).start()) }
        })
        .extend_rpc_modules({
            let consensus_watcher = consensus_watcher.clone();
            let storage = common.storage.clone();
            move |ctx| {
                let provider = ctx.provider().clone();
                let ee_rpc_server = EeRpcServer::new(
                    provider,
                    consensus_watcher,
                    storage.clone(),
                    storage.clone(),
                );
                ctx.modules.merge_configured(ee_rpc_server.into_rpc())?;

                // Register `alpen_estimateFees` (execution + DA fee quote) on the
                // configured eth API, which carries the simulation + state access.
                let fee_api = ctx.registry.eth_api().clone();
                ctx.modules
                    .merge_configured(AlpenFeeApiServer::into_rpc(fee_api))?;
                Ok(())
            }
        })
        .launch()
        .await?;
    info!(target: "alpen-client", component = "alpen", "installed StateDiffGenerator exex for DA");
    info!(target: "alpen-client", component = "alpen", "installed AccessedStateGenerator exex (account-proof range witness)");

    let node = &handle.node;

    // Sync chainstate to engine before starting other tasks
    let engine = AlpenRethExecEngine::new(node.beacon_engine_handle.clone());
    let sync_result = sync_chainstate_to_engine(common.storage.as_ref(), &node.provider, &engine)
        .instrument(info_span!("chainstate_sync", component = "alpen"))
        .await;
    if let Err(e) = sync_result {
        error!(target: "alpen-client", component = "alpen", error = ?e, "failed to sync chainstate to engine on startup");
        return Err(eyre::eyre!("chainstate sync failed: {e}"));
    }
    info!(target: "alpen-client", component = "alpen", "chainstate sync completed successfully");

    // Built once and borrowed by both callees below, rather than cloning the
    // same handles into two bundles.
    let launched = LaunchedNode {
        provider: node.provider.clone(),
        task_executor: node.task_executor.clone(),
        beacon_engine_handle: node.beacon_engine_handle.clone(),
        preconf_tx,
        preconf_rx,
    };

    launched.spawn_shared_tasks(
        consensus_watcher,
        gossip_rx,
        // The sequencer's gossip pubkey is *derived* from its private key,
        // not taken as separate config. Contrast `GossipConfig::full_node`,
        // which is told the pubkey.
        GossipConfig::sequencer(privkey)?,
    );

    start_services(
        &common,
        mode,
        privkey,
        &sequencer_dbs,
        &launched,
        node.payload_builder_handle.clone(),
        btcio,
    )
    .await?;

    common.run_until_exit(handle.node_exit_future).await
}

/// Launches every service that only runs in sequencer mode: the exec chain /
/// OL chain tracker, the DA (btcio) pipeline, the EE chunk + acct provers,
/// and the batch/chunk builder services.
///
/// Resolves the block-builder config and boot state itself. The shared
/// Bitcoin client and writer config arrive from [`run`], which creates them
/// before the payload builder and fee-rate controller are launched.
async fn start_services<N>(
    common: &NodeBootstrap,
    mode: &SequencerMode,
    privkey: Buf32,
    sequencer_dbs: &SequencerDatabases,
    launched: &LaunchedNode<N>,
    payload_builder_handle: PayloadBuilderHandle<AlpenEngineTypes>,
    btcio: BtcioResources,
) -> eyre::Result<()>
where
    N: NodeTypesWithDB + ProviderNodeTypes,
    BlockchainProvider<N>: StateProviderFactory
        + BlockReader<Block = reth_primitives::Block>
        + HeaderProvider<Header = reth_primitives::Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let LaunchedNode {
        provider: node_provider,
        task_executor,
        beacon_engine_handle,
        preconf_tx,
        preconf_rx,
    } = launched;

    let sequencer_config = &mode.config;
    let BtcioResources {
        client: btc_client,
        writer_config,
    } = btcio;

    let NodeBootstrap {
        storage,
        params,
        ol_tracker,
        sequencer:
            BootstrapResources {
                service_executor,
                ol_client,
                ..
            },
        ..
    } = common;
    let consensus_watcher = ol_tracker.consensus_watcher();
    let status_watcher = ol_tracker.ol_status_watcher();
    let genesis_info = params.genesis_block_info();
    let genesis_blocknumhash =
        BlockNumHash::new(genesis_info.blockhash().0.into(), genesis_info.blocknum());

    let block_builder_config =
        BlockBuilderConfig::default().with_blocktime_ms(sequencer_config.blocktime_ms.get());

    let sequencer_keypair = sequencer_bitcoin_keypair(&privkey)?;

    let SequencerBootState {
        ol_chain_tracker: ol_chain_tracker_state,
        exec_chain: exec_chain_state,
        batch_builder: batch_builder_state,
        batch_lifecycle: batch_lifecycle_state,
    } = init_boot_state(storage.as_ref(), ol_client.as_ref()).await?;

    let payload_engine = Arc::new(AlpenRethPayloadEngine::new(
        payload_builder_handle,
        beacon_engine_handle.clone(),
        sequencer_config.beneficiary_address,
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

    // A rotation-consuming block must end its batch, so the block-count
    // cadence is OR'd with the rotation rule rather than special-cased in the
    // batch builder.
    let batch_sealing_policy = OrSealing::new(
        FixedBlockCountSealing::new(sequencer_config.batch_sealing_block_count),
        SealOnRotation,
    );
    let block_data_provider = Arc::new(ComposedDataProvider::new(
        BlockCountDataProvider,
        RotationDataProvider::new(storage.clone()),
    ));

    // Per-block proof witnesses are captured inline during payload
    // build and persisted by `AlpenRethPayloadEngine`, and the
    // chunk prover's `ChunkSpec::fetch_input` assembles a chunk
    // proof input from those per-block records. There is no
    // chunk-seal extraction step and no chunk-spanning multiproof.

    // Channel from batch builder → chunk builder.
    let (batch_event_tx, batch_event_rx) =
        mpsc::channel::<BatchBuilderEvent>(sequencer_config.batch_event_channel_capacity.get());

    let (batch_builder_handle, batch_builder_task) = create_batch_builder(
        latest_batch.id(),
        genesis_blocknumhash,
        batch_builder_state,
        preconf_rx.clone(),
        block_data_provider,
        batch_sealing_policy,
        storage.clone(),
        storage.clone(),
        exec_chain_handle.clone(),
        Some(batch_event_tx),
    );

    let da_pipeline = da_pipeline::start(
        service_executor,
        task_executor,
        da_pipeline::DaPipelineInputs {
            btc_client: btc_client.clone(),
            l1_reorg_safe_depth: mode.l1_reorg_safe_depth,
            genesis_l1_height: mode.genesis_l1_height,
            dbs: sequencer_dbs,
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
        sequencer_dbs,
        ol_client.as_ref(),
        provers::EeProverInputs {
            storage: storage.clone(),
            node_provider: node_provider.clone(),
            btc_client,
            backend: sequencer_config.prover.clone(),
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
        ol_client.clone(),
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
    let chunk_block_count = sequencer_config.chunk_sealing_block_count();

    // sequencer.chunk_sealing_gas_limit is validated against the genesis gas
    // limit in `node::launch`, before any node/DB/OL startup work.

    // u64::MAX effectively disables the gas policy while keeping a
    // single monomorphic code path (no dyn / enum branching).
    let chunk_gas_limit = sequencer_config.chunk_sealing_gas_limit.unwrap_or(u64::MAX);
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
