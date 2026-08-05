//! Reth node bootstrap: health check, config/storage/genesis setup, OL
//! client resolution, node building and launch, and full-node task
//! spawning. Hands off to [`sequencer`] once the node
//! exists, when running in sequencer mode.

use std::sync::Arc;

use alpen_ee_common::{
    chain_status_checked, BatchStorage, BlockNumHash, ExecBlockStorage, OLClient, Storage,
};
use alpen_ee_config::AlpenEeConfig;
use alpen_ee_database::init_db_storage;
use alpen_ee_engine::{create_engine_control_task, sync_chainstate_to_engine, AlpenRethExecEngine};
use alpen_ee_genesis::{
    ensure_batch_genesis, ensure_finalized_exec_chain_genesis, ensure_genesis_ee_account_state,
};
use alpen_ee_ol_tracker::init_ol_tracker_state;
use alpen_ee_rpc_server::{AlpenEeRpcServer, EeRpcServer};
use alpen_reth_evm::evm::AlpenEvmFactory;
#[cfg(feature = "sequencer")]
use alpen_reth_exex::{AccessedStateGenerator, StateDiffGenerator};
use alpen_reth_node::{
    args::AlpenNodeArgs, AlpenEthereumNode, AlpenGossipProtocolHandler, AlpenGossipState,
};
use eyre::Context;
use jsonrpsee::server::ServerHandle;
use reth_chainspec::ChainSpec;
use reth_network::{protocol::IntoRlpxSubProtocol, NetworkProtocols};
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_provider::CanonStateSubscriptions;
use strata_common::healthz::{start_health_check_server, HealthCheckState};
#[cfg(feature = "sequencer")]
use strata_config::btcio::WriterConfig;
use strata_identifiers::{EpochCommitment, OLBlockId};
use strata_predicate::PredicateKey;
use strata_primitives::buf::Buf32;
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};
use tracing::{error, info, info_span, Instrument};

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
    let is_sequencer = !matches!(alpen_config.mode, NodeMode::FullNode(_));
    let service_executor = ServiceExecutor::from_reth(builder.task_executor().clone());
    let (health_check_state, _health_check_handle) = start_health_check(
        &alpen_config.health_check_host,
        alpen_config.health_check_port,
    )
    .await?;

    // --- CONFIGS ---
    let datadir = builder.config().datadir().data_dir().to_path_buf();

    let params = ext.alpen_params.clone();
    let genesis_info = params.genesis_block_info();

    info!(target: "alpen-client", component = "alpen", blockhash=%genesis_info.blockhash(), "EE genesis info");
    let bridge_params = *params.bridge_params();
    info!(
        target: "alpen-client", component = "alpen",
        account_id = ?params.strata_exec_account_id(),
        ?bridge_params,
        sequencer = is_sequencer,
        "Starting EE Node",
    );

    // OL client URL is not used when the dummy OL client is enabled
    let ol_client_url = match &alpen_config.ol.source {
        OlSource::Dummy => String::new(),
        OlSource::Rpc { client_url, .. } => client_url.clone(),
    };

    // Sequencer-only fields (tx-forward target, or None for a sequencer,
    // which never forwards to itself) resolved once, up front.
    let sequencer_http_url = match &alpen_config.mode {
        NodeMode::FullNode(fc) => fc.sequencer_http_url.clone(),
        #[cfg(feature = "sequencer")]
        NodeMode::Sequencer(_) => None,
    };

    let config = Arc::new(AlpenEeConfig::new(
        params.clone(),
        PredicateKey::always_accept(),
        ol_client_url,
        sequencer_http_url.clone(),
        Some(alpen_config.db_retry_count),
    ));

    // NOTE: ATM we reuse `SEQUENCER_PRIVATE_KEY` for both gossip
    // package signing and EE DA reveal tapscript signing. That is
    // operationally convenient for now, but it couples network
    // identity with Bitcoin DA spend authority. Should we split this
    // into a dedicated DA reveal signing key/config?
    //
    // The sequencer's gossip pubkey is *derived* from this key, not taken as
    // separate config — see `sequencer::sequencer_gossip_pubkey`'s doc comment.
    #[cfg_attr(not(feature = "sequencer"), allow(unused_variables))]
    let (gossip_config, sequencer_privkey): (GossipConfig, Option<Buf32>) = match &alpen_config.mode
    {
        NodeMode::FullNode(fc) => (
            GossipConfig::FullNode {
                sequencer_pubkey: fc.sequencer_pubkey,
            },
            None,
        ),
        #[cfg(feature = "sequencer")]
        NodeMode::Sequencer(_) => {
            let privkey = sequencer_privkey_from_env()?;
            let pubkey = sequencer::sequencer_gossip_pubkey(&privkey)?;
            (
                GossipConfig::Sequencer {
                    sequencer_pubkey: pubkey,
                    sequencer_privkey: privkey,
                },
                Some(privkey),
            )
        }
    };

    // --- VALIDATE SEQUENCER CONFIG ---
    //
    // These are pure functions of the already-parsed config and the Alpen
    // params artifact, so they run before any DB, OL, or reth node startup
    // work: a config mistake should fail immediately, not deep inside
    // sequencer startup after stateful work has already happened.
    #[cfg(feature = "sequencer")]
    let writer_config = if let NodeMode::Sequencer(seq_config) = &alpen_config.mode {
        seq_config.validate_against_params(params.as_ref())?;
        Some(Arc::new(WriterConfig {
            l1_fee_policy_config: seq_config.l1_fee_policy.clone(),
            ..Default::default()
        }))
    } else {
        None
    };

    // OL client resolution validates the OL config synchronously before its
    // one network call, so a missing/invalid setting also fails here, before
    // the database is touched below.
    let (ol_client, genesis_epoch) = resolve_ol_client(&alpen_config, is_sequencer, &config).await?;

    // --- INITIALIZE STATE ---

    let dbs = init_db_storage(&datadir, config.db_retry_count())
        .context("failed to load alpen database")?;

    let db_handle = Handle::current();
    let storage: Arc<_> = dbs.node_storage(db_handle.clone()).into();

    ensure_genesis(config.as_ref(), &genesis_epoch, storage.as_ref(), is_sequencer)
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

    // The sequencer's real exec-chain tip, when running as a sequencer. Needed
    // before the reth node is built so the preconf watch channel (seeding the
    // engine-control task below) never starts from the wrong fork-choice head.
    let sequencer_head = {
        #[cfg(feature = "sequencer")]
        {
            sequencer::initial_preconf_head(is_sequencer, storage.as_ref()).await?
        }
        #[cfg(not(feature = "sequencer"))]
        {
            None
        }
    };
    let initial_preconf_head = sequencer_head.unwrap_or_else(|| {
        // In non-sequencer mode, we only have the hash from OL tracker.
        // Use block number 0 as initial value; it will be updated by gossip.
        let hash = ol_tracker_state.best_ee_state().last_exec_blkid();
        BlockNumHash::new(hash, 0)
    });
    // --- INITIALIZE SERVICES ---

    // Create gossip channel before building the node so we can register it early
    let (gossip_tx, gossip_rx) = mpsc::unbounded_channel();

    // Create preconf channel for p2p head block gossip -> engine control integration
    // This channel sends block hash and number received from peers to the engine control
    // task
    let (preconf_tx, preconf_rx) = watch::channel(initial_preconf_head);

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

    let evm_factory = AlpenEvmFactory::from_bridge_params(&bridge_params);
    let node_args = AlpenNodeArgs {
        sequencer_http: sequencer_http_url,
        evm_factory,
    };

    let consensus_watcher = ol_tracker.consensus_watcher();
    let status_watcher = ol_tracker.ol_status_watcher();

    let mut node_builder = builder
            .node(AlpenEthereumNode::new(node_args))
            // Register Alpen gossip RLPx subprotocol
            .on_component_initialized({
                let gossip_tx = gossip_tx.clone();
                move |node| {
                    // Add the custom RLPx subprotocol before node fully starts
                    // See: crates/reth/node/src/gossip/
                    let handler =
                        AlpenGossipProtocolHandler::new(AlpenGossipState::new(gossip_tx));
                    node.components
                        .network
                        .add_rlpx_sub_protocol(handler.into_rlpx_sub_protocol());
                    info!(target: "alpen-gossip", component = "alpen", "Registered Alpen gossip RLPx subprotocol");
                    Ok(())
                }
            });

    // Install state diff exex for sequencer DA.
    // The exex persists per-block state diffs that the blob provider reads.
    #[cfg(feature = "sequencer")]
    if is_sequencer {
        node_builder = node_builder.install_exex("state_diffs", {
            let state_diff_db = dbs.witness_db();
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
            let accessed_state_store = storage.clone();
            |ctx| async { Ok(AccessedStateGenerator::new(ctx, accessed_state_store).start()) }
        });
        info!(target: "alpen-client", component = "alpen", "installed AccessedStateGenerator exex (account-proof range witness)");
    }

    node_builder = node_builder.extend_rpc_modules({
        let consensus_watcher = consensus_watcher.clone();
        let storage = storage.clone();
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

    // Sync chainstate to engine for sequencer nodes before starting other tasks
    if is_sequencer {
        let engine = AlpenRethExecEngine::new(node.beacon_engine_handle.clone());
        let storage_clone = storage.clone();
        let provider_clone = node.provider.clone();

        // Block on the async sync operation
        let sync_result =
            sync_chainstate_to_engine(storage_clone.as_ref(), &provider_clone, &engine)
                .instrument(info_span!("chainstate_sync", component = "alpen"))
                .await;

        if let Err(e) = sync_result {
            error!(target: "alpen-client", component = "alpen", error = ?e, "failed to sync chainstate to engine on startup");
            return Err(eyre::eyre!("chainstate sync failed: {e}"));
        }

        info!(target: "alpen-client", component = "alpen", "chainstate sync completed successfully");
    }

    let engine_control_task = create_engine_control_task(
        preconf_rx.clone(),
        consensus_watcher.clone(),
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
    if let NodeMode::Sequencer(seq_config) = &alpen_config.mode {
        sequencer::launch(
            &service_executor,
            sequencer::SequencerLaunchCtx {
                node_provider: node.provider.clone(),
                task_executor: node.task_executor.clone(),
                payload_builder_handle: node.payload_builder_handle.clone(),
                beacon_engine_handle: node.beacon_engine_handle.clone(),
                sequencer_config: seq_config,
                l1_reorg_safe_depth: alpen_config.l1_reorg_safe_depth,
                genesis_l1_height: alpen_config.genesis_l1_height,
                storage: storage.clone(),
                dbs: &dbs,
                db_handle: db_handle.clone(),
                preconf_tx,
                preconf_rx,
                consensus_watcher,
                status_watcher,
                ol_client,
                genesis_info,
                params: params.clone(),
                sequencer_privkey: sequencer_privkey.expect(
                    "resolved above whenever NodeMode::Sequencer is matched",
                ),
                writer_config: writer_config
                    .expect("resolved above whenever NodeMode::Sequencer is matched"),
            },
        )
        .await?;
    }

    health_check_state.mark_ready();
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
    is_sequencer: bool,
    config: &AlpenEeConfig,
) -> eyre::Result<(Arc<OLClientKind>, EpochCommitment)> {
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
                    config.params().strata_exec_account_id(),
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

/// Handle genesis related tasks.
/// Mainly deals with ensuring database has minimal expected state.
async fn ensure_genesis<TStorage: Storage + ExecBlockStorage + BatchStorage>(
    config: &AlpenEeConfig,
    genesis_epoch: &EpochCommitment,
    storage: &TStorage,
    is_sequencer: bool,
) -> eyre::Result<()> {
    ensure_genesis_ee_account_state(config, genesis_epoch, storage).await?;

    if is_sequencer {
        ensure_finalized_exec_chain_genesis(config, genesis_epoch.to_block_commitment(), storage)
            .await?;
        ensure_batch_genesis(config, storage).await?;
    }

    Ok(())
}
