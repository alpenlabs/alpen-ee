//! Strata `eth_` endpoint implementation.
//! adapted from reth-node-optimism::rpc

pub mod fees;
pub mod receipt;
pub mod transaction;

mod block;
mod call;
mod pending_block;

use std::{
    fmt::{self, Formatter},
    marker::PhantomData,
    sync::Arc,
};

use alloy_network::Ethereum;
use alloy_primitives::U256;
use alloy_rpc_types_eth::BlockId;
use reth_chainspec::{EthereumHardforks, Hardforks};
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeComponents, FullNodeTypes, HeaderTy, NodeTypes};
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx};
use reth_provider::{BlockReader, ChainSpecProvider, ProviderHeader};
use reth_rpc::{eth::core::EthApiInner, RpcTypes};
use reth_rpc_eth_api::{
    helpers::{
        pending_block::BuildPendingEnv, EthApiSpec, EthFees, EthState, LoadFee, LoadPendingBlock,
        LoadState, SpawnBlocking, Trace,
    },
    EthApiTypes, FromEvmError, FullEthApiServer, RpcConvert, RpcConverter, RpcNodeCore,
    RpcNodeCoreExt,
};
use reth_rpc_eth_types::{
    receipt::EthReceiptConverter, EthApiError, EthStateCache, FeeHistoryCache, GasPriceOracle,
};
use reth_tasks::{
    pool::{BlockingTaskGuard, BlockingTaskPool},
    TaskSpawner,
};

use crate::SequencerClient;

/// Adapter for [`EthApiInner`], which holds all the data required to serve core `eth_` API.
pub type EthApiNodeBackend<N, Rpc> = EthApiInner<N, Rpc>;

/// A helper trait with requirements for [`RpcNodeCore`] to be used in [`AlpenEthApi`].
pub trait StrataNodeCore: RpcNodeCore<Provider: BlockReader> {}
impl<T> StrataNodeCore for T where T: RpcNodeCore<Provider: BlockReader> {}

/// Supplies the sequencer's latest DA fee rate to mutable-state fee estimation.
pub trait LiveDaFeeRateProvider: fmt::Debug + Send + Sync + 'static {
    /// Returns the rate a new payload build would currently sample.
    fn current_rate(&self) -> u64;
}

/// Strata Eth API implementation.
///
/// This type provides the functionality for handling `eth_` related requests.
///
/// This wraps a default `Eth` implementation, and provides additional functionality where the
/// Strata spec deviates from the default (ethereum) spec, e.g. transaction forwarding to the
/// sequencer.
///
/// This type implements the [`FullEthApi`](reth_rpc_eth_api::helpers::FullEthApi) by implemented
/// all the `Eth` helper traits and prerequisite traits.
pub struct AlpenEthApi<N: StrataNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    inner: Arc<AlpenEthApiInner<N, Rpc>>,
}

impl<N: RpcNodeCore, Rpc: RpcConvert> Clone for AlpenEthApi<N, Rpc> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<N: RpcNodeCore, Rpc: RpcConvert> AlpenEthApi<N, Rpc> {
    /// Returns a reference to the [`EthApiNodeBackend`].
    pub fn eth_api(&self) -> &EthApiNodeBackend<N, Rpc> {
        self.inner.eth_api()
    }

    /// Returns the configured sequencer client, if any.
    pub fn sequencer_client(&self) -> Option<&SequencerClient> {
        self.inner.sequencer_client()
    }

    /// Samples the live sequencer rate for estimates targeting mutable chain state.
    pub(crate) fn live_da_fee_rate(&self, at: BlockId) -> Option<u64> {
        sample_live_da_fee_rate(self.inner.live_da_fee_rate_provider(), at)
    }

    /// Build a [`AlpenEthApi`] using [`AlpenEthApiBuilder`].
    pub const fn builder() -> AlpenEthApiBuilder {
        AlpenEthApiBuilder::new()
    }
}

/// Returns whether an estimate targets state whose next-block rate is not yet committed.
const fn uses_live_da_fee_rate(at: BlockId) -> bool {
    at.is_latest() || at.is_pending()
}

