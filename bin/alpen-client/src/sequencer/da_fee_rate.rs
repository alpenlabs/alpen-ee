//! Produces the DA fee rate used by sequencer payload builds.
//!
//! A policy recommends a rate in wei per DA byte. The controller added in a
//! later commit applies [`RateAdjustment`] and publishes the result to the
//! payload builder without exposing the selected policy there.

use std::sync::Arc;

use alpen_reth_evm::WEI_PER_SAT;
use async_trait::async_trait;
use bitcoind_async_client::Client as BtcClient;
use strata_config::btcio::L1FeePolicyConfig;
use thiserror::Error;

use super::bitcoin_fee_rate::resolve_fee_rate;

const BASIS_POINTS_DENOMINATOR: u128 = 10_000;

/// A rate in wei per DA byte returned by a [`DaFeeRatePolicy`], before operator adjustment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PolicyRate(u64);

impl PolicyRate {
    /// Creates a policy rate denominated in wei per DA byte.
    pub(crate) const fn new(wei_per_byte: u64) -> Self {
        Self(wei_per_byte)
    }

    /// Returns the rate in wei per DA byte.
    pub(crate) const fn wei_per_byte(self) -> u64 {
        self.0
    }
}

/// A policy rate after applying the operator multiplier and offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdjustedRate(u64);

impl AdjustedRate {
    /// Returns the adjusted rate in wei per DA byte.
    pub(crate) const fn wei_per_byte(self) -> u64 {
        self.0
    }
}

/// Reports a failure to produce a usable policy rate.
#[derive(Debug, Error)]
pub(crate) enum DaFeeRatePolicyError {
    /// The selected external source did not return a rate.
    #[error("DA fee-rate source lookup failed")]
    Source(#[source] anyhow::Error),
    /// Converting a source-specific rate to wei per DA byte exceeded [`u64`].
    #[error("DA fee-rate conversion exceeds u64")]
    ConversionOverflow,
}

/// Supplies an unadjusted recommendation in wei per DA byte.
#[async_trait]
pub(crate) trait DaFeeRatePolicy: Send + Sync + 'static {
    /// Fetches the policy's current recommended rate.
    async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError>;
}

/// Applies the operator-configured multiplier and per-byte offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RateAdjustment {
    multiplier_bps: u64,
    offset_wei_per_byte: u64,
}

impl RateAdjustment {
    /// Creates an adjustment expressed in basis points and wei per DA byte.
    pub(crate) const fn new(multiplier_bps: u64, offset_wei_per_byte: u64) -> Self {
        Self {
            multiplier_bps,
            offset_wei_per_byte,
        }
    }

    /// Applies the multiplier with upward rounding, then adds the offset.
    pub(crate) fn apply(
        self,
        policy_rate: PolicyRate,
    ) -> Result<AdjustedRate, RateAdjustmentError> {
        let raw_rate = policy_rate.wei_per_byte() as u128;
        let adjusted = (raw_rate * self.multiplier_bps as u128).div_ceil(BASIS_POINTS_DENOMINATOR)
            + self.offset_wei_per_byte as u128;
        adjusted
            .try_into()
            .map(AdjustedRate)
            .map_err(|_| RateAdjustmentError::Overflow(adjusted))
    }
}

impl Default for RateAdjustment {
    fn default() -> Self {
        Self::new(10_000, 0)
    }
}

/// Reports that an adjusted DA fee rate cannot be represented as [`u64`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum RateAdjustmentError {
    #[error("adjusted DA fee rate exceeds u64: {0}")]
    Overflow(u128),
}

/// Returns one configured policy rate without consulting an external source.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FixedDaFeeRatePolicy {
    rate: PolicyRate,
}

impl FixedDaFeeRatePolicy {
    /// Creates a fixed policy denominated in wei per DA byte.
    pub(crate) const fn new(rate_wei_per_byte: u64) -> Self {
        Self {
            rate: PolicyRate::new(rate_wei_per_byte),
        }
    }
}

#[async_trait]
impl DaFeeRatePolicy for FixedDaFeeRatePolicy {
    async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError> {
        Ok(self.rate)
    }
}

/// Resolves the Bitcoin writer's fee policy and converts it into DA pricing units.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "constructed by sequencer configuration in a later checkpoint"
    )
)]
pub(crate) struct WriterBackedDaFeeRatePolicy {
    client: Arc<BtcClient>,
    fee_policy_config: L1FeePolicyConfig,
}

impl WriterBackedDaFeeRatePolicy {
    /// Creates a policy backed by the configured Bitcoin fee-rate source.
    pub(crate) fn new(client: Arc<BtcClient>, fee_policy_config: L1FeePolicyConfig) -> Self {
        Self {
            client,
            fee_policy_config,
        }
    }
}

