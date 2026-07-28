//! Version-aware consensus: header and block validation dispatched by the
//! spec version stamped in each header's `extra_data`.
//!
//! Wraps one [`FlooredConsensus`] per known spec version and picks per block,
//! so one node validates both sides of an upgrade during sync and reorgs.
//! Beyond dispatch it enforces the shape of the version claim itself:
//! `validate_header` runs the full [`HeaderExtra`] layout parse (strict — a
//! malformed or unknown stamp fails the block, though an absent one reads as
//! [`AlpenSpecId::V0`]), and versions never regress along a chain. Whether the
//! claimed version *equals* the one derived from the inbox ordering is the
//! Alpen layer's check, where the inbox data lives.
//!
//! Each per-version unit is a [`FlooredConsensus`]: standard Ethereum
//! consensus with the fee model's base-fee floor. Only the
//! base-fee-against-parent check diverges from [`EthBeaconConsensus`] — full
//! nodes import blocks before the proof exists, so they must accept the
//! floored base fee `max(BASE_FEE_FLOOR, eip1559_next(parent))` (see
//! [`alpen_reth_evm::base_fee`]); stock reth recomputes the pure EIP-1559
//! value and would reject a floored block. Above the floor the two agree.

use std::sync::Arc;

use alloy_consensus::BlockHeader as _;
use alpen_ee_params::{header_spec_version, AlpenSpecId, EvmSpec, HeaderExtra, HeaderExtraError};
use alpen_reth_evm::base_fee::expected_floored_base_fee;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_consensus::{Consensus, FullConsensus, HeaderValidator};
use reth_consensus_common::validation::{
    validate_against_parent_4844, validate_against_parent_gas_limit,
    validate_against_parent_hash_number, validate_against_parent_timestamp,
};
use reth_errors::ConsensusError;
use reth_ethereum_primitives::BlockBody;
use reth_evm::block::BlockExecutionResult;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{components::ConsensusBuilder, BuilderContext};
use reth_node_ethereum::consensus::EthBeaconConsensus;
use reth_primitives::{
    Block, EthPrimitives, Header, Receipt, RecoveredBlock, SealedBlock, SealedHeader,
};
use reth_primitives_traits::GotExpected;

use crate::evm_config::version_indexed;

fn consensus_error(err: HeaderExtraError) -> ConsensusError {
    ConsensusError::Other(err.to_string())
}

/// Consensus rules of one spec version: standard Ethereum consensus, with the
/// base-fee-against-parent check replaced by the fee model's floored rule.
#[derive(Debug, Clone)]
pub(crate) struct FlooredConsensus {
    inner: EthBeaconConsensus<ChainSpec>,
    chain_spec: Arc<ChainSpec>,
}

impl FlooredConsensus {
    /// Creates a [`FlooredConsensus`] for the given chain spec.
    pub(crate) fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            inner: EthBeaconConsensus::new(chain_spec.clone()),
            chain_spec,
        }
    }
}

/// Floored variant of reth's `validate_against_parent_eip1559_base_fee`: the expected base
/// fee is `max(BASE_FEE_FLOOR, eip1559_next(parent))` — identical to stock except for the
/// [`apply_base_fee_floor`] clamp.
fn validate_against_parent_base_fee_floored(
    header: &Header,
    parent: &Header,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    if chain_spec.is_london_active_at_block(header.number()) {
        let base_fee = header
            .base_fee_per_gas()
            .ok_or(ConsensusError::BaseFeeMissing)?;

        // Single source of truth shared with the proof guest (`evm-ee`), so host and guest
        // enforce byte-identical base fees.
        let expected_base_fee = expected_floored_base_fee(header, parent, chain_spec)
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
        )?;
        if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp()) {
            validate_against_parent_4844(header.header(), parent.header(), blob_params)?;
        }
        Ok(())
    }
}

impl Consensus<Block> for FlooredConsensus {
    type Error = ConsensusError;

    fn validate_body_against_header(
        &self,
        body: &BlockBody,
        header: &SealedHeader,
    ) -> Result<(), Self::Error> {
        <EthBeaconConsensus<ChainSpec> as Consensus<Block>>::validate_body_against_header(
            &self.inner,
            body,
            header,
        )
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock<Block>) -> Result<(), Self::Error> {
        self.inner.validate_block_pre_execution(block)
    }
}

impl FullConsensus<EthPrimitives> for FlooredConsensus {
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<Block>,
        result: &BlockExecutionResult<Receipt>,
    ) -> Result<(), ConsensusError> {
        <EthBeaconConsensus<ChainSpec> as FullConsensus<EthPrimitives>>::validate_block_post_execution(
            &self.inner,
            block,
            result,
        )
    }
}

