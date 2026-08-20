use alpen_reth_evm::{config::AlpenEvmConfig, evm::AlpenEvmFactory};
use reth_chainspec::ChainSpec;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{components::ExecutorBuilder, BuilderContext};
// use reth_node_ethereum::BasicBlockExecutorProvider;
use reth_primitives::EthPrimitives;

/// Builds a regular ethereum block executor that uses the custom EVM.
#[derive(Debug, Clone, Default)]
pub struct AlpenExecutorBuilder {
    evm_factory: AlpenEvmFactory,
}

impl AlpenExecutorBuilder {
    pub fn new(evm_factory: AlpenEvmFactory) -> Self {
        Self { evm_factory }
    }
}

impl<Node> ExecutorBuilder<Node> for AlpenExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = AlpenEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(AlpenEvmConfig::new(ctx.chain_spec(), self.evm_factory))
    }
}
