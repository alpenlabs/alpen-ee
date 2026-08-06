//! Reth node bootstrap and mode dispatch.
//!
//! [`launch`] resolves the resources every node needs — health check server,
//! database and storage, the OL client, and the OL tracker — then dispatches
//! on [`NodeMode`] to the path for the configured mode: [`crate::full_node`],
//! or the `sequencer` module in a build with that feature. Each of those owns
//! its reth builder chain end to end.
//!
//! The builder chain isn't shared because reth's builder changes generic type
//! at `.node(...)`, so a helper taking or returning it would have to name
//! those deeply generic types. Everything that runs *after* launch is shared
//! instead, via [`LaunchedNode`] and [`NodeBootstrap::run_until_exit`], both
//! of which take the node's handles individually.

use std::{future::Future, sync::Arc};

use alpen_ee_common::{chain_status_checked, BlockNumHash, ConsensusHeads, OLClient};
use alpen_ee_database::{open_ee_db, EeNodeStorage};
use alpen_ee_engine::{create_engine_control_task, AlpenRethExecEngine};
use alpen_ee_genesis::ensure_genesis_ee_account_state;
use alpen_ee_ol_tracker::init_ol_tracker_state;
use alpen_ee_params::AlpenParams;
use alpen_reth_node::{AlpenEngineTypes, AlpenGossipEvent};
use eyre::Context;
use jsonrpsee::server::ServerHandle;
use reth_chainspec::ChainSpec;
use reth_node_builder::{
    ConsensusEngineHandle, NodeBuilder, NodeTypes, NodeTypesWithDB, WithLaunchContext,
};
use reth_primitives::EthPrimitives;
use reth_provider::{
    providers::{BlockchainProvider, ProviderNodeTypes},
    CanonStateSubscriptions,
};
use reth_tasks::TaskExecutor;
use strata_common::healthz::{start_health_check_server, HealthCheckState};
use strata_identifiers::{EpochCommitment, OLBlockId};
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};
use tracing::{info, info_span, Instrument};

#[cfg(feature = "sequencer")]
use crate::{args::sequencer_privkey_from_env, sequencer};
use crate::{
    args::{ol_submit_bearer_token_from_env, AdditionalConfig},
    config::{AlpenClientConfig, NodeMode, OlSource},
    full_node,
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

    // NOTE: ATM we reuse `SEQUENCER_PRIVATE_KEY` for both gossip package
    // signing and EE DA reveal tapscript signing. That is operationally
    // convenient for now, but it couples network identity with Bitcoin DA
    // spend authority. Should we split this into a dedicated DA reveal
    // signing key/config?
    //
    // Resolve the sequencer identity before bootstrap connects to OL, opens
    // Sled, initializes genesis state, or starts the OL tracker. A missing or
    // malformed key is a configuration error and must not perform stateful
    // startup work first.
    #[cfg(feature = "sequencer")]
    let sequencer_privkey = match &alpen_config.mode {
        NodeMode::FullNode(_) => None,
        NodeMode::Sequencer(_) => Some(sequencer_privkey_from_env()?),
    };

    let common = bootstrap_node(&builder, &alpen_config, &params).await?;

    match &alpen_config.mode {
        NodeMode::FullNode(full_node_config) => {
            full_node::run(builder, common, full_node_config).await
        }
        #[cfg(feature = "sequencer")]
        NodeMode::Sequencer(sequencer_mode) => {
            let privkey = sequencer_privkey
                .expect("sequencer key was resolved before bootstrap for sequencer mode");
            sequencer::run(builder, common, sequencer_mode, privkey).await
        }
    }
}

/// Resources every node holds once bootstrap is done, whatever its mode.
pub(crate) struct NodeBootstrap {
    health_check_state: HealthCheckState,
    /// Kept alive for the node's lifetime: dropping it closes the watch
    /// channel the health server's accept loop stops on.
    _health_check_handle: ServerHandle,
    pub(crate) params: Arc<AlpenParams>,
    pub(crate) storage: Arc<EeNodeStorage>,
    /// Kept as the handle (not pre-extracted watchers) so each mode pulls
    /// exactly the watchers it needs, where it needs them — a full node
    /// never reads `ol_status_watcher` at all, for instance.
    pub(crate) ol_tracker: services::ol_tracker::OLTrackerHandle,
    #[cfg(feature = "sequencer")]
    pub(crate) sequencer: sequencer::BootstrapResources,
}

