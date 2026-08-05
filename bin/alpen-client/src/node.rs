//! Reth node bootstrap and launch.
//!
//! Startup runs in three phases. [`bootstrap_node`] resolves the resources
//! every node needs, whatever its mode: health check server, database and
//! storage, the OL client, and the OL tracker. [`resolve_mode_setup`] then
//! resolves the handful of things that actually differ between a full node
//! and a sequencer. [`run_node`] takes both and builds, launches, and runs
//! the reth node along a single shared path.

use std::sync::Arc;

use alpen_ee_common::{chain_status_checked, BlockNumHash, OLClient};
use alpen_ee_database::{open_ee_db, EeNodeStorage, SequencerDatabases};
#[cfg(feature = "sequencer")]
use alpen_ee_engine::sync_chainstate_to_engine;
use alpen_ee_engine::{create_engine_control_task, AlpenRethExecEngine};
use alpen_ee_genesis::ensure_genesis_ee_account_state;
#[cfg(feature = "sequencer")]
use alpen_ee_genesis::{ensure_batch_genesis, ensure_finalized_exec_chain_genesis};
use alpen_ee_ol_tracker::init_ol_tracker_state;
use alpen_ee_params::AlpenParams;
use alpen_ee_rpc_server::{AlpenEeRpcServer, EeRpcServer};
use alpen_reth_evm::evm::AlpenEvmFactory;
#[cfg(feature = "sequencer")]
use alpen_reth_exex::{AccessedStateGenerator, StateDiffGenerator};
use alpen_reth_node::{
    AlpenEthereumNode, AlpenGossipProtocolHandler, AlpenGossipState, AlpenNodeMode,
};
use eyre::Context;
use jsonrpsee::server::ServerHandle;
use reth_chainspec::ChainSpec;
use reth_network::{protocol::IntoRlpxSubProtocol, NetworkProtocols};
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_provider::CanonStateSubscriptions;
use strata_common::healthz::{start_health_check_server, HealthCheckState};
use strata_identifiers::{EpochCommitment, OLBlockId};
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};
#[cfg(feature = "sequencer")]
use tracing::error;
use tracing::{info, info_span, Instrument};

#[cfg(feature = "sequencer")]
use crate::args::sequencer_privkey_from_env;
#[cfg(feature = "sequencer")]
use crate::sequencer;
use crate::{
    args::{ol_submit_bearer_token_from_env, AdditionalConfig},
    config::{AlpenClientConfig, NodeMode, OlSource},
    gossip::{create_gossip_task, GossipConfig},
    ol::{DummyOLClient, OLClientKind, RpcOLClient},
    service_executor::ServiceExecutor,
    services,
};

pub(crate) async fn launch(
    builder: WithLaunchContext<NodeBuilder<Arc<reth_db::DatabaseEnv>, ChainSpec>>,
    ext: AdditionalConfig,
) -> eyre::Result<()> {
    let alpen_config = ext.alpen_config.clone();
    let params = ext.alpen_params.clone();

    // `params` is safe to log in full: it's public consensus data (account
    // id, bridge params, chain spec), not secrets. `alpen_config` isn't —
    // `NodeMode::Sequencer` carries `bitcoind: BitcoindConfig`, whose
    // `rpc_password` has a plain derived `Debug` with no redaction — so we
    // only pull out the one field worth a human glancing at logs.
    info!(
        target: "alpen-client", component = "alpen",
        ?params,
        sequencer = !matches!(alpen_config.mode, NodeMode::FullNode(_)),
        "Starting EE Node",
    );

    let common = bootstrap_node(&builder, &alpen_config, &params).await?;
    let mode_setup = resolve_mode_setup(&alpen_config.mode, &common).await?;

    run_node(builder, common, mode_setup, &alpen_config).await
}

