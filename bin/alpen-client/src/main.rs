//! Reth node for the Alpen codebase.
//!
//! # Logging
//!
//! Alpen (non-reth) logs carry a `component = "alpen"` field so they can be
//! filtered apart from the embedded reth logs in monitoring. The field is
//! attached via `info_span!(..., component = "alpen")` spans, so it is only
//! present while those spans are enabled. Run this crate with the `alpen_client`
//! target at INFO or a more verbose level to get the tags: lowering it (e.g.
//! `RUST_LOG=alpen_client=warn`) or capping the compile-time level below info
//! (`tracing/release_max_level_*`) disables the spans and silently drops the tag.

mod args;
mod dummy_ol_client;
mod gossip;
mod ol_client;
mod rpc_client;
#[cfg(feature = "sequencer")]
mod sequencer;
mod service_executor;
mod services;

use std::{env, process, sync::Arc};

use alpen_chainspec::AlpenChainSpecParser;
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
use clap::Parser;
use eyre::Context;
use reth_chainspec::ChainSpec;
use reth_cli_commands::{launcher::FnLauncher, node::NodeCommand};
use reth_cli_runner::{tokio_runtime, CliRunner};
use reth_cli_util::sigsegv_handler;
use reth_network::{protocol::IntoRlpxSubProtocol, NetworkProtocols};
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_provider::CanonStateSubscriptions;
use strata_common::healthz::{start_health_check_server, HealthCheckState};
use strata_identifiers::{EpochCommitment, OLBlockId};
use strata_logging::{init_logging_from_config, LoggingInitConfig};
use strata_predicate::PredicateKey;
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};
use tracing::{error, info, info_span, Instrument};

use crate::{
    args::{sequencer_privkey_from_env, AdditionalConfig},
    dummy_ol_client::DummyOLClient,
    gossip::{create_gossip_task, GossipConfig},
    ol_client::OLClientKind,
    rpc_client::RpcOLClient,
    service_executor::ServiceExecutor,
};

fn main() {
    sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: fine to set this in a non-async context.
        unsafe { env::set_var("RUST_BACKTRACE", "1") };
    }

    let mut command = NodeCommand::<AlpenChainSpecParser, AdditionalConfig>::parse();

    // use the EVM chain spec embedded in the Alpen params artifact
    command.chain = Arc::new(command.ext.chain.alpen_params.chain_spec().clone());
    // enable engine api v4
    command.engine.accept_execution_requests_hash = true;
    // allow chain fork blocks to be created
    command
        .engine
        .always_process_payload_attributes_on_canonical_head = true;

    if let Err(err) = run(
        command,
        |builder: WithLaunchContext<NodeBuilder<Arc<reth_db::DatabaseEnv>, ChainSpec>>,
         ext: AdditionalConfig| async move {
            let service_executor = ServiceExecutor::from_reth(builder.task_executor().clone());
            let health_check_state = HealthCheckState::new();
            let health_check_addr = format!(
                "{}:{}",
                ext.node.health_check_host, ext.node.health_check_port
            );
            let _health_check_handle =
                start_health_check_server(health_check_addr.clone(), health_check_state.clone())
                    .instrument(info_span!("start_health_check_server", component = "alpen"))
                    .await
                    .context("failed to start health check server")?;
            info!(target: "alpen-client", component = "alpen", %health_check_addr, "health check server started");

            // --- CONFIGS ---
            let datadir = builder.config().datadir().data_dir().to_path_buf();

            // TODO(STR-2982): read config from file
            let params = ext.chain.alpen_params.clone();
            let genesis_info = params.genesis_block_info();

            info!(target: "alpen-client", component = "alpen", blockhash=%genesis_info.blockhash(), "EE genesis info");
            let bridge_params = *params.bridge_params();
            info!(
                target: "alpen-client", component = "alpen",
                account_id = ?params.strata_exec_account_id(),
                ?bridge_params,
                sequencer = ext.sequencer.enabled,
                "Starting EE Node",
            );

            // OL client URL is not used when the dummy OL client is enabled
            let ol_client_url = ext.ol.client_url.clone().unwrap_or_default();

            let config = Arc::new(AlpenEeConfig::new(
                params.clone(),
                PredicateKey::always_accept(),
                ol_client_url,
                ext.sequencer.http_url.clone(),
                ext.node.db_retry_count,
            ));

            // NOTE: ATM we reuse `SEQUENCER_PRIVATE_KEY` for both gossip
            // package signing and EE DA reveal tapscript signing. That is
            // operationally convenient for now, but it couples network
            // identity with Bitcoin DA spend authority. Should we split this
            // into a dedicated DA reveal signing key/config?
            let sequencer_privkey = sequencer_privkey_from_env(ext.sequencer.enabled)?;

            let gossip_config = GossipConfig {
                sequencer_pubkey: ext.sequencer.pubkey,
                sequencer_enabled: ext.sequencer.enabled,
                sequencer_privkey,
            };

            // --- INITIALIZE STATE ---

            let dbs = init_db_storage(&datadir, config.db_retry_count())
                .context("failed to load alpen database")?;

            let db_handle = Handle::current();
            let storage: Arc<_> = dbs.node_storage(db_handle.clone()).into();

            let ol_client = if ext.ol.dummy_client {
                use strata_identifiers::Buf32;
                use strata_primitives::EpochCommitment;
                let genesis_epoch = EpochCommitment::new(0, 0, OLBlockId::from(Buf32([1; 32])));
                info!(target: "alpen-client", component = "alpen", "Using dummy OL client (no real OL connection)");
                OLClientKind::Dummy(DummyOLClient { genesis_epoch })
            } else {
                let ol_url = ext.ol.client_url.as_ref().ok_or_else(|| {
                    eyre::eyre!("--ol-client-url is required when not using --dummy-ol-client")
                })?;
                if ext.sequencer.enabled && ext.ol.submit_url.is_none() {
                    eyre::bail!(
                        "--ol-submit-url is required with --sequencer when not using \
                         --dummy-ol-client"
                    );
                }
                OLClientKind::Rpc(
                    RpcOLClient::try_new(
                        config.params().strata_exec_account_id(),
                        ol_url,
                        ext.ol.submit_url.as_deref(),
                        ext.ol.submit_bearer_token.as_deref(),
                    )
                    .map_err(|e| eyre::eyre!("failed to create OL client: {e}"))?,
                )
            };
            let ol_client = Arc::new(ol_client);

            // Fetch the genesis epoch commitment from the OL client once at startup.
            let genesis_epoch = ol_client
                .account_genesis_epoch()
                .instrument(info_span!("account_genesis_epoch", component = "alpen"))
                .await
                .context("failed to fetch account genesis epoch from OL")?;

            ensure_genesis(
                config.as_ref(),
                &genesis_epoch,
                storage.as_ref(),
                ext.sequencer.enabled,
            )
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
                    sequencer::initial_preconf_head(ext.sequencer.enabled, storage.as_ref()).await?
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
                ext.ol.dev_track_latest_epoch,
                &service_executor,
            )
            .await
            .map_err(|e| eyre::eyre!("failed to start ol tracker service: {e}"))?;

            let evm_factory = AlpenEvmFactory::from_bridge_params(&bridge_params);
            let node_args = AlpenNodeArgs {
                sequencer_http: ext.sequencer.http_url.clone(),
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
            if ext.sequencer.enabled {
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
                    |ctx| async {
                        Ok(AccessedStateGenerator::new(ctx, accessed_state_store).start())
                    }
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
            if ext.sequencer.enabled {
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
            if ext.sequencer.enabled {
                sequencer::launch(
                    &service_executor,
                    sequencer::SequencerLaunchCtx {
                        node_provider: node.provider.clone(),
                        task_executor: node.task_executor.clone(),
                        payload_builder_handle: node.payload_builder_handle.clone(),
                        beacon_engine_handle: node.beacon_engine_handle.clone(),
                        ext: &ext,
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
                            "sequencer_privkey_from_env already validated SEQUENCER_PRIVATE_KEY \
                             is set when --sequencer is set",
                        ),
                    },
                )
                .await?;
            }

            health_check_state.mark_ready();
            handle.node_exit_future.await
        },
    ) {
        eprintln!("Error: {err:?}");
        process::exit(1);
    }
}

