use alpen_ee_params::EvmSpec;
use alpen_reth_evm::evm::AlpenEvmFactory;

#[derive(Debug, Clone)]
pub struct AlpenNodeArgs {
    pub sequencer_http: Option<String>,
    pub evm_factory: AlpenEvmFactory,
    /// The embedded EVM chain spec whose per-version table backs per-block
    /// version resolution across the node's fork-sensitive components.
    pub evm_spec: EvmSpec,
}