/// Resources both a full node and a sequencer need, resolved once up front:
/// health check server, chain params, database/storage, the OL client and
/// its genesis epoch, and the (already-started) OL tracker, plus the
/// account-state genesis check common to every node. Exec-chain and batch
/// genesis are sequencer-only and are handled in [`resolve_mode_setup`]
/// instead.
///
/// Handed whole to [`sequencer::launch`] rather than unpacked field by field
/// at the call site: everything the sequencer needs from node startup is
/// already here, so passing the struct keeps that list in one place instead
/// of restating it as a dozen arguments.
#[cfg_attr(
    not(feature = "sequencer"),
    expect(
        dead_code,
        reason = "service_executor/dbs/ol_client/genesis_epoch are only read on the sequencer path"
    )
)]
pub(crate) struct NodeBootstrap {
    pub(crate) service_executor: ServiceExecutor,
    health_check_state: HealthCheckState,
    _health_check_handle: ServerHandle,
    pub(crate) params: Arc<AlpenParams>,
    pub(crate) dbs: SequencerDatabases,
    pub(crate) storage: Arc<EeNodeStorage>,
    pub(crate) ol_client: Arc<OLClientKind>,
    genesis_epoch: EpochCommitment,
    /// Kept as the handle (not pre-extracted watchers) so each mode pulls
    /// exactly the watchers it needs, where it needs them — a full node
    /// never reads `ol_status_watcher` at all, for instance.
    pub(crate) ol_tracker: services::ol_tracker::OLTrackerHandle,
}

async fn bootstrap_node(
    builder: &WithLaunchContext<NodeBuilder<Arc<reth_db::DatabaseEnv>, ChainSpec>>,
    alpen_config: &AlpenClientConfig,
    params: &Arc<AlpenParams>,
) -> eyre::Result<NodeBootstrap> {
    let service_executor = ServiceExecutor::from_reth(builder.task_executor().clone());
    let (health_check_state, _health_check_handle) = start_health_check(
        &alpen_config.health_check_host,
        alpen_config.health_check_port,
    )
    .await?;

    let datadir = builder.config().datadir().data_dir().to_path_buf();

    // OL client resolution validates the OL config synchronously before its
    // one network call, so a missing/invalid setting also fails here, before
    // the database is touched below.
    let (ol_client, genesis_epoch) = resolve_ol_client(alpen_config, params.as_ref()).await?;

    // --- INITIALIZE STATE ---

    let db = open_ee_db(&datadir, alpen_config.db_retry_count)
        .context("failed to load alpen database")?;

    let storage: Arc<_> = db
        .node_storage(Handle::current())
        .context("failed to open EE node storage")?
        .into();

    let dbs = db
        .sequencer_databases()
        .context("failed to open sequencer databases")?;

    ensure_genesis_ee_account_state(params.as_ref(), &genesis_epoch, storage.as_ref())
        .instrument(info_span!("ensure_genesis", component = "alpen"))
        .await
        .context("genesis should not fail")?;

    let ol_chain_status = chain_status_checked(ol_client.as_ref())
        .instrument(info_span!("chain_status_check", component = "alpen"))
        .await
        .context("cannot fetch OL chain status")?;

    let ol_tracker_state = init_ol_tracker_state(ol_chain_status, storage.as_ref())
        .instrument(info_span!("init_ol_tracker", component = "alpen"))
        .await
        .context("ol tracker state initialization should not fail")?;

    let ol_tracker = services::ol_tracker::start_ol_tracker_service(
        ol_tracker_state,
        genesis_epoch.epoch(),
        storage.clone(),
        ol_client.clone(),
        alpen_config.ol.epoch_tracking_mode,
        &service_executor,
    )
    .await
    .map_err(|e| eyre::eyre!("failed to start ol tracker service: {e}"))?;

    Ok(NodeBootstrap {
        service_executor,
        health_check_state,
        _health_check_handle,
        params: params.clone(),
        dbs,
        storage,
        ol_client,
        genesis_epoch,
        ol_tracker,
    })
}

/// The values that differ between a full node and a sequencer and are
/// needed before the reth node exists: two the builder consumes, and the
/// head that seeds the preconf channel.
///
/// Whether a node is a sequencer is *not* recorded here — [`run_node`]
/// reads that off [`NodeMode`] directly, which keeps this struct to
/// resolved values rather than a mix of values and mode flags.
struct ModeSetup {
    /// Plain data until it is handed to the reth builder, so it can be built
    /// here alongside the other mode-dependent values.
    node: AlpenEthereumNode,
    gossip_config: GossipConfig,
    /// The two modes have genuinely different notions of where the chain
    /// starts, so this is resolved per mode rather than derived in
    /// [`run_node`] — see the comments at each construction site below.
    initial_preconf_head: BlockNumHash,
}

