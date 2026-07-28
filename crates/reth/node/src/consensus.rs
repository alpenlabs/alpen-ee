//! Version-aware consensus: header and block validation dispatched by the
//! spec version stamped in each header's `extra_data`.
//!
//! Wraps one [`EthBeaconConsensus`] per known spec version and picks per
//! block, so one node validates both sides of an upgrade during sync and
//! reorgs. Beyond dispatch it enforces the shape of the version claim
//! itself: `validate_header` runs the full [`HeaderExtra`] layout parse
//! (strict — an unknown or malformed stamp fails the block), and versions
//! never regress along a chain. Whether the claimed version *equals* the one
//! derived from the inbox ordering is the Alpen layer's check, where the
//! inbox data lives.

use std::sync::Arc;

use alpen_ee_params::{header_spec_version, AlpenSpecId, EvmSpec, HeaderExtra, HeaderExtraError};
use reth_chainspec::ChainSpec;
use reth_consensus::{Consensus, FullConsensus, HeaderValidator};
use reth_errors::ConsensusError;
use reth_ethereum_primitives::BlockBody;
use reth_evm::block::BlockExecutionResult;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{components::ConsensusBuilder, BuilderContext};
use reth_node_ethereum::consensus::EthBeaconConsensus;
use reth_primitives::{
    Block, EthPrimitives, Header, Receipt, RecoveredBlock, SealedBlock, SealedHeader,
};

use crate::evm_config::version_indexed;

fn consensus_error(err: HeaderExtraError) -> ConsensusError {
    ConsensusError::Other(err.to_string())
}

/// Version-aware consensus over the per-version chain spec table.
#[derive(Debug, Clone)]
pub struct AlpenConsensus {
    /// Consensus rules of each known [`AlpenSpecId`], indexed by
    /// discriminant.
    inners: Vec<EthBeaconConsensus<ChainSpec>>,
}

impl AlpenConsensus {
    /// Creates the consensus over `evm_spec`'s per-version chain spec table.
    pub fn new(evm_spec: &EvmSpec) -> Self {
        Self {
            inners: evm_spec
                .chain_specs()
                .iter()
                .cloned()
                .map(EthBeaconConsensus::new)
                .collect(),
        }
    }

    /// Returns the consensus rules governing `header`, erring on a stamp
    /// that does not resolve to a version.
    fn inner_for(&self, header: &Header) -> Result<&EthBeaconConsensus<ChainSpec>, ConsensusError> {
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
        sealed_header(1, HeaderExtra::new(spec_version).encode().into())
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
        let header = sealed_header(1, Bytes::from_static(&[0x00, 0x01, 0xFF]));

        let err = consensus
            .validate_header(&header)
            .expect_err("trailing bytes violate v1's layout");
        assert!(
            matches!(&err, ConsensusError::Other(msg) if msg.contains("layout")),
            "{err:?}"
        );
    }

    /// The genesis header's operator-authored `extra_data` is never parsed;
    /// block 0 validates under v0.
    #[test]
    fn genesis_header_is_exempt_from_the_layout() {
        let consensus = test_consensus();
        let genesis = sealed_header(0, Bytes::from_static(b"SC"));

        assert_eq!(consensus.validate_header(&genesis), Ok(()));
    }
}
