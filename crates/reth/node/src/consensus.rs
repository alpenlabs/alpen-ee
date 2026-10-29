//! Version-aware consensus: header and block validation dispatched by the
//! spec version stamped in each header's `extra_data`.
//!
//! Wraps one [`FlooredConsensus`] per known spec version and picks per block,
//! so one node validates both sides of an upgrade during sync and reorgs.
//! Each unit gets its version's floor from the params' [`FeeSpec`].
//!
//! The rules themselves live in [`alpen_reth_evm::consensus`], which the chunk
//! proof runs too. This module only adds the dispatch. Whether the claimed
//! version *equals* the one derived from the inbox ordering is the Alpen
//! layer's check, where the inbox data lives.

use std::{iter, sync::Arc};

use alpen_params::{header_spec_version, AlpenSpecId, EvmSpec, FeeSpec};
use alpen_reth_evm::consensus::{
    validate_header_extra, validate_header_extra_against_parent, FlooredConsensus,
};
use reth_chainspec::ChainSpec;
use reth_consensus::{Consensus, FullConsensus, HeaderValidator, ReceiptRootBloom};
use reth_errors::ConsensusError;
use reth_ethereum_primitives::{Block, BlockBody, EthPrimitives, Receipt};
use reth_evm::block::BlockExecutionResult;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{components::ConsensusBuilder, BuilderContext};
use reth_primitives_traits::{Header, RecoveredBlock, SealedBlock, SealedHeader};

use crate::evm_config::version_indexed;

/// Version-aware consensus over the per-version chain spec table.
#[derive(Debug, Clone)]
pub struct AlpenConsensus {
    /// Consensus rules of each known [`AlpenSpecId`], indexed by
    /// discriminant.
    inners: Vec<FlooredConsensus>,
}

impl AlpenConsensus {
    /// Creates the consensus over `evm_spec`'s per-version chain spec table,
    /// holding each version to its base-fee floor in `fee_spec`.
    pub fn new(evm_spec: &EvmSpec, fee_spec: &FeeSpec) -> Self {
        Self {
            inners: iter::successors(Some(AlpenSpecId::V0), |version| version.successor().ok())
                .map(|version| {
                    FlooredConsensus::new(
                        evm_spec.chain_spec(version).clone(),
                        fee_spec.base_fee_floor(version),
                    )
                })
                .collect(),
        }
    }

    /// Returns the consensus rules governing `header`, erring on a stamp
    /// that does not resolve to a version.
    fn inner_for(&self, header: &Header) -> Result<&FlooredConsensus, ConsensusError> {
        let spec_version = header_spec_version(header).map_err(ConsensusError::other)?;
        Ok(version_indexed(&self.inners, spec_version))
    }
}

impl HeaderValidator for AlpenConsensus {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        // The one full-layout checkpoint: dispatch sites elsewhere only peek
        // the version prefix, so this parse is what rejects `extra_data`
        // that violates its version's layout. Genesis is exempt — its
        // `extra_data` is the operator-authored genesis document's.
        let spec_version = validate_header_extra(header.header())?.spec_version();
        version_indexed(&self.inners, spec_version).validate_header(header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        let version =
            validate_header_extra_against_parent(header.header(), parent.header())?.spec_version();
        version_indexed(&self.inners, version).validate_header_against_parent(header, parent)
    }
}

impl Consensus<Block> for AlpenConsensus {
    fn validate_body_against_header(
        &self,
        body: &BlockBody,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        let inner = self.inner_for(header.header())?;
        Consensus::<Block>::validate_body_against_header(inner, body, header)
    }

    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<Block>,
    ) -> Result<(), ConsensusError> {
        self.inner_for(block.header())?
            .validate_block_pre_execution(block)
    }
}

impl FullConsensus<EthPrimitives> for AlpenConsensus {
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<Block>,
        result: &BlockExecutionResult<Receipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        let inner = self.inner_for(block.header())?;
        FullConsensus::<EthPrimitives>::validate_block_post_execution(
            inner,
            block,
            result,
            receipt_root_bloom,
        )
    }
}

/// Builds [`AlpenConsensus`] over the per-version chain spec table.
#[derive(Debug, Clone)]
pub struct AlpenConsensusBuilder {
    evm_spec: EvmSpec,
    fee_spec: FeeSpec,
}

