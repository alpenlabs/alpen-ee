//! Alpen consensus: standard Ethereum consensus with the fee-model base-fee floor.
//!
//! The only divergence from [`EthBeaconConsensus`] is the base-fee-against-parent check at
//! header validation: full nodes import blocks before the proof exists, so they must accept
//! the floored base fee `max(BASE_FEE_FLOOR, eip1559_next(parent))` (see
//! [`alpen_reth_evm::base_fee`]). Stock reth recomputes the pure EIP-1559 value and would
//! reject a floored block. Every other check is delegated to the inner consensus.
//!
//! Above the floor this matches stock consensus (floored == pure EIP-1559); it diverges only
//! when EIP-1559 would put the base fee below `BASE_FEE_FLOOR`.

use std::{fmt::Debug, sync::Arc};

use alloy_consensus::BlockHeader as _;
use alloy_eips::eip1559::INITIAL_BASE_FEE;
use alpen_reth_evm::base_fee::apply_base_fee_floor;
use reth_chainspec::{EthChainSpec, EthereumHardfork, EthereumHardforks};
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator};
use reth_consensus_common::validation::{
    validate_against_parent_4844, validate_against_parent_gas_limit,
    validate_against_parent_hash_number, validate_against_parent_timestamp,
};
use reth_execution_types::BlockExecutionResult;
use reth_node_builder::{
    components::ConsensusBuilder,
    node::{FullNodeTypes, NodeTypes},
    BuilderContext,
};
use reth_node_ethereum::consensus::EthBeaconConsensus;
use reth_primitives::EthPrimitives;
use reth_primitives_traits::{
    Block, BlockHeader, GotExpected, NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader,
};

/// Consensus for the Alpen EE: standard Ethereum consensus, with the base-fee-against-parent
/// check replaced by the fee-model floored rule `max(BASE_FEE_FLOOR, eip1559_next(parent))`.
#[derive(Debug, Clone)]
pub struct AlpenConsensus<ChainSpec> {
    inner: EthBeaconConsensus<ChainSpec>,
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec: EthChainSpec + EthereumHardforks> AlpenConsensus<ChainSpec> {
    /// Creates an [`AlpenConsensus`] for the given chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            inner: EthBeaconConsensus::new(chain_spec.clone()),
            chain_spec,
        }
    }
}

/// Floored variant of reth's `validate_against_parent_eip1559_base_fee`: the expected base
/// fee is `max(BASE_FEE_FLOOR, eip1559_next(parent))` — identical to stock except for the
/// [`apply_base_fee_floor`] clamp.
fn validate_against_parent_base_fee_floored<ChainSpec>(
    header: &ChainSpec::Header,
    parent: &ChainSpec::Header,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError>
where
    ChainSpec: EthChainSpec + EthereumHardforks,
{
    if chain_spec.is_london_active_at_block(header.number()) {
        let base_fee = header
            .base_fee_per_gas()
            .ok_or(ConsensusError::BaseFeeMissing)?;

        let expected_base_fee = if chain_spec
            .ethereum_fork_activation(EthereumHardfork::London)
            .transitions_at_block(header.number())
        {
            INITIAL_BASE_FEE
        } else {
            apply_base_fee_floor(
                chain_spec
                    .next_block_base_fee(parent, header.timestamp())
                    .ok_or(ConsensusError::BaseFeeMissing)?,
            )
        };

        if expected_base_fee != base_fee {
            return Err(ConsensusError::BaseFeeDiff(GotExpected {
                expected: expected_base_fee,
                got: base_fee,
            }));
        }
    }

    Ok(())
}

impl<H, ChainSpec> HeaderValidator<H> for AlpenConsensus<ChainSpec>
where
    H: BlockHeader,
    ChainSpec: EthChainSpec<Header = H> + EthereumHardforks + Debug + Send + Sync,
{
    fn validate_header(&self, header: &SealedHeader<H>) -> Result<(), ConsensusError> {
        self.inner.validate_header(header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        // Mirrors `EthBeaconConsensus::validate_header_against_parent`, but floors the
        // base-fee-against-parent check (fee-model D).
        validate_against_parent_hash_number(header.header(), parent)?;
        validate_against_parent_timestamp(header.header(), parent.header())?;
        validate_against_parent_gas_limit(header, parent, &self.chain_spec)?;
        validate_against_parent_base_fee_floored(
            header.header(),
            parent.header(),
            self.chain_spec.as_ref(),
        )?;
        if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp()) {
            validate_against_parent_4844(header.header(), parent.header(), blob_params)?;
        }
        Ok(())
    }
}

impl<B, ChainSpec> Consensus<B> for AlpenConsensus<ChainSpec>
where
    B: Block,
    ChainSpec: EthChainSpec<Header = B::Header> + EthereumHardforks + Debug + Send + Sync,
{
    type Error = ConsensusError;

    fn validate_body_against_header(
        &self,
        body: &B::Body,
        header: &SealedHeader<B::Header>,
    ) -> Result<(), Self::Error> {
        <EthBeaconConsensus<ChainSpec> as Consensus<B>>::validate_body_against_header(
            &self.inner,
            body,
            header,
        )
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock<B>) -> Result<(), Self::Error> {
        self.inner.validate_block_pre_execution(block)
    }
}

impl<N, ChainSpec> FullConsensus<N> for AlpenConsensus<ChainSpec>
where
    N: NodePrimitives,
    ChainSpec: Send + Sync + EthChainSpec<Header = N::BlockHeader> + EthereumHardforks + Debug,
{
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<N::Block>,
        result: &BlockExecutionResult<N::Receipt>,
    ) -> Result<(), ConsensusError> {
        <EthBeaconConsensus<ChainSpec> as FullConsensus<N>>::validate_block_post_execution(
            &self.inner,
            block,
            result,
        )
    }
}

/// Builds [`AlpenConsensus`] in place of the stock `EthereumConsensusBuilder`.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct AlpenConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for AlpenConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<ChainSpec: EthChainSpec + EthereumHardforks, Primitives = EthPrimitives>,
    >,
{
    type Consensus = Arc<AlpenConsensus<<Node::Types as NodeTypes>::ChainSpec>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(AlpenConsensus::new(ctx.chain_spec())))
    }
}
