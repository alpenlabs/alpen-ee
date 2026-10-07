//! [`RethHeaderSummaryProvider`] — Reth-backed [`HeaderSummaryProvider`] for DA blobs.
//!
//! The DA blob pipeline needs an [`EvmHeaderSummary`] for each batch so that
//! verifiers can reconstruct EVM chain metadata (block number, timestamp,
//! base fee, gas used/limit, DA rate). [`RethHeaderSummaryProvider`]
//! satisfies the [`HeaderSummaryProvider`] trait by reading headers directly
//! from the Reth [`HeaderProvider`](reth_provider::HeaderProvider).
//!
//! This adapter lives in the binary crate because it depends on
//! `reth_provider::HeaderProvider`, which is only available where the Reth node
//! is assembled. The generic DA providers that consume it live in
//! `alpen-da-provider`.

use alpen_common::HeaderSummaryProvider;
use alpen_da_types::EvmHeaderSummary;
use alpen_params::{AlpenSpecId, HeaderExtra};

/// [`HeaderSummaryProvider`] backed by a Reth [`HeaderProvider`](reth_provider::HeaderProvider).
pub(crate) struct RethHeaderSummaryProvider<P> {
    provider: P,
}

impl<P> RethHeaderSummaryProvider<P> {
    pub(crate) fn new(provider: P) -> Self {
        Self { provider }
    }
}

impl<P> HeaderSummaryProvider for RethHeaderSummaryProvider<P>
where
    P: reth_provider::HeaderProvider<Header = reth_primitives_traits::Header> + Send + Sync,
{
    fn header_summary(
        &self,
        block_num: u64,
        spec_version: AlpenSpecId,
    ) -> eyre::Result<EvmHeaderSummary> {
        let header = self
            .provider
            .header_by_number(block_num)?
            .ok_or_else(|| eyre::eyre!("no header for block {block_num}"))?;
        summarize_header(&header, spec_version)
    }
}

/// Extracts the [`EvmHeaderSummary`] fields from a reth header governed by
/// `spec_version`, failing if the header is stamped with another version.
///
/// Split out from the trait impl so the reth → DA mapping can be unit-tested
/// without constructing a full [`reth_provider::HeaderProvider`].
fn summarize_header(
    header: &reth_primitives_traits::Header,
    spec_version: AlpenSpecId,
) -> eyre::Result<EvmHeaderSummary> {
    let header_extra = HeaderExtra::of_header(header)?;
    let header_version = header_extra.spec_version();
    eyre::ensure!(
        header_version == spec_version,
        "block {} is stamped {header_version:?}, but its batch is {spec_version:?}",
        header.number
    );
    Ok(EvmHeaderSummary {
        block_num: header.number,
        timestamp: header.timestamp,
        base_fee: header.base_fee_per_gas.ok_or_else(|| {
            eyre::eyre!(
                "block {} missing base_fee_per_gas; \
                 Alpen is post-London from genesis so this should always be present",
                header.number
            )
        })?,
        gas_used: header.gas_used,
        gas_limit: header.gas_limit,
        // V0 headers have no rate, and V0 summaries leave it out.
        da_rate: header_extra.da_rate().unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use reth_primitives_traits::Header;

    use super::*;

    fn stamped_header(spec_version: AlpenSpecId, da_rate: u64) -> Header {
        Header {
            number: 12345,
            timestamp: 1_700_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            gas_used: 15_000_000,
            gas_limit: 36_000_000,
            extra_data: HeaderExtra::new(spec_version, da_rate).encode().into(),
            ..Default::default()
        }
    }

    /// Header → EvmHeaderSummary mapping: verifies each field comes from the
    /// right source on the reth header.
    #[test]
    fn summarize_header_maps_fields_correctly() {
        let header = stamped_header(AlpenSpecId::V1, 2_500_000_000);
        let summary = summarize_header(&header, AlpenSpecId::V1).expect("mapping must succeed");

        assert_eq!(summary.block_num, 12345);
        assert_eq!(summary.timestamp, 1_700_000_000);
        assert_eq!(summary.base_fee, 1_000_000_000);
        assert_eq!(summary.gas_used, 15_000_000);
        assert_eq!(summary.gas_limit, 36_000_000);
        assert_eq!(summary.da_rate, 2_500_000_000);
    }

    #[test]
    fn summarize_header_drops_the_rate_under_v0() {
        let header = stamped_header(AlpenSpecId::V0, 2_500_000_000);
        let summary = summarize_header(&header, AlpenSpecId::V0).expect("mapping must succeed");

        assert_eq!(summary.da_rate, 0);
    }

    #[test]
    fn summarize_header_rejects_a_header_from_another_version() {
        let header = stamped_header(AlpenSpecId::V0, 2_500_000_000);
        let err = summarize_header(&header, AlpenSpecId::V1).expect_err("should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("V0") && msg.contains("V1") && msg.contains("block 12345"),
            "error must name both versions and the block, got: {msg}"
        );
    }

    #[test]
    fn summarize_header_errors_when_base_fee_missing() {
        let header = Header {
            number: 7,
            base_fee_per_gas: None,
            ..Default::default()
        };
        let err = summarize_header(&header, AlpenSpecId::V0).expect_err("should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("base_fee_per_gas") && msg.contains("block 7"),
            "error must identify missing field and block number, got: {msg}"
        );
    }
}