/// Reads the provider only for estimates targeting mutable chain state.
fn sample_live_da_fee_rate(
    provider: Option<&dyn LiveDaFeeRateProvider>,
    at: BlockId,
) -> Option<u64> {
    uses_live_da_fee_rate(at)
        .then(|| provider.map(LiveDaFeeRateProvider::current_rate))
        .flatten()
}

impl<N, Rpc> EthApiTypes for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
    type Error = EthApiError;
    type NetworkTypes = Rpc::Network;
    type RpcConvert = Rpc;

    fn tx_resp_builder(&self) -> &Self::RpcConvert {
        self.inner.eth_api.tx_resp_builder()
    }
}

impl<N, Rpc> RpcNodeCore for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
    type Primitives = N::Primitives;
    type Provider = N::Provider;
    type Pool = N::Pool;
    type Evm = N::Evm;
    type Network = N::Network;

    #[inline]
    fn pool(&self) -> &Self::Pool {
        self.inner.eth_api.pool()
    }

    #[inline]
    fn evm_config(&self) -> &Self::Evm {
        self.inner.eth_api.evm_config()
    }

    #[inline]
    fn network(&self) -> &Self::Network {
        self.inner.eth_api.network()
    }

    #[inline]
    fn provider(&self) -> &Self::Provider {
        self.inner.eth_api.provider()
    }
}

impl<N, Rpc> RpcNodeCoreExt for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
    #[inline]
    fn cache(&self) -> &EthStateCache<N::Primitives> {
        self.inner.eth_api.cache()
    }
}

impl<N, Rpc> EthApiSpec for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
    #[inline]
    fn starting_block(&self) -> U256 {
        self.inner.eth_api.starting_block()
    }
}

impl<N, Rpc> SpawnBlocking for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
    #[inline]
    fn io_task_spawner(&self) -> impl TaskSpawner {
        self.inner.eth_api.task_spawner()
    }

    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.eth_api.blocking_task_pool()
    }

    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.eth_api.blocking_task_guard()
    }
}

impl<N, Rpc> LoadFee for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.eth_api().gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache<ProviderHeader<N::Provider>> {
        self.inner.eth_api().fee_history_cache()
    }
}

impl<N, Rpc> LoadState for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
    Self: LoadPendingBlock,
{
}

impl<N, Rpc> EthState for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
    Self: LoadPendingBlock,
{
    #[inline]
    fn max_proof_window(&self) -> u64 {
        self.inner.eth_api.eth_proof_window()
    }
}

impl<N, Rpc> EthFees for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
}

impl<N, Rpc> Trace for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
}

impl<N: RpcNodeCore, Rpc: RpcConvert> fmt::Debug for AlpenEthApi<N, Rpc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpEthApi").finish_non_exhaustive()
    }
}

/// Container type for [`AlpenEthApi`]
#[allow(
    missing_debug_implementations,
    clippy::allow_attributes,
    reason = "Some inner types don't have Debug implementation"
)]
struct AlpenEthApiInner<N: RpcNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    eth_api: EthApiNodeBackend<N, Rpc>,
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    sequencer_client: Option<SequencerClient>,
    /// Live rate source installed only on the sequencer.
    live_da_fee_rate_provider: Option<Arc<dyn LiveDaFeeRateProvider>>,
}

impl<N: RpcNodeCore, Rpc: RpcConvert> fmt::Debug for AlpenEthApiInner<N, Rpc> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlpenEthApiInner").finish()
    }
}

impl<N: RpcNodeCore, Rpc: RpcConvert> AlpenEthApiInner<N, Rpc> {
    /// Returns a reference to the [`EthApiNodeBackend`].
    const fn eth_api(&self) -> &EthApiNodeBackend<N, Rpc> {
        &self.eth_api
    }

    /// Returns the configured sequencer client, if any.
    const fn sequencer_client(&self) -> Option<&SequencerClient> {
        self.sequencer_client.as_ref()
    }

    /// Returns the live sequencer rate source, if configured.
    fn live_da_fee_rate_provider(&self) -> Option<&dyn LiveDaFeeRateProvider> {
        self.live_da_fee_rate_provider.as_deref()
    }
}