impl NodeBootstrap {
    /// Marks the node ready for health checks, then blocks until it exits.
    ///
    /// Takes the exit future as `impl Future` so callers don't have to name
    /// reth's launched-node types. Consumes `self` so the health check server
    /// stays up for exactly as long as the node does.
    pub(crate) async fn run_until_exit(
        self,
        node_exit_future: impl Future<Output = eyre::Result<()>>,
    ) -> eyre::Result<()> {
        self.health_check_state.mark_ready();
        node_exit_future.await
    }
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

    // Sled locks its directory exclusively, so this is the one place the
    // instance is opened. A sequencer takes its extra databases off the same
    // handle in `sequencer::run`; a full node never creates those trees.
    let db = open_ee_db(&datadir, alpen_config.db_retry_count)
        .context("failed to load alpen database")?;

    let storage: Arc<_> = db
        .node_storage(Handle::current())
        .context("failed to open EE node storage")?
        .into();

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
        health_check_state,
        _health_check_handle,
        params: params.clone(),
        storage,
        ol_tracker,
        #[cfg(feature = "sequencer")]
        sequencer: sequencer::BootstrapResources {
            service_executor,
            db,
            ol_client,
            genesis_epoch,
        },
    })
}

/// The handles a launched reth node hands back to Alpen code.
///
/// Grouped into a struct because the launched node's own type is too generic
/// to pass across a function boundary, and because the shared tasks and the
/// sequencer's own startup both want the same set. Every field is read by
/// [`Self::spawn_shared_tasks`], so both modes use all of them.
///
/// The one handle deliberately left out is the payload builder: only the
/// sequencer's payload engine touches it, so it travels as its own argument
/// rather than as a field nothing on the full-node path would read.
pub(crate) struct LaunchedNode<N: NodeTypesWithDB + ProviderNodeTypes> {
    pub(crate) provider: BlockchainProvider<N>,
    pub(crate) task_executor: TaskExecutor,
    pub(crate) beacon_engine_handle: ConsensusEngineHandle<AlpenEngineTypes>,
    /// Both ends are kept rather than deriving the receiver from the sender
    /// on demand: `Sender::subscribe` marks the current value as already
    /// seen, so a head update published between channel creation and a later
    /// `subscribe` would be missed by the batch builder.
    pub(crate) preconf_tx: watch::Sender<BlockNumHash>,
    pub(crate) preconf_rx: watch::Receiver<BlockNumHash>,
}

impl<N> LaunchedNode<N>
where
    N: NodeTypesWithDB + ProviderNodeTypes + NodeTypes<Primitives = EthPrimitives>,
{
    /// Spawns the two tasks every node runs: engine control and gossip.
    pub(crate) fn spawn_shared_tasks(
        &self,
        consensus_watcher: watch::Receiver<ConsensusHeads>,
        gossip_rx: mpsc::UnboundedReceiver<AlpenGossipEvent>,
        gossip_config: GossipConfig,
    ) {
        let engine_control_task = create_engine_control_task(
            self.preconf_rx.clone(),
            consensus_watcher,
            self.provider.clone(),
            AlpenRethExecEngine::new(self.beacon_engine_handle.clone()),
        );

        // Subscribe to canonical state notifications for broadcasting new blocks
        let state_events = self.provider.subscribe_to_canonical_state();

        let gossip_task = create_gossip_task(
            gossip_rx,
            state_events,
            self.preconf_tx.clone(),
            gossip_config,
        );

        self.task_executor.spawn_critical(
            "engine_control",
            engine_control_task.instrument(info_span!("engine_control", component = "alpen")),
        );
        self.task_executor.spawn_critical(
            "gossip_task",
            gossip_task.instrument(info_span!("gossip_task", component = "alpen")),
        );
    }
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
