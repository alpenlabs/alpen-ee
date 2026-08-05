use alpen_reth_evm::evm::AlpenEvmFactory;
use alpen_reth_rpc::{eth::AlpenEthApiBuilder, SequencerClient};
use reth_chainspec::ChainSpec;
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor, NextBlockEnvAttributes};
use reth_node_api::{FullNodeComponents, NodeAddOns};
use reth_node_builder::{
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    node::{FullNodeTypes, NodeTypes},
    rpc::{
        BasicEngineApiBuilder, BasicEngineValidatorBuilder, EngineApiBuilder, EngineValidatorAddOn,
        EngineValidatorBuilder, EthApiBuilder, Identity, PayloadValidatorBuilder, RethRpcAddOns,
        RpcAddOns, RpcHandle, RpcHooks,
    },
    Node, NodeAdapter, NodeComponentsBuilder,
};
use reth_node_ethereum::node::{EthereumConsensusBuilder, EthereumNetworkBuilder};
use reth_primitives::EthPrimitives;
use reth_provider::EthStorage;
use reth_rpc_eth_types::{error::FromEvmError, EthApiError};
use revm::context::TxEnv;

use crate::{
    engine::AlpenEngineValidatorBuilder, evm::AlpenExecutorBuilder,
    payload_builder::AlpenPayloadBuilderBuilder, pool::AlpenEthereumPoolBuilder, AlpenEngineTypes,
};

/// Which role the node plays on the network.
///
/// The only thing the role changes about the reth node itself is whether
/// submitted transactions are forwarded, so the forwarding target lives on the
/// variant that can have one. A sequencer with a forwarding target is then not
/// expressible.
#[derive(Debug, Clone)]
pub enum AlpenNodeMode {
    /// Builds blocks itself, so it never forwards. It is what full nodes
    /// forward to.
    Sequencer,
    /// Forwards `eth_sendRawTransaction` to the sequencer.
    ///
    /// `sequencer_http` is genuinely optional. Without it the node still serves
    /// reads and follows the chain over gossip and reth P2P, it just can't
    /// accept transaction submissions.
    FullNode { sequencer_http: Option<String> },
}

impl AlpenNodeMode {
    /// Builds [`AlpenNodeMode::Sequencer`].
    pub fn sequencer() -> Self {
        Self::Sequencer
    }

    /// Builds [`AlpenNodeMode::FullNode`].
    pub fn full_node(sequencer_http: Option<String>) -> Self {
        Self::FullNode { sequencer_http }
    }

    /// The URL that submitted transactions are forwarded to, if any.
    fn forward_target(&self) -> Option<String> {
        match self {
            Self::Sequencer => None,
            Self::FullNode { sequencer_http } => sequencer_http.clone(),
        }
    }
}

/// The Alpen EE node type.
///
/// Reth builds components and add-ons from `&self` once the node value has been
/// handed to its builder, so anything they need has to be resolved up front and
/// stored here.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AlpenEthereumNode {
    /// Carries the bridge params the Alpen precompiles validate against. Has to
    /// match what the provers build from the same params, otherwise a block
    /// executes one way on the node and another way in its proof.
    evm_factory: AlpenEvmFactory,
    mode: AlpenNodeMode,
}

impl AlpenEthereumNode {
    pub fn new(evm_factory: AlpenEvmFactory, mode: AlpenNodeMode) -> Self {
        Self { evm_factory, mode }
    }
}

impl NodeTypes for AlpenEthereumNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = AlpenEngineTypes;
}

impl<N> Node<N> for AlpenEthereumNode
where
    N: FullNodeTypes<
        Types: NodeTypes<
            Payload = AlpenEngineTypes,
            ChainSpec = ChainSpec,
            Primitives = EthPrimitives,
            Storage = EthStorage,
        >,
    >,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        AlpenEthereumPoolBuilder,
        BasicPayloadServiceBuilder<AlpenPayloadBuilderBuilder>,
        EthereumNetworkBuilder,
        AlpenExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns = AlpenRethNodeAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        AlpenEthApiBuilder,
        AlpenEngineValidatorBuilder,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(AlpenEthereumPoolBuilder::default())
            .executor(AlpenExecutorBuilder::new(self.evm_factory.clone()))
            .payload(BasicPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }

    fn add_ons(&self) -> Self::AddOns {
        Self::AddOns::builder()
            .with_sequencer(self.mode.forward_target())
            .build()
    }
}