/// Resolves the mode-dependent setup, including the sequencer-only genesis
/// steps: exec-chain and batch genesis have to land before the sequencer's
/// preconf head can be read back out of the exec chain, and both have to
/// happen before the reth node is built.
async fn resolve_mode_setup(mode: &NodeMode, common: &NodeBootstrap) -> eyre::Result<ModeSetup> {
    let evm_factory = AlpenEvmFactory::from_bridge_params(common.params.bridge_params());

    match mode {
        NodeMode::FullNode(fc) => {
            // A full node has no chain of its own to start from. Gossip
            // overwrites this as soon as the first message arrives, so the
            // tracker's current head with block number 0 is a fine seed.
            let confirmed_head = common.ol_tracker.consensus_watcher().borrow().confirmed;

            Ok(ModeSetup {
                node: AlpenEthereumNode::new(
                    evm_factory,
                    AlpenNodeMode::full_node(fc.sequencer_http_url.clone()),
                ),
                gossip_config: GossipConfig::full_node(fc.sequencer_pubkey),
                initial_preconf_head: BlockNumHash::new(confirmed_head, 0),
            })
        }

        // NOTE: ATM we reuse `SEQUENCER_PRIVATE_KEY` for both gossip package
        // signing and EE DA reveal tapscript signing. That is operationally
        // convenient for now, but it couples network identity with Bitcoin DA
        // spend authority. Should we split this into a dedicated DA reveal
        // signing key/config?
        //
        // The sequencer's gossip pubkey is *derived* from this key, not taken
        // as separate config — `GossipConfig::sequencer` does the derivation.
        // Contrast the full node above, which is told the pubkey.
        #[cfg(feature = "sequencer")]
        NodeMode::Sequencer(_) => {
            let privkey = sequencer_privkey_from_env()?;

            // Account-state genesis is common to every node and already done
            // in `bootstrap_node`; exec-chain and batch genesis only matter
            // once a sequencer is producing blocks.
            ensure_finalized_exec_chain_genesis(
                common.params.as_ref(),
                common.genesis_epoch.to_block_commitment(),
                common.storage.as_ref(),
            )
            .instrument(info_span!("ensure_exec_chain_genesis", component = "alpen"))
            .await
            .context("genesis should not fail")?;
            ensure_batch_genesis(common.params.as_ref(), common.storage.as_ref())
                .instrument(info_span!("ensure_batch_genesis", component = "alpen"))
                .await
                .context("genesis should not fail")?;

            // The sequencer's real exec-chain tip, readable only after the
            // genesis steps above. Seeding the preconf channel with anything
            // else would start the engine-control task from the wrong
            // fork-choice head.
            let initial_preconf_head =
                sequencer::initial_preconf_head(common.storage.as_ref()).await?;

            Ok(ModeSetup {
                node: AlpenEthereumNode::new(evm_factory, AlpenNodeMode::sequencer()),
                gossip_config: GossipConfig::sequencer(privkey)?,
                initial_preconf_head,
            })
        }
    }
}