/// Version-aware consensus over the per-version chain spec table.
#[derive(Debug, Clone)]
pub struct AlpenConsensus {
    /// Consensus rules of each known [`AlpenSpecId`], indexed by
    /// discriminant.
    inners: Vec<FlooredConsensus>,
}

impl AlpenConsensus {
    /// Creates the consensus over `evm_spec`'s per-version chain spec table.
    pub fn new(evm_spec: &EvmSpec) -> Self {
        Self {
            inners: evm_spec
                .chain_specs()
                .iter()
                .cloned()
                .map(FlooredConsensus::new)
                .collect(),
        }
    }

    /// Returns the consensus rules governing `header`, erring on a stamp
    /// that does not resolve to a version.
    fn inner_for(&self, header: &Header) -> Result<&FlooredConsensus, ConsensusError> {
        let spec_version = header_spec_version(header).map_err(consensus_error)?;
        Ok(version_indexed(&self.inners, spec_version))
    }
}

impl HeaderValidator for AlpenConsensus {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        // The one full-layout checkpoint: dispatch sites elsewhere only peek
        // the version prefix, so this parse is what rejects `extra_data`
        // that violates its version's layout. Genesis is exempt — its
        // `extra_data` is the operator-authored genesis document's.
        let spec_version = if header.number == 0 {
            AlpenSpecId::V0
        } else {
            HeaderExtra::decode(&header.extra_data)
                .map_err(consensus_error)?
                .spec_version()
        };
        version_indexed(&self.inners, spec_version).validate_header(header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        // Upgrades only ever move forward: a chain whose version regresses
        // is structurally invalid regardless of what the inbox ordering
        // would derive.
        let version = header_spec_version(header.header()).map_err(consensus_error)?;
        let parent_version = header_spec_version(parent.header()).map_err(consensus_error)?;
        if version < parent_version {
            return Err(ConsensusError::Other(format!(
                "alpen spec version regressed from {parent_version:?} to {version:?}"
            )));
        }
        version_indexed(&self.inners, version).validate_header_against_parent(header, parent)
    }
}

impl Consensus<Block> for AlpenConsensus {
    type Error = ConsensusError;

    fn validate_body_against_header(
        &self,
        body: &BlockBody,
        header: &SealedHeader,
    ) -> Result<(), Self::Error> {
        let inner = self.inner_for(header.header())?;
        Consensus::<Block>::validate_body_against_header(inner, body, header)
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock<Block>) -> Result<(), Self::Error> {
        self.inner_for(block.header())?
            .validate_block_pre_execution(block)
    }
}

impl FullConsensus<EthPrimitives> for AlpenConsensus {
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<Block>,
        result: &BlockExecutionResult<Receipt>,
    ) -> Result<(), ConsensusError> {
        let inner = self.inner_for(block.header())?;
        FullConsensus::<EthPrimitives>::validate_block_post_execution(inner, block, result)
    }
}

/// Builds [`AlpenConsensus`] over the per-version chain spec table.
#[derive(Debug, Clone)]
pub struct AlpenConsensusBuilder {
    evm_spec: EvmSpec,
}

impl AlpenConsensusBuilder {
    pub fn new(evm_spec: EvmSpec) -> Self {
        Self { evm_spec }
    }
}

