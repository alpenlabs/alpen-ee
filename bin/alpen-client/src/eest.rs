//! Isolated execution-client process used by the EEST fixture driver.
//!
//! This mode exercises Alpen's real EVM, payload builder, consensus and
//! Engine API, but intentionally does not start a sequencer's OL/DA/batch/
//! chunk/proving services. EEST restores the Engine forkchoice to a stable
//! baseline before each case; those services consume a monotonically advancing
//! canonical chain and would make that valid EVM-level reorg unsafe.

use std::sync::{atomic::AtomicU64, Arc};

use alpen_ee_params::AlpenParams;
use alpen_reth_evm::evm::AlpenEvmFactory;
use alpen_reth_node::{AlpenEthereumNode, AlpenNodeMode};
use eyre::Context;
use reth_chainspec::ChainSpec;
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use tracing::info;

/// Launch the minimal Alpen execution node that EEST controls through Engine
/// API calls.
pub(crate) async fn run(
    builder: WithLaunchContext<NodeBuilder<Arc<reth_db::DatabaseEnv>, ChainSpec>>,
    params: Arc<AlpenParams>,
) -> eyre::Result<()> {
    let evm_factory = AlpenEvmFactory::from_bridge_params(params.bridge_params());
    let node = AlpenEthereumNode::new(
        evm_factory,
        params.evm_spec().clone(),
        AlpenNodeMode::sequencer(),
        Arc::new(AtomicU64::new(0)),
        params.base_fee_floor(),
    );

    let handle = builder
        .node(node)
        .launch()
        .await
        .context("failed to launch isolated EEST execution node")?;
    info!(
        target: "alpen-client",
        component = "alpen",
        base_fee_floor = params.base_fee_floor(),
        "started isolated EEST execution node without EE auxiliary services"
    );

    handle
        .node_exit_future
        .await
        .context("isolated EEST execution node exited")
}
