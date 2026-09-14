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
    eest_fixture_mode: bool,
}

impl AlpenExecutorBuilder {
    pub fn new(evm_factory: AlpenEvmFactory, evm_spec: EvmSpec) -> Self {
        Self {
            evm_factory,
            evm_spec,
            eest_fixture_mode: false,
        }
    }

    /// Configures execution for canonical Ethereum fixtures whose
    /// `extra_data` must not be interpreted as an Alpen header stamp.
    pub fn with_eest_fixture_mode(mut self) -> Self {
        self.eest_fixture_mode = true;
        self
    }
}

impl<Node> ExecutorBuilder<Node> for AlpenExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = AlpenEvmConfig;

    async fn build_evm(self, _ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let config = AlpenEvmConfig::new(&self.evm_spec, self.evm_factory);
        Ok(if self.eest_fixture_mode {
            config.with_eest_fixture_mode()
        } else {
            config
        })
    }
}