impl<Node> ConsensusBuilder<Node> for AlpenConsensusBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type Consensus = Arc<AlpenConsensus>;

    async fn build_consensus(self, _ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(AlpenConsensus::new(&self.evm_spec)))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use alpen_ee_params::{AlpenSpecId, EvmSpec, HeaderExtra};
    use alpen_reth_evm::base_fee::BASE_FEE_FLOOR;
    use reth_consensus::HeaderValidator;
    use reth_errors::ConsensusError;
    use reth_primitives::{Header, SealedHeader};

    use super::AlpenConsensus;

    fn test_consensus() -> AlpenConsensus {
        let evm_spec: EvmSpec = serde_json::from_str("{}").expect("empty genesis document parses");
        AlpenConsensus::new(&evm_spec)
    }

    fn sealed_header(number: u64, extra_data: Bytes) -> SealedHeader {
        SealedHeader::seal_slow(Header {
            number,
            extra_data,
            ..Default::default()
        })
    }

    fn stamped_header(spec_version: AlpenSpecId) -> SealedHeader {
        sealed_header(1, HeaderExtra::new(spec_version, 0).encode().into())
    }

    #[test]
    fn version_regression_is_refused() {
        let consensus = test_consensus();
        let parent = stamped_header(AlpenSpecId::V1);
        let child = stamped_header(AlpenSpecId::V0);

        let err = consensus
            .validate_header_against_parent(&child, &parent)
            .expect_err("regressing from v1 to v0 is structurally invalid");
        assert!(
            matches!(&err, ConsensusError::Other(msg) if msg.contains("regressed")),
            "{err:?}"
        );
    }

    #[test]
    fn unknown_version_is_refused() {
        let consensus = test_consensus();
        let header = sealed_header(1, Bytes::from_static(&[0x00, 0x07]));

        let err = consensus
            .validate_header(&header)
            .expect_err("v7 is past this binary's versions");
        assert!(
            matches!(&err, ConsensusError::Other(msg) if msg.contains("no spec version")),
            "{err:?}"
        );
    }

    /// `validate_header` runs the full layout parse, not just the version
    /// prefix: bytes past the version's layout fail the header.
    #[test]
    fn layout_violation_is_refused() {
        let consensus = test_consensus();
        let mut extra_data = HeaderExtra::new(AlpenSpecId::V1, 0).encode();
        extra_data.push(0xFF);
        let header = sealed_header(1, extra_data.into());

        let err = consensus
            .validate_header(&header)
            .expect_err("trailing bytes violate v1's layout");
        assert!(
            matches!(&err, ConsensusError::Other(msg) if msg.contains("layout")),
            "{err:?}"
        );
    }

    /// The fee model's base-fee floor survives per-version dispatch: a child
    /// carrying the floored base fee validates against its parent under every
    /// version, where stock consensus would demand the pure EIP-1559 value.
    #[test]
    fn the_base_fee_floor_applies_under_every_version() {
        // London must be active for the base-fee-against-parent rule to run
        // at all; the empty genesis document never activates it.
        let evm_spec: EvmSpec = serde_json::from_str(
            r#"{"config":{"chainId":2892,"londonBlock":0,"shanghaiTime":0}}"#,
        )
        .expect("genesis document parses");
        let consensus = AlpenConsensus::new(&evm_spec);

        for version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            let parent = SealedHeader::seal_slow(Header {
                number: 1,
                gas_limit: 30_000_000,
                gas_used: 0,
                base_fee_per_gas: Some(BASE_FEE_FLOOR),
                extra_data: HeaderExtra::new(version, 0).encode().into(),
                ..Default::default()
            });
            // An empty parent drives EIP-1559 below the floor, so the pure
            // recurrence and the floored rule disagree here.
            let child = SealedHeader::seal_slow(Header {
                number: 2,
                parent_hash: parent.hash(),
                gas_limit: 30_000_000,
                timestamp: 1,
                base_fee_per_gas: Some(BASE_FEE_FLOOR),
                extra_data: HeaderExtra::new(version, 0).encode().into(),
                ..Default::default()
            });

            assert_eq!(
                consensus.validate_header_against_parent(&child, &parent),
                Ok(()),
                "{version:?}"
            );
        }
    }

    /// The genesis header's operator-authored `extra_data` is never parsed;
    /// block 0 validates under v0.
    #[test]
    fn genesis_header_is_exempt_from_the_layout() {
        let consensus = test_consensus();
        let genesis = sealed_header(0, Bytes::from_static(b"SC"));

        assert_eq!(consensus.validate_header(&genesis), Ok(()));
    }

    /// An existing chain must be able to cross into the stamped format: a
    /// newly stamped child validates against a legacy (unstamped) tip, and
    /// legacy-against-legacy keeps working behind it.
    #[test]
    fn stamped_child_validates_against_a_legacy_tip() {
        let consensus = test_consensus();

        let legacy_parent = SealedHeader::seal_slow(Header {
            number: 100,
            gas_limit: 30_000_000,
            extra_data: Default::default(),
            ..Default::default()
        });

        // legacy tip validates on its own
        assert_eq!(consensus.validate_header(&legacy_parent), Ok(()));

        // legacy -> legacy
        let legacy_child = SealedHeader::seal_slow(Header {
            number: 101,
            parent_hash: legacy_parent.hash(),
            gas_limit: 30_000_000,
            timestamp: 1,
            extra_data: Default::default(),
            ..Default::default()
        });
        assert_eq!(consensus.validate_header(&legacy_child), Ok(()));
        assert_eq!(
            consensus.validate_header_against_parent(&legacy_child, &legacy_parent),
            Ok(())
        );

        // legacy -> first stamped child (the activation boundary)
        for version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            let stamped_child = SealedHeader::seal_slow(Header {
                number: 101,
                parent_hash: legacy_parent.hash(),
                gas_limit: 30_000_000,
                timestamp: 1,
                extra_data: HeaderExtra::new(version, 0).encode().into(),
                ..Default::default()
            });
            assert_eq!(
                consensus.validate_header(&stamped_child),
                Ok(()),
                "{version:?}"
            );
            assert_eq!(
                consensus.validate_header_against_parent(&stamped_child, &legacy_parent),
                Ok(()),
                "{version:?}"
            );
        }
    }
}