impl AlpenConsensusBuilder {
    pub fn new(evm_spec: EvmSpec, fee_spec: FeeSpec) -> Self {
        Self { evm_spec, fee_spec }
    }
}

impl<Node> ConsensusBuilder<Node> for AlpenConsensusBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type Consensus = Arc<AlpenConsensus>;

    async fn build_consensus(self, _ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(AlpenConsensus::new(
            &self.evm_spec,
            &self.fee_spec,
        )))
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip1559::INITIAL_BASE_FEE;
    use alloy_primitives::Bytes;
    use alpen_params::{AlpenSpecId, EvmSpec, FeeSpec, HeaderExtra, SpecVersioned};
    use reth_consensus::HeaderValidator;
    use reth_errors::ConsensusError;
    use reth_primitives_traits::{Header, SealedHeader};

    use super::AlpenConsensus;

    /// Mirrors the fork set every real Alpen network ships (see the `evm_spec`
    /// in `params/*.json`): London, Shanghai, Cancun and
    /// Prague all at 0, with v1's Osaka layered on by its own delta. A
    /// stripped-down document would leave Osaka sitting on no Cancun, a
    /// combination no chain has and no header can satisfy.
    const TEST_GENESIS: &str = r#"{"config":{"chainId":2892,"londonBlock":0,
        "shanghaiTime":0,"cancunTime":0,"pragueTime":0}}"#;

    /// Where an idle chain's base fee settles under plain EIP-1559: at 7 wei
    /// the 1/8 decay rounds to zero. Deployed chains with idle blocks sit
    /// there.
    const DECAYED_BASE_FEE: u64 = 7;

    const BASE_FEE_FLOOR: u64 = 1_000_000_000;

    /// The fee spec of a deployed chain: plain EIP-1559 under V0, and the
    /// floor from V1 on.
    fn deployed_fee_spec() -> FeeSpec {
        FeeSpec::new(SpecVersioned::new(0).with(AlpenSpecId::V1, BASE_FEE_FLOOR))
    }

    fn test_consensus() -> AlpenConsensus {
        let evm_spec: EvmSpec = serde_json::from_str(TEST_GENESIS).expect("genesis parses");
        AlpenConsensus::new(&evm_spec, &deployed_fee_spec())
    }

    fn sealed_header(number: u64, extra_data: Bytes) -> SealedHeader {
        SealedHeader::seal_slow(Header {
            number,
            extra_data,
            ..Default::default()
        })
    }

    /// A header carrying every field [`TEST_GENESIS`]'s forks require, so
    /// validation reaches the rules under test instead of stopping at a
    /// missing field. Callers override what they actually care about.
    fn valid_header(number: u64, extra_data: Bytes) -> Header {
        Header {
            number,
            extra_data,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(BASE_FEE_FLOOR),
            withdrawals_root: Some(Default::default()),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(Default::default()),
            requests_hash: Some(Default::default()),
            ..Default::default()
        }
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
            matches!(&err, ConsensusError::Other(msg) if msg.to_string().contains("regressed")),
            "{err:?}"
        );
    }

    #[test]
    fn da_rate_increase_above_quote_margin_is_refused() {
        let consensus = test_consensus();
        let parent = SealedHeader::seal_slow(valid_header(
            1,
            HeaderExtra::new(AlpenSpecId::V1, 1_000).encode().into(),
        ));
        let child = |rate| {
            SealedHeader::seal_slow(Header {
                parent_hash: parent.hash(),
                timestamp: 1,
                ..valid_header(2, HeaderExtra::new(AlpenSpecId::V1, rate).encode().into())
            })
        };

        assert!(consensus
            .validate_header_against_parent(&child(1_100), &parent)
            .is_ok());
        let err = consensus
            .validate_header_against_parent(&child(1_101), &parent)
            .expect_err("DA rate increases above the quote margin must be invalid");
        assert!(
            matches!(&err, ConsensusError::Other(msg) if msg.to_string().contains("exceeds maximum")),
            "{err:?}"
        );
    }

    /// A V0 parent has no rate, so the first V1 block may start at any rate.
    #[test]
    fn da_rate_is_unbounded_over_a_v0_parent() {
        let consensus = test_consensus();
        let parent = SealedHeader::seal_slow(Header {
            base_fee_per_gas: Some(DECAYED_BASE_FEE),
            ..valid_header(1, HeaderExtra::new(AlpenSpecId::V0, 0).encode().into())
        });
        let child = SealedHeader::seal_slow(Header {
            parent_hash: parent.hash(),
            timestamp: 1,
            ..valid_header(
                2,
                HeaderExtra::new(AlpenSpecId::V1, u64::MAX).encode().into(),
            )
        });

        let result = consensus.validate_header_against_parent(&child, &parent);
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn unknown_version_is_refused() {
        let consensus = test_consensus();
        let header = sealed_header(1, Bytes::from_static(&[0x00, 0x07]));

        let err = consensus
            .validate_header(&header)
            .expect_err("v7 is past this binary's versions");
        assert!(
            matches!(&err, ConsensusError::Other(msg) if msg.to_string().contains("no spec version")),
            "{err:?}"
        );
    }

    /// `validate_header` runs the full layout parse, not just the version
    /// prefix: bytes past the version's layout fail the header, and so does
    /// any byte under V0, whose layout is empty.
    #[test]
    fn layout_violation_is_refused() {
        let consensus = test_consensus();
        let mut v1_with_trailing_byte = HeaderExtra::new(AlpenSpecId::V1, 0).encode();
        v1_with_trailing_byte.push(0xFF);
        let v0_with_rate = [0u16.to_be_bytes().as_slice(), &1_000u64.to_be_bytes()].concat();

        for extra_data in [v1_with_trailing_byte, v0_with_rate] {
            let header = sealed_header(1, extra_data.into());
            let err = consensus
                .validate_header(&header)
                .expect_err("the bytes violate the named version's layout");
            assert!(
                matches!(&err, ConsensusError::Other(msg) if msg.to_string().contains("layout")),
                "{err:?}"
            );
        }
    }

    /// A child of `parent` stamped with `spec_version` and carrying `base_fee`.
    fn child_with_base_fee(
        parent: &SealedHeader,
        spec_version: AlpenSpecId,
        base_fee: u64,
    ) -> SealedHeader {
        SealedHeader::seal_slow(Header {
            parent_hash: parent.hash(),
            timestamp: parent.timestamp + 1,
            base_fee_per_gas: Some(base_fee),
            ..valid_header(
                parent.number + 1,
                HeaderExtra::new(spec_version, 0).encode().into(),
            )
        })
    }

    fn assert_base_fee_diff(result: Result<(), ConsensusError>) {
        assert!(
            matches!(result, Err(ConsensusError::BaseFeeDiff(_))),
            "{result:?}"
        );
    }

    /// A deployed chain's V0 has no floor, so a V0 child of an empty block at
    /// the EIP-1559 fixed point keeps that base fee, and a floored one is
    /// refused.
    #[test]
    fn v0_without_a_floor_follows_plain_eip1559() {
        let consensus = test_consensus();
        let parent = SealedHeader::seal_slow(Header {
            base_fee_per_gas: Some(DECAYED_BASE_FEE),
            ..valid_header(1, HeaderExtra::new(AlpenSpecId::V0, 0).encode().into())
        });

        let unfloored = child_with_base_fee(&parent, AlpenSpecId::V0, DECAYED_BASE_FEE);
        assert!(consensus
            .validate_header_against_parent(&unfloored, &parent)
            .is_ok());

        let floored = child_with_base_fee(&parent, AlpenSpecId::V0, BASE_FEE_FLOOR);
        assert_base_fee_diff(consensus.validate_header_against_parent(&floored, &parent));
    }

    /// V1 applies the floor, including on the first V1 block over a V0 parent.
    #[test]
    fn v1_applies_the_floor_from_its_first_block() {
        let consensus = test_consensus();

        for parent_version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            let parent = SealedHeader::seal_slow(Header {
                base_fee_per_gas: Some(DECAYED_BASE_FEE),
                ..valid_header(1, HeaderExtra::new(parent_version, 0).encode().into())
            });

            let floored = child_with_base_fee(&parent, AlpenSpecId::V1, BASE_FEE_FLOOR);
            assert!(
                consensus
                    .validate_header_against_parent(&floored, &parent)
                    .is_ok(),
                "{parent_version:?}"
            );

            let unfloored = child_with_base_fee(&parent, AlpenSpecId::V1, DECAYED_BASE_FEE);
            assert_base_fee_diff(consensus.validate_header_against_parent(&unfloored, &parent));
        }
    }

    #[test]
    fn zero_base_fee_floor_accepts_the_standard_eip1559_recurrence() {
        let evm_spec: EvmSpec =
            serde_json::from_str(r#"{"config":{"chainId":2892,"londonBlock":0,"shanghaiTime":0}}"#)
                .expect("genesis document parses");
        let consensus = AlpenConsensus::new(&evm_spec, &FeeSpec::new(SpecVersioned::new(0)));
        let version = AlpenSpecId::V0;
        let parent = SealedHeader::seal_slow(Header {
            number: 1,
            gas_limit: 30_000_000,
            gas_used: 0,
            base_fee_per_gas: Some(BASE_FEE_FLOOR),
            extra_data: HeaderExtra::new(version, 0).encode().into(),
            ..Default::default()
        });
        let child = SealedHeader::seal_slow(Header {
            number: 2,
            parent_hash: parent.hash(),
            gas_limit: 30_000_000,
            timestamp: 1,
            base_fee_per_gas: Some(875_000_000),
            extra_data: HeaderExtra::new(version, 0).encode().into(),
            ..Default::default()
        });

        let result = consensus.validate_header_against_parent(&child, &parent);
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn configured_floor_applies_on_the_london_activation_block() {
        let evm_spec: EvmSpec =
            serde_json::from_str(r#"{"config":{"chainId":2892,"londonBlock":2}}"#)
                .expect("genesis document parses");
        let base_fee_floor = INITIAL_BASE_FEE + 1;
        let consensus =
            AlpenConsensus::new(&evm_spec, &FeeSpec::new(SpecVersioned::new(base_fee_floor)));
        let extra_data: Bytes = HeaderExtra::new(AlpenSpecId::V0, 0).encode().into();
        let parent = SealedHeader::seal_slow(Header {
            number: 1,
            gas_limit: 30_000_000,
            extra_data: extra_data.clone(),
            ..Default::default()
        });
        let child = SealedHeader::seal_slow(Header {
            number: 2,
            parent_hash: parent.hash(),
            // EIP-1559 applies the elasticity multiplier to the parent when
            // validating the London activation block's gas-limit delta.
            gas_limit: 60_000_000,
            timestamp: 1,
            base_fee_per_gas: Some(base_fee_floor),
            extra_data,
            ..Default::default()
        });

        let result = consensus.validate_header_against_parent(&child, &parent);
        assert!(result.is_ok(), "{result:?}");
    }

    /// The genesis header's operator-authored `extra_data` is never parsed;
    /// block 0 validates under v0.
    #[test]
    fn genesis_header_is_exempt_from_the_layout() {
        let consensus = test_consensus();
        let genesis = SealedHeader::seal_slow(valid_header(0, Bytes::from_static(b"SC")));

        assert!(consensus.validate_header(&genesis).is_ok());
    }

    /// The deployed chain crosses into V1 from a tip its V0 binary built,
    /// with empty `extra_data` and the unfloored base fee deployed chains have
    /// today. V0 children keep validating behind that tip, and the first V1
    /// child validates against it.
    #[test]
    fn v1_child_validates_against_a_deployed_v0_tip() {
        let consensus = test_consensus();

        let v0_parent = SealedHeader::seal_slow(Header {
            base_fee_per_gas: Some(DECAYED_BASE_FEE),
            ..valid_header(100, Default::default())
        });
        assert!(consensus.validate_header(&v0_parent).is_ok());

        for (version, base_fee) in [
            (AlpenSpecId::V0, DECAYED_BASE_FEE),
            (AlpenSpecId::V1, BASE_FEE_FLOOR),
        ] {
            let child = SealedHeader::seal_slow(Header {
                parent_hash: v0_parent.hash(),
                timestamp: 1,
                base_fee_per_gas: Some(base_fee),
                ..valid_header(101, HeaderExtra::new(version, 0).encode().into())
            });
            assert!(consensus.validate_header(&child).is_ok(), "{version:?}");
            assert!(
                consensus
                    .validate_header_against_parent(&child, &v0_parent)
                    .is_ok(),
                "{version:?}"
            );
        }
    }
}
