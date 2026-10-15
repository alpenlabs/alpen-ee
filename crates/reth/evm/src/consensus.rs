//! Header consensus rules shared by the node and the EVM proof.
//!
//! The node validates every block with these rules before the proof exists,
//! and the chunk proof runs the same rules on the blocks it proves. Keeping
//! them in one place is what keeps the two from accepting different chains.
//!
//! [`FlooredConsensus`] holds the rules of one spec version: standard Ethereum
//! consensus with the fee model's base-fee floor. Only the
//! base-fee-against-parent check diverges from [`EthBeaconConsensus`] — full
//! nodes import blocks before the proof exists, so they must accept the
//! floored base fee `max(base_fee_floor, eip1559_next(parent))` (see
//! [`crate::base_fee`]); stock reth recomputes the pure EIP-1559 value and
//! would reject a floored block. Above the floor the two agree. A zero floor
//! validates exactly as stock reth would.
//!
//! The `extra_data` stamp rules hold across versions, so they are free
//! functions: [`validate_header_extra`] and
//! [`validate_header_extra_against_parent`].

use std::sync::Arc;

use alloy_consensus::BlockHeader as _;
use alpen_params::{HeaderExtra, HeaderExtraError};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom};
use reth_consensus_common::validation::{
    validate_against_parent_4844, validate_against_parent_gas_limit,
    validate_against_parent_hash_number, validate_against_parent_timestamp,
};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_ethereum_primitives::{Block, BlockBody, EthPrimitives, Receipt};
use reth_evm::block::BlockExecutionResult;
use reth_primitives_traits::{GotExpected, Header, RecoveredBlock, SealedBlock, SealedHeader};

use crate::{base_fee::expected_floored_base_fee, da_fee::validate_da_rate_against_parent};

fn consensus_error(err: HeaderExtraError) -> ConsensusError {
    ConsensusError::other(err)
}

/// Parses `header`'s full `extra_data` stamp.
///
/// Fails on a malformed or unknown stamp. An empty field is
/// [`AlpenSpecId::V0`](alpen_params::AlpenSpecId::V0)'s layout. Genesis is
/// exempt, since its `extra_data` is the operator-authored genesis document's.
pub fn validate_header_extra(header: &Header) -> Result<HeaderExtra, ConsensusError> {
    HeaderExtra::of_header(header).map_err(consensus_error)
}

/// Checks `header`'s `extra_data` stamp against its parent's and returns the
/// header's stamp.
///
/// Upgrades only ever move forward, so a version that regresses is invalid.
/// The DA rate may rise by at most the quote margin over the parent's rate.
pub fn validate_header_extra_against_parent(
    header: &Header,
    parent: &Header,
) -> Result<HeaderExtra, ConsensusError> {
    let header_extra = validate_header_extra(header)?;
    let parent_extra = validate_header_extra(parent)?;

    let version = header_extra.spec_version();
    let parent_version = parent_extra.spec_version();
    if version < parent_version {
        return Err(ConsensusError::msg(format!(
            "alpen spec version regressed from {parent_version:?} to {version:?}"
        )));
    }

    if let Some(next_rate) = header_extra.da_rate() {
        validate_da_rate_against_parent(parent_extra.da_rate(), next_rate)
            .map_err(ConsensusError::other)?;
    }

    Ok(header_extra)
}

/// Consensus rules of one spec version: standard Ethereum consensus, with the
/// base-fee-against-parent check replaced by the fee model's floored rule.
#[derive(Debug, Clone)]
pub struct FlooredConsensus {
    inner: EthBeaconConsensus<ChainSpec>,
    chain_spec: Arc<ChainSpec>,
    base_fee_floor: u64,
}

impl FlooredConsensus {
    /// Creates a [`FlooredConsensus`] for the given chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>, base_fee_floor: u64) -> Self {
        Self {
            inner: EthBeaconConsensus::new(chain_spec.clone()),
            chain_spec,
            base_fee_floor,
        }
    }
}

/// Floored variant of reth's `validate_against_parent_eip1559_base_fee`: the expected base
/// fee is `max(base_fee_floor, eip1559_next(parent))` — identical to stock except for the
/// floor clamp.
fn validate_against_parent_base_fee_floored(
    header: &Header,
    parent: &Header,
    chain_spec: &ChainSpec,
    base_fee_floor: u64,
) -> Result<(), ConsensusError> {
    if chain_spec.is_london_active_at_block(header.number()) {
        let base_fee = header
            .base_fee_per_gas()
            .ok_or(ConsensusError::BaseFeeMissing)?;

        let expected_base_fee =
            expected_floored_base_fee(header, parent, chain_spec, base_fee_floor)
                .ok_or(ConsensusError::BaseFeeMissing)?;

        if expected_base_fee != base_fee {
            return Err(ConsensusError::BaseFeeDiff(GotExpected {
                expected: expected_base_fee,
                got: base_fee,
            }));
        }
    }

    Ok(())
}

impl HeaderValidator for FlooredConsensus {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        self.inner.validate_header(header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        // Mirrors `EthBeaconConsensus::validate_header_against_parent`, but floors the
        // base-fee-against-parent check.
        validate_against_parent_hash_number(header.header(), parent)?;
        validate_against_parent_timestamp(header.header(), parent.header())?;
        validate_against_parent_gas_limit(header, parent, &self.chain_spec)?;
        validate_against_parent_base_fee_floored(
            header.header(),
            parent.header(),
            self.chain_spec.as_ref(),
            self.base_fee_floor,
        )?;
        if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp()) {
            validate_against_parent_4844(header.header(), parent.header(), blob_params)?;
        }
        Ok(())
    }
}

impl Consensus<Block> for FlooredConsensus {
    fn validate_body_against_header(
        &self,
        body: &BlockBody,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        <EthBeaconConsensus<ChainSpec> as Consensus<Block>>::validate_body_against_header(
            &self.inner,
            body,
            header,
        )
    }

    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<Block>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_block_pre_execution(block)
    }
}

impl FullConsensus<EthPrimitives> for FlooredConsensus {
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<Block>,
        result: &BlockExecutionResult<Receipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        <EthBeaconConsensus<ChainSpec> as FullConsensus<EthPrimitives>>::validate_block_post_execution(
            &self.inner,
            block,
            result,
            receipt_root_bloom,
        )
    }
}