/// Builds and launches the reth node, starts the tasks that run alongside
/// it, and blocks until the node exits.
///
/// This is one function rather than a set of helpers because reth's builder
/// changes generic type at `.node(...)`, and every step after it returns
/// `Self`. A single chain therefore never has to name those deeply generic
/// types, whereas any helper taking or returning the builder would.
///
/// A sequencer differs from a full node in exactly three places here, all
/// keyed off the single `seq_config` binding below: the DA/witness ExExes
/// installed before launch, the chainstate sync right after it, and the
/// sequencer services started at the end.
async fn run_node(
    builder: WithLaunchContext<NodeBuilder<Arc<reth_db::DatabaseEnv>, ChainSpec>>,
    common: NodeBootstrap,
    mode_setup: ModeSetup,
    #[cfg_attr(
        not(feature = "sequencer"),
        expect(
            unused_variables,
            reason = "only the sequencer path below reads the config"
        )
    )]
    alpen_config: &AlpenClientConfig,
) -> eyre::Result<()> {
    let node = mode_setup.node;
    let gossip_config = mode_setup.gossip_config;

    // The one place the mode is read after `resolve_mode_setup`. Everything
    // downstream branches on this binding, never on `NodeMode` again — the
    // sequencer config itself is read by `sequencer::launch` off the config
    // it is handed, so it doesn't need to be pulled out here.
    #[cfg(feature = "sequencer")]
    let is_sequencer = matches!(alpen_config.mode, NodeMode::Sequencer(_));

    let consensus_watcher = common.ol_tracker.consensus_watcher();

    // Create gossip channel before building the node so we can register it early
    let (gossip_tx, gossip_rx) = mpsc::unbounded_channel();

    // Preconf channel for p2p head block gossip -> engine control
    // integration, seeded per mode by `resolve_mode_setup`.
    let (preconf_tx, preconf_rx) = watch::channel(mode_setup.initial_preconf_head);

    let mut node_builder = builder
        .node(node)
        // Register Alpen gossip RLPx subprotocol
        .on_component_initialized({
            let gossip_tx = gossip_tx.clone();
            move |node| {
                // Add the custom RLPx subprotocol before node fully starts
                // See: crates/reth/node/src/gossip/
                let handler = AlpenGossipProtocolHandler::new(AlpenGossipState::new(gossip_tx));
                node.components
                    .network
                    .add_rlpx_sub_protocol(handler.into_rlpx_sub_protocol());
                info!(target: "alpen-gossip", component = "alpen", "Registered Alpen gossip RLPx subprotocol");
                Ok(())
            }
        });

    #[cfg(feature = "sequencer")]
    if is_sequencer {
        // Install state diff exex for sequencer DA.
        // The exex persists per-block state diffs that the blob provider reads.
        node_builder = node_builder.install_exex("state_diffs", {
            let state_diff_db = common.dbs.witness_db();
            |ctx| async { Ok(StateDiffGenerator::new(ctx, state_diff_db).start()) }
        });
        info!(target: "alpen-client", component = "alpen", "installed StateDiffGenerator exex for DA");

        // Per-block accessed-state capture. The CHUNK proof's witness is
        // now produced inline during payload build (see the EE node's
        // `try_build_payload` / `AlpenRethPayloadEngine`); this exex
        // remains only to feed the ACCOUNT proof's batch-range witness
        // (`RangeWitnessExtractor` reads `AccessedStateStore`). Retiring
        // it is a separate acct-proof migration tracked as follow-up
        // work to STR-3649.
        node_builder = node_builder.install_exex("accessed_state", {
            let accessed_state_store = common.storage.clone();
            |ctx| async { Ok(AccessedStateGenerator::new(ctx, accessed_state_store).start()) }
        });
        info!(target: "alpen-client", component = "alpen", "installed AccessedStateGenerator exex (account-proof range witness)");
    }

    node_builder = node_builder.extend_rpc_modules({
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
            Ok(())
        }
    });

    let handle = node_builder.launch().await?;
    let node = &handle.node;

    // Sync chainstate to engine before starting other tasks
    #[cfg(feature = "sequencer")]
    if is_sequencer {
        let engine = AlpenRethExecEngine::new(node.beacon_engine_handle.clone());

        let sync_result =
            sync_chainstate_to_engine(common.storage.as_ref(), &node.provider, &engine)
                .instrument(info_span!("chainstate_sync", component = "alpen"))
                .await;

        if let Err(e) = sync_result {
            error!(target: "alpen-client", component = "alpen", error = ?e, "failed to sync chainstate to engine on startup");
            return Err(eyre::eyre!("chainstate sync failed: {e}"));
        }

        info!(target: "alpen-client", component = "alpen", "chainstate sync completed successfully");
    }

    // Tasks every node runs. The preconf handles are cloned rather than moved
    // because the sequencer services below need them too; the extra clones
    // live as long as this function, which runs for the whole node lifetime
    // either way.
    let engine_control_task = create_engine_control_task(
        preconf_rx.clone(),
        consensus_watcher,
        node.provider.clone(),
        AlpenRethExecEngine::new(node.beacon_engine_handle.clone()),
    );

    // Subscribe to canonical state notifications for broadcasting new blocks
    let state_events = node.provider.subscribe_to_canonical_state();

    // Create gossip task for broadcasting new blocks
    let gossip_task =
        create_gossip_task(gossip_rx, state_events, preconf_tx.clone(), gossip_config);

    // Spawn critical tasks
    node.task_executor.spawn_critical(
        "engine_control",
        engine_control_task.instrument(info_span!("engine_control", component = "alpen")),
    );
    node.task_executor.spawn_critical(
        "gossip_task",
        gossip_task.instrument(info_span!("gossip_task", component = "alpen")),
    );

    #[cfg(feature = "sequencer")]
    if is_sequencer {
        sequencer::launch(
            &common,
            alpen_config,
            sequencer::RethNodeParts {
                node_provider: node.provider.clone(),
                task_executor: node.task_executor.clone(),
                payload_builder_handle: node.payload_builder_handle.clone(),
                beacon_engine_handle: node.beacon_engine_handle.clone(),
                preconf_tx,
                preconf_rx,
            },
        )
        .await?;
    }

    common.health_check_state.mark_ready();
    handle.node_exit_future.await
}