#[async_trait]
impl DaFeeRatePolicy for WriterBackedDaFeeRatePolicy {
    async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError> {
        let fee_rate = resolve_fee_rate(self.client.as_ref(), &self.fee_policy_config)
            .await
            .map_err(DaFeeRatePolicyError::Source)?;
        let fee_kwu = fee_rate.to_sat_per_kwu();
        const WEIGHT_UNITS_PER_KWU: u64 = 1000;
        let rate_per_da =
            (fee_kwu as u128 * WEI_PER_SAT as u128).div_ceil(WEIGHT_UNITS_PER_KWU as u128);
        let rate_per_da =
            u64::try_from(rate_per_da).map_err(|_| DaFeeRatePolicyError::ConversionOverflow)?;
        Ok(PolicyRate(rate_per_da))
    }
}

#[cfg(test)]
mod tests {
    use bitcoind_async_client::{corepc_types::bitcoin::FeeRate, Auth};
    use strata_config::btcio::{FeePolicy, MempoolExplorerFeePolicy};

    use super::*;

    fn writer_policy(fee_policy: FeePolicy) -> WriterBackedDaFeeRatePolicy {
        let client = BtcClient::new(
            "http://127.0.0.1:1".to_string(),
            Auth::UserPass("test-user".to_string(), "test-password".to_string()),
            Some(1),
            Some(0),
            Some(1),
        )
        .expect("test Bitcoin client should be constructed");
        WriterBackedDaFeeRatePolicy::new(Arc::new(client), L1FeePolicyConfig::new(fee_policy))
    }

    fn adjusted_rate(
        policy_rate: u64,
        multiplier_bps: u64,
        offset_wei_per_byte: u64,
    ) -> Result<u64, RateAdjustmentError> {
        RateAdjustment::new(multiplier_bps, offset_wei_per_byte)
            .apply(PolicyRate::new(policy_rate))
            .map(AdjustedRate::wei_per_byte)
    }

    #[test]
    fn default_adjustment_is_identity() {
        assert_eq!(
            RateAdjustment::default()
                .apply(PolicyRate::new(42))
                .unwrap()
                .wei_per_byte(),
            42
        );
    }

    #[test]
    fn adjustment_rounds_multiplier_up_before_adding_offset() {
        assert_eq!(adjusted_rate(5, 15_000, 3), Ok(11));
        assert_eq!(adjusted_rate(1, 10_001, 0), Ok(2));
    }

    #[test]
    fn zero_multiplier_leaves_only_offset() {
        assert_eq!(adjusted_rate(u64::MAX, 0, 17), Ok(17));
    }

    #[test]
    fn adjustment_rejects_scaled_rate_that_exceeds_u64() {
        assert!(matches!(
            adjusted_rate(u64::MAX, 10_001, 0),
            Err(RateAdjustmentError::Overflow(_))
        ));
    }

    #[test]
    fn adjustment_rejects_offset_addition_that_exceeds_u64() {
        assert!(matches!(
            adjusted_rate(u64::MAX, 10_000, 1),
            Err(RateAdjustmentError::Overflow(_))
        ));
    }

    #[tokio::test]
    async fn fixed_policy_returns_its_configured_rate() {
        let policy = FixedDaFeeRatePolicy::new(73);

        assert_eq!(policy.fetch_rate().await.unwrap().wei_per_byte(), 73);
    }

    #[test]
    fn policy_trait_is_object_safe() {
        fn accept_policy(_: &dyn DaFeeRatePolicy) {}

        accept_policy(&FixedDaFeeRatePolicy::new(1));
    }

    #[tokio::test]
    async fn writer_policy_converts_sat_per_kwu_to_wei_per_da_byte() {
        for (sat_per_kwu, expected_wei_per_byte) in [
            (1, 10_000_000),
            (125, 1_250_000_000),
            (1_000, 10_000_000_000),
        ] {
            let policy = writer_policy(FeePolicy::Fixed {
                fee_rate: FeeRate::from_sat_per_kwu(sat_per_kwu),
            });

            assert_eq!(
                policy.fetch_rate().await.unwrap().wei_per_byte(),
                expected_wei_per_byte
            );
        }
    }

    #[tokio::test]
    async fn writer_policy_maps_fee_source_failures() {
        let policy = writer_policy(FeePolicy::MempoolExplorer {
            policy: MempoolExplorerFeePolicy::Fastest,
            mempool_base_url: "not a url".to_string(),
            fallback_conf_target: 1,
        });

        assert!(matches!(
            policy.fetch_rate().await,
            Err(DaFeeRatePolicyError::Source(_))
        ));
    }

    #[tokio::test]
    async fn writer_policy_rejects_conversion_overflow() {
        let policy = writer_policy(FeePolicy::Fixed {
            fee_rate: FeeRate::from_sat_per_kwu(u64::MAX),
        });

        assert!(matches!(
            policy.fetch_rate().await,
            Err(DaFeeRatePolicyError::ConversionOverflow)
        ));
    }
}