/// Run node with logging
/// based on reth::cli::Cli::run
fn run<L>(
    command: NodeCommand<AlpenChainSpecParser, AdditionalConfig>,
    launcher: L,
) -> eyre::Result<()>
where
    L: std::ops::AsyncFnOnce(
        WithLaunchContext<NodeBuilder<Arc<reth_db::DatabaseEnv>, ChainSpec>>,
        AdditionalConfig,
    ) -> eyre::Result<()>,
{
    if command.ext.sequencer.enabled && !cfg!(feature = "sequencer") {
        error!(
            target: "alpen-client",
            component = "alpen",
            "Sequencer flag enabled but binary built without `sequencer` feature. Rebuild with default features or enable the `sequencer` feature."
        );
        eyre::bail!("sequencer feature not enabled at compile time");
    }

    // Build the tokio runtime ourselves so logging init can run inside its
    // context, then hand it to CliRunner. The OTLP tracing exporter requires
    // an active tokio handle when it is built.
    let rt = tokio_runtime()?;

    {
        let _g = rt.handle().enter();

        let mut extra_filter_directives =
            vec!["sp1_core_executor=warn", "jsonrpsee_server::server=warn"];
        if let Some(verbosity_filter) = command.ext.display.verbosity_filter_directive() {
            extra_filter_directives.push(verbosity_filter);
        }

        init_logging_from_config(LoggingInitConfig {
            service_base_name: "alpen-client",
            service_label: command.ext.display.service_label.as_deref(),
            otlp_url: command.ext.display.otlp_url.as_deref(),
            log_dir: None,
            log_file_prefix: None,
            json_format: None,
            default_log_prefix: "alpen-client",
            extra_filter_directives: &extra_filter_directives,
        });
    }

    let runner = CliRunner::from_runtime(rt);

    info!(target: "alpen-client", component = "alpen", "logging initialized");

    let result = runner.run_command_until_exit(|ctx| {
        command.execute(
            ctx,
            FnLauncher::new::<AlpenChainSpecParser, AdditionalConfig>(launcher),
        )
    });

    // Flush OTLP tracing buffers before the process exits.
    strata_logging::finalize();

    result
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
