//! Sums a per-block `u64` and seals once the sum would exceed a limit.
//!
//! One policy covers every "at most N of something per group" rule: block
//! count (each block reports `1`), gas used, or any other additive figure.
//! Which quantity is summed is decided by the [`super::BlockDataProvider`]
//! paired with it, not by the policy type.

use super::policy::{AccumulationPolicy, SealingPolicy};

/// Accumulates a per-block `u64` into a running, saturating sum.
#[derive(Debug, Clone, Copy)]
pub struct ValueAccumulatorPolicy;

impl AccumulationPolicy for ValueAccumulatorPolicy {
    type BlockData = u64;
    type AccumulatedValue = u64;

    fn accumulate(accumulated: &mut u64, new: &u64) {
        *accumulated = accumulated.saturating_add(*new);
    }
}

/// Seals when the accumulated sum plus the incoming block's value would exceed `max_limit`.
///
/// A sum of exactly `max_limit` is admitted. `u64::MAX` never seals, which is how a
/// limit is switched off without changing the policy type.
#[derive(Debug, Clone, Copy)]
pub struct MaxValueSealing {
    max_limit: u64,
}

impl MaxValueSealing {
    /// Creates a sealing policy that admits sums up to and including `max_limit`.
    pub fn new(max_limit: u64) -> Self {
        Self { max_limit }
    }

    /// The largest sum a group may reach.
    pub fn max_limit(&self) -> u64 {
        self.max_limit
    }
}

impl SealingPolicy<ValueAccumulatorPolicy> for MaxValueSealing {
    fn would_exceed(&self, accumulated: &u64, new: &u64) -> bool {
        accumulated.saturating_add(*new) > self.max_limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sealing_policy::Accumulator, test_utils::*};

    #[test]
    fn accumulates_sum() {
        let mut acc: Accumulator<ValueAccumulatorPolicy> = Accumulator::new();

        acc.add_block(test_blocknumhash(1), &100);
        acc.add_block(test_blocknumhash(2), &200);

        assert_eq!(*acc.value(), 300);
    }

    #[test]
    fn accumulate_saturates_instead_of_overflowing() {
        let mut acc: Accumulator<ValueAccumulatorPolicy> = Accumulator::new();

        acc.add_block(test_blocknumhash(1), &u64::MAX);
        acc.add_block(test_blocknumhash(2), &1);

        assert_eq!(*acc.value(), u64::MAX);
    }

    #[test]
    fn empty_accumulator_never_exceeds_a_nonzero_limit() {
        let sealing = MaxValueSealing::new(3);
        let acc: Accumulator<ValueAccumulatorPolicy> = Accumulator::new();

        assert!(!acc.would_exceed(&sealing, &1));
        assert!(!acc.would_exceed(&sealing, &3));
    }

    #[test]
    fn exact_limit_is_admitted_and_one_past_seals() {
        let sealing = MaxValueSealing::new(1000);
        let mut acc: Accumulator<ValueAccumulatorPolicy> = Accumulator::new();
        acc.add_block(test_blocknumhash(1), &500);

        // 500 + 500 = 1000 <= 1000
        assert!(!acc.would_exceed(&sealing, &500));
        // 500 + 501 = 1001 > 1000
        assert!(acc.would_exceed(&sealing, &501));
    }

    #[test]
    fn counts_blocks_when_each_reports_one() {
        let sealing = MaxValueSealing::new(3);
        let mut acc: Accumulator<ValueAccumulatorPolicy> = Accumulator::new();

        acc.add_block(test_blocknumhash(1), &1);
        acc.add_block(test_blocknumhash(2), &1);
        // 2 + 1 = 3 <= 3, a group of exactly max_limit blocks is allowed
        assert!(!acc.would_exceed(&sealing, &1));

        acc.add_block(test_blocknumhash(3), &1);
        // 3 + 1 = 4 > 3, seal before the fourth block
        assert!(acc.would_exceed(&sealing, &1));
    }

    #[test]
    fn single_block_over_limit_reports_exceed_on_empty_accumulator() {
        // The caller's `!is_empty()` guard is what stops an empty group from
        // sealing; the check itself is honest about the overshoot.
        let sealing = MaxValueSealing::new(1000);
        let acc: Accumulator<ValueAccumulatorPolicy> = Accumulator::new();

        assert!(acc.would_exceed(&sealing, &1500));
    }

    #[test]
    fn max_limit_of_u64_max_never_seals() {
        let sealing = MaxValueSealing::new(u64::MAX);
        let mut acc: Accumulator<ValueAccumulatorPolicy> = Accumulator::new();
        acc.add_block(test_blocknumhash(1), &u64::MAX);

        // Saturates at u64::MAX, which is not greater than the limit.
        assert!(!acc.would_exceed(&sealing, &u64::MAX));
    }

    #[test]
    fn resets_after_drain() {
        let mut acc: Accumulator<ValueAccumulatorPolicy> = Accumulator::new();
        acc.add_block(test_blocknumhash(1), &999);

        acc.drain();

        assert_eq!(*acc.value(), 0);
    }

    #[test]
    fn max_limit_getter() {
        assert_eq!(MaxValueSealing::new(100).max_limit(), 100);
    }
}
