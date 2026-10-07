use std::sync::Arc;

use alpen_params::AlpenParams;
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::EthPrimitives;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{components::ExecutorBuilder, BuilderContext};

use crate::evm_config::AlpenEvmConfig;

/// Builds the version-aware block executor over the custom EVM.
#[derive(Debug, Clone)]
pub struct AlpenExecutorBuilder {
    params: Arc<AlpenParams>,
}

impl AlpenExecutorBuilder {
    pub fn new(params: Arc<AlpenParams>) -> Self {
        Self { params }
    }
}

impl<Node> ExecutorBuilder<Node> for AlpenExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = AlpenEvmConfig;

    async fn build_evm(self, _ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(AlpenEvmConfig::new(&self.params))
    }
}