#[expect(
    missing_debug_implementations,
    reason = "Some inner types don't have Debug implementation"
)]
pub struct AlpenEthApiBuilder<NetworkT = Ethereum> {
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    sequencer_client: Option<SequencerClient>,
    /// Live rate source installed only on the sequencer.
    live_da_fee_rate_provider: Option<Arc<dyn LiveDaFeeRateProvider>>,
    /// Marker for network types.
    _nt: PhantomData<NetworkT>,
}

impl<NetworkT> AlpenEthApiBuilder<NetworkT> {
    /// Creates a [`AlpenEthApiBuilder`] instance.
    pub const fn new() -> Self {
        Self {
            sequencer_client: None,
            live_da_fee_rate_provider: None,
            _nt: PhantomData,
        }
    }

    /// With a [`SequencerClient`].
    pub fn with_sequencer(mut self, sequencer_client: Option<SequencerClient>) -> Self {
        self.sequencer_client = sequencer_client;
        self
    }

    /// Installs the live DA fee-rate source used by mutable-state estimation.
    pub fn with_live_da_fee_rate_provider(
        mut self,
        provider: Option<Arc<dyn LiveDaFeeRateProvider>>,
    ) -> Self {
        self.live_da_fee_rate_provider = provider;
        self
    }
}

impl<NetworkT> Default for AlpenEthApiBuilder<NetworkT> {
    fn default() -> Self {
        Self {
            sequencer_client: None,
            live_da_fee_rate_provider: None,
            _nt: PhantomData,
        }
    }
}

/// Converter for Alpen RPC types.
pub type AlpenRpcConvert<N, NetworkT> = RpcConverter<
    NetworkT,
    <N as FullNodeComponents>::Evm,
    EthReceiptConverter<<<N as FullNodeTypes>::Types as NodeTypes>::ChainSpec>,
    (),
    (),
>;

impl<N, NetworkT> EthApiBuilder<N> for AlpenEthApiBuilder<NetworkT>
where
    N: FullNodeComponents<
        Types: NodeTypes<ChainSpec: Hardforks + EthereumHardforks>,
        Evm: ConfigureEvm<NextBlockEnvCtx: BuildPendingEnv<HeaderTy<N::Types>>>,
    >,
    NetworkT: RpcTypes,
    AlpenRpcConvert<N, NetworkT>: RpcConvert<Network = NetworkT>,
    AlpenEthApi<N, AlpenRpcConvert<N, NetworkT>>:
        FullEthApiServer<Provider = N::Provider, Pool = N::Pool>,
{
    type EthApi = AlpenEthApi<N, AlpenRpcConvert<N, NetworkT>>;

    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let Self {
            sequencer_client,
            live_da_fee_rate_provider,
            ..
        } = self;

        let rpc_converter = RpcConverter::new(EthReceiptConverter::new(
            ctx.components.provider().chain_spec(),
        ));

        let eth_api = ctx
            .eth_api_builder()
            .with_rpc_converter(rpc_converter)
            .build_inner();

        Ok(AlpenEthApi {
            inner: Arc::new(AlpenEthApiInner {
                eth_api,
                sequencer_client,
                live_da_fee_rate_provider,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FixedRate(u64);

    impl LiveDaFeeRateProvider for FixedRate {
        fn current_rate(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn only_mutable_block_tags_use_the_live_rate() {
        let provider = FixedRate(17);
        let provider = Some(&provider as &dyn LiveDaFeeRateProvider);

        assert_eq!(
            sample_live_da_fee_rate(provider, BlockId::latest()),
            Some(17)
        );
        assert_eq!(
            sample_live_da_fee_rate(provider, BlockId::pending()),
            Some(17)
        );
        assert_eq!(sample_live_da_fee_rate(None, BlockId::latest()), None);

        for historical in [
            BlockId::earliest(),
            BlockId::safe(),
            BlockId::finalized(),
            BlockId::number(7),
        ] {
            assert_eq!(sample_live_da_fee_rate(provider, historical), None);
        }
    }
}
