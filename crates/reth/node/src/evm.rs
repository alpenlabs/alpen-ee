use alpen_ee_params::EvmSpec;
use alpen_reth_evm::evm::AlpenEvmFactory;
use reth_chainspec::ChainSpec;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{components::ExecutorBuilder, BuilderContext};
use reth_primitives::EthPrimitives;

use crate::evm_config::AlpenEvmConfig;

/// Builds the version-aware block executor over the custom EVM.
#[derive(Debug, Clone)]
pub struct AlpenExecutorBuilder {
    evm_factory: AlpenEvmFactory,
    evm_spec: EvmSpec,
}

impl AlpenExecutorBuilder {
    pub fn new(evm_factory: AlpenEvmFactory, evm_spec: EvmSpec) -> Self {
        Self {
            evm_factory,
            evm_spec,
        }
    }
}

impl<Node> ExecutorBuilder<Node> for AlpenExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = AlpenEvmConfig;

    async fn build_evm(self, _ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(AlpenEvmConfig::new(&self.evm_spec, self.evm_factory))
    }
}