/// Starts the HTTP health check server.
///
/// Returns both the status handle (for `mark_ready` once the node is up)
/// and the server's [`ServerHandle`]. The `ServerHandle` must be kept alive
/// by the caller for as long as the server should keep running: dropping it
/// closes the watch channel the server's accept loop stops on.
async fn start_health_check(
    health_check_host: &str,
    health_check_port: u16,
) -> eyre::Result<(HealthCheckState, ServerHandle)> {
    let health_check_state = HealthCheckState::new();
    let health_check_addr = format!("{health_check_host}:{health_check_port}");
    let health_check_handle =
        start_health_check_server(health_check_addr.clone(), health_check_state.clone())
            .instrument(info_span!("start_health_check_server", component = "alpen"))
            .await
            .context("failed to start health check server")?;
    info!(target: "alpen-client", component = "alpen", %health_check_addr, "health check server started");

    Ok((health_check_state, health_check_handle))
}

/// Resolves the OL client (dummy or real RPC) and fetches its genesis epoch
/// commitment.
async fn resolve_ol_client(
    alpen_config: &AlpenClientConfig,
    params: &AlpenParams,
) -> eyre::Result<(Arc<OLClientKind>, EpochCommitment)> {
    let is_sequencer = !matches!(alpen_config.mode, NodeMode::FullNode(_));
    let ol_client = match &alpen_config.ol.source {
        OlSource::Dummy => {
            use strata_identifiers::Buf32;
            use strata_primitives::EpochCommitment;
            let genesis_epoch = EpochCommitment::new(0, 0, OLBlockId::from(Buf32([1; 32])));
            info!(target: "alpen-client", component = "alpen", "Using dummy OL client (no real OL connection)");
            OLClientKind::Dummy(DummyOLClient { genesis_epoch })
        }
        OlSource::Rpc {
            client_url,
            submit_url,
        } => {
            // `ol.submit_url` required-when-sequencer is already enforced by
            // `AlpenClientConfig`'s `TryFrom` at config-parse time; the
            // bearer token authenticating it is a secret, read from the
            // environment here rather than stored in the config file.
            let submit_bearer_token = if is_sequencer && submit_url.is_some() {
                Some(ol_submit_bearer_token_from_env()?)
            } else {
                None
            };
            OLClientKind::Rpc(
                RpcOLClient::try_new(
                    params.strata_exec_account_id(),
                    client_url,
                    submit_url.as_deref(),
                    submit_bearer_token.as_deref(),
                )
                .map_err(|e| eyre::eyre!("failed to create OL client: {e}"))?,
            )
        }
    };
    let ol_client = Arc::new(ol_client);

    // Fetch the genesis epoch commitment from the OL client once at startup.
    let genesis_epoch = ol_client
        .account_genesis_epoch()
        .instrument(info_span!("account_genesis_epoch", component = "alpen"))
        .await
        .context("failed to fetch account genesis epoch from OL")?;

    Ok((ol_client, genesis_epoch))
}
