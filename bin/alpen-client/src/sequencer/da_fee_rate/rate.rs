//! DA fee-rate values and operator adjustment.

use thiserror::Error;

const BASIS_POINTS_DENOMINATOR: u128 = 10_000;

/// A rate in wei per DA byte returned by a [`super::policy::DaFeeRatePolicy`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PolicyRate(u64);

impl PolicyRate {
    /// Creates a policy rate denominated in wei per DA byte.
    pub(super) const fn new(wei_per_byte: u64) -> Self {
        Self(wei_per_byte)
    }

    /// Returns the rate in wei per DA byte.
    pub(super) const fn wei_per_byte(self) -> u64 {
        self.0
    }
}

/// A policy rate after applying the operator multiplier and offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AdjustedRate(u64);

impl AdjustedRate {
    /// Returns the rate in wei per DA byte.
    pub(super) const fn wei_per_byte(self) -> u64 {
        self.0
    }
}

/// Applies the operator-configured multiplier and per-byte offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AffineAdjustment {
    multiplier_bps: u64,
    offset_wei_per_byte: u64,
}

impl AffineAdjustment {
    /// Creates an adjustment expressed in basis points and wei per DA byte.
    pub(super) const fn new(multiplier_bps: u64, offset_wei_per_byte: u64) -> Self {
        Self {
            multiplier_bps,
            offset_wei_per_byte,
        }
    }

    /// Applies the multiplier with upward rounding, then adds the offset.
    pub(super) fn apply(
        self,
        policy_rate: PolicyRate,
    ) -> Result<AdjustedRate, AffineAdjustmentError> {
        let raw_rate = policy_rate.wei_per_byte() as u128;
        let adjusted = (raw_rate * self.multiplier_bps as u128).div_ceil(BASIS_POINTS_DENOMINATOR)
            + self.offset_wei_per_byte as u128;
        adjusted
            .try_into()
            .map(AdjustedRate)
            .map_err(|_| AffineAdjustmentError::Overflow(adjusted))
    }
}

impl Default for AffineAdjustment {
    fn default() -> Self {
        Self::new(10_000, 0)
    }
}

/// Reports that an adjusted DA fee rate cannot be represented as [`u64`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(super) enum AffineAdjustmentError {
    #[error("adjusted DA fee rate exceeds u64: {0}")]
    Overflow(u128),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adjusted_rate(
        policy_rate: u64,
        multiplier_bps: u64,
        offset_wei_per_byte: u64,
    ) -> Result<u64, AffineAdjustmentError> {
        AffineAdjustment::new(multiplier_bps, offset_wei_per_byte)
            .apply(PolicyRate::new(policy_rate))
            .map(AdjustedRate::wei_per_byte)
    }

    #[test]
    fn default_adjustment_is_identity() {
        assert_eq!(
            AffineAdjustment::default()
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
            Err(AffineAdjustmentError::Overflow(_))
        ));
    }

    #[test]
    fn adjustment_rejects_offset_addition_that_exceeds_u64() {
        assert!(matches!(
            adjusted_rate(u64::MAX, 10_000, 1),
            Err(AffineAdjustmentError::Overflow(_))
        ));
    }
}