#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct AlpenRethAddOnsBuilder {
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    sequencer_client: Option<SequencerClient>,
}

impl AlpenRethAddOnsBuilder {
    /// With a [`SequencerClient`].
    pub fn with_sequencer(mut self, sequencer_client: Option<String>) -> Self {
        self.sequencer_client = sequencer_client.map(SequencerClient::new);
        self
    }
}

impl AlpenRethAddOnsBuilder {
    /// Builds an instance of [`StrataAddOns`].
    pub fn build<N>(self) -> AlpenRethNodeAddOns<N, AlpenEthApiBuilder, AlpenEngineValidatorBuilder>
    where
        N: FullNodeComponents<Types: NodeTypes<Primitives = EthPrimitives>>,
        AlpenEthApiBuilder: EthApiBuilder<N>,
    {
        let Self { sequencer_client } = self;

        let sequencer_client_clone = sequencer_client.clone();
        AlpenRethNodeAddOns {
            rpc_add_ons: RpcAddOns::new(
                AlpenEthApiBuilder::default().with_sequencer(sequencer_client_clone),
                AlpenEngineValidatorBuilder::default(),
                BasicEngineApiBuilder::default(),
                BasicEngineValidatorBuilder::default(),
                Default::default(),
            ),
        }
    }
}

/// Add-ons for Strata.
#[derive(Debug)]
pub struct AlpenRethNodeAddOns<
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    PVB,
    EB = BasicEngineApiBuilder<PVB>,
    EVB = BasicEngineValidatorBuilder<PVB>,
    RpcMiddleware = Identity,
> {
    /// Rpc add-ons responsible for launching the RPC servers and instantiating the RPC handlers
    /// and eth-api.
    pub rpc_add_ons: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>,
}

impl<N> Default for AlpenRethNodeAddOns<N, AlpenEthApiBuilder, AlpenEngineValidatorBuilder>
where
    N: FullNodeComponents<Types: NodeTypes<Primitives = EthPrimitives>>,
    AlpenEthApiBuilder: EthApiBuilder<N>,
{
    fn default() -> Self {
        Self::builder().build()
    }
}

impl<N> AlpenRethNodeAddOns<N, AlpenEthApiBuilder, AlpenEngineValidatorBuilder>
where
    N: FullNodeComponents<Types: NodeTypes<Primitives = EthPrimitives>>,
    AlpenEthApiBuilder: EthApiBuilder<N>,
{
    /// Build a [`OpAddOns`] using [`OpAddOnsBuilder`].
    pub fn builder() -> AlpenRethAddOnsBuilder {
        AlpenRethAddOnsBuilder::default()
    }
}

impl<N, EthB, PVB, EB, EVB> NodeAddOns<N> for AlpenRethNodeAddOns<N, EthB, PVB, EB, EVB>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec = ChainSpec,
            Primitives = EthPrimitives,
            Storage = EthStorage,
            Payload = AlpenEngineTypes,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N>,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
{
    type Handle = RpcHandle<N, EthB::EthApi>;

    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        self.rpc_add_ons.launch_add_ons(ctx).await
    }
}

impl<N, EthB, PVB, EB, EVB> RethRpcAddOns<N> for AlpenRethNodeAddOns<N, EthB, PVB, EB, EVB>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec = ChainSpec,
            Primitives = EthPrimitives,
            Storage = EthStorage,
            Payload = AlpenEngineTypes,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N>,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
{
    type EthApi = EthB::EthApi;

    fn hooks_mut(&mut self) -> &mut RpcHooks<N, Self::EthApi> {
        self.rpc_add_ons.hooks_mut()
    }
}

impl<N, EthB, PVB, EB, EVB> EngineValidatorAddOn<N> for AlpenRethNodeAddOns<N, EthB, PVB, EB, EVB>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec = ChainSpec,
            Primitives = EthPrimitives,
            Payload = AlpenEngineTypes,
        >,
    >,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
{
    type ValidatorBuilder = EVB;

    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        self.rpc_add_ons.engine_validator_builder()
    }
}
