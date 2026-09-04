//! Full-node startup: builds, launches, and runs the reth node.
//!
//! The counterpart to the sequencer's own startup, which only exists in a
//! build with the `sequencer` feature. A full node produces no blocks of its
//! own, so this is the whole of its mode-specific startup: there is no
//! genesis work beyond what [`crate::node`] already did, no ExExes, and no
//! services past the two every node runs.

use std::sync::Arc;

use alpen_ee_common::BlockNumHash;
use alpen_ee_rpc_server::{AlpenEeRpcServer, EeRpcServer};
use alpen_reth_evm::evm::AlpenEvmFactory;
use alpen_reth_node::{
    AlpenEthereumNode, AlpenGossipProtocolHandler, AlpenGossipState, AlpenNodeMode, DaFeeRateHandle,
};
use alpen_reth_rpc::AlpenFeeApiServer;
use reth_chainspec::ChainSpec;
use reth_network::{protocol::IntoRlpxSubProtocol, NetworkProtocols};
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use tokio::sync::{mpsc, watch};
use tracing::info;

use crate::{
    config::FullNodeConfig,
    gossip::GossipConfig,
    node::{LaunchedNode, NodeBootstrap},
};

pub(crate) async fn run(
    builder: WithLaunchContext<NodeBuilder<Arc<reth_db::DatabaseEnv>, ChainSpec>>,
    common: NodeBootstrap,
    config: &FullNodeConfig,
) -> eyre::Result<()> {
    let evm_factory = AlpenEvmFactory::from_bridge_params(common.params.bridge_params());
    // A full node never builds blocks, so its DA fee rate handle is never sampled. It
    // recovers each block's frozen rate from the header `extra_data` instead.
    // The handle exists only to satisfy the shared node type.
    let da_fee_rate_handle = DaFeeRateHandle::fixed(0);
    let node = AlpenEthereumNode::new(
        evm_factory,
        common.params.evm_spec().clone(),
        AlpenNodeMode::full_node(config.sequencer_http_url.clone()),
        da_fee_rate_handle,
    );

    let consensus_watcher = common.ol_tracker.consensus_watcher();

    // Create gossip channel before building the node so we can register it early
    let (gossip_tx, gossip_rx) = mpsc::unbounded_channel();

    // A full node has no chain of its own to start from. Gossip overwrites
    // this as soon as the first message arrives, so the tracker's current
    // confirmed head at block number 0 is a fine seed.
    let confirmed_head = consensus_watcher.borrow().confirmed;
    let (preconf_tx, preconf_rx) = watch::channel(BlockNumHash::new(confirmed_head, 0));

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

    LaunchedNode {
        provider: handle.node.provider.clone(),
        task_executor: handle.node.task_executor.clone(),
        beacon_engine_handle: handle.node.beacon_engine_handle.clone(),
        preconf_tx,
        preconf_rx,
    }
    .spawn_shared_tasks(
        consensus_watcher,
        gossip_rx,
        // A full node is *told* the sequencer's pubkey — it can't derive it,
        // holding no private key. Contrast `GossipConfig::sequencer`.
        GossipConfig::full_node(config.sequencer_pubkey),
    );

    common.run_until_exit(handle.node_exit_future).await
}
