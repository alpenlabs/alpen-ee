//! Produces the DA fee rate used by sequencer payload builds.
//!
//! A policy recommends a rate in wei per DA byte. The controller added in a
//! later commit applies [`RateAdjustment`] and publishes the result to the
//! payload builder without exposing the selected policy there.

use async_trait::async_trait;
use thiserror::Error;

const BASIS_POINTS_DENOMINATOR: u128 = 10_000;

/// A rate returned by a [`DaFeeRatePolicy`], before operator adjustment.
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
    #[expect(dead_code, reason = "the writer-backed policy is added in commit 2")]
    #[error("DA fee-rate source lookup failed")]
    Source(#[source] anyhow::Error),
    /// Converting a source-specific rate to wei per DA byte exceeded [`u64`].
    #[expect(dead_code, reason = "the writer-backed policy is added in commit 2")]
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
