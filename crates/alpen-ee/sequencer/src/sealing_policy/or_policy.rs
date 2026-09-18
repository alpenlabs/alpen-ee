//! Combinators for composing batch policies and sealing strategies.
//!
//! [`ComposedPolicy`] pairs two [`AccumulationPolicy`] types into one whose block data
//! and accumulated value are tuples of the inner types. Sealing combinators
//! like [`OrSealing`] define how the two halves are checked.
//! [`ComposedDataProvider`] fetches block data from both inner providers.
//!
//! The pair types nest, so any number of policies can be OR'd. [`crate::or_sealing!`] builds the
//! nested sealing policy and data provider from a list of pairs, and [`crate::compose_policy!`]
//! names the matching [`AccumulationPolicy`] type.

use std::marker::PhantomData;

use async_trait::async_trait;
use strata_acct_types::Hash;

use super::policy::{AccumulationPolicy, BlockDataProvider, SealingPolicy};

/// Composed batch policy that pairs two inner policies.
///
/// `BlockData` is `(A::BlockData, B::BlockData)` and `AccumulatedValue` is
/// `(A::AccumulatedValue, B::AccumulatedValue)`. Each half is accumulated
/// independently.
#[derive(Debug)]
pub struct ComposedPolicy<A: AccumulationPolicy, B: AccumulationPolicy>(PhantomData<(A, B)>);

impl<A: AccumulationPolicy, B: AccumulationPolicy> AccumulationPolicy for ComposedPolicy<A, B> {
    type BlockData = (A::BlockData, B::BlockData);
    type AccumulatedValue = (A::AccumulatedValue, B::AccumulatedValue);

    fn accumulate(value: &mut Self::AccumulatedValue, data: &Self::BlockData) {
        A::accumulate(&mut value.0, &data.0);
        B::accumulate(&mut value.1, &data.1);
    }
}

/// Seals a batch when **either** of two sealing policies triggers.
///
/// Both `SA` and `SB` operate on their respective projected half of the
/// composed accumulated value.
#[derive(Debug)]
pub struct OrSealing<A: AccumulationPolicy, B: AccumulationPolicy, SA, SB> {
    a: SA,
    b: SB,
    _marker: PhantomData<(A, B)>,
}

impl<A, B, SA, SB> OrSealing<A, B, SA, SB>
where
    A: AccumulationPolicy,
    B: AccumulationPolicy,
    SA: SealingPolicy<A>,
    SB: SealingPolicy<B>,
{
    /// Create a new OR-combined sealing policy.
    pub fn new(a: SA, b: SB) -> Self {
        Self {
            a,
            b,
            _marker: PhantomData,
        }
    }
}

impl<A, B, SA, SB> SealingPolicy<ComposedPolicy<A, B>> for OrSealing<A, B, SA, SB>
where
    A: AccumulationPolicy,
    B: AccumulationPolicy,
    SA: SealingPolicy<A>,
    SB: SealingPolicy<B>,
{
    fn would_exceed(
        &self,
        value: &(A::AccumulatedValue, B::AccumulatedValue),
        block_data: &(A::BlockData, B::BlockData),
    ) -> bool {
        self.a.would_exceed(&value.0, &block_data.0) || self.b.would_exceed(&value.1, &block_data.1)
    }

    fn must_seal(&self, value: &(A::AccumulatedValue, B::AccumulatedValue)) -> bool {
        self.a.must_seal(&value.0) || self.b.must_seal(&value.1)
    }
}

/// Composed data provider that fetches block data from two inner providers.
#[derive(Debug)]
pub struct ComposedDataProvider<A: AccumulationPolicy, B: AccumulationPolicy, DA, DB> {
    a: DA,
    b: DB,
    _marker: PhantomData<(A, B)>,
}

impl<A, B, DA, DB> ComposedDataProvider<A, B, DA, DB>
where
    A: AccumulationPolicy,
    B: AccumulationPolicy,
    DA: BlockDataProvider<A>,
    DB: BlockDataProvider<B>,
{
    /// Create a new composed data provider.
    pub fn new(a: DA, b: DB) -> Self {
        Self {
            a,
            b,
            _marker: PhantomData,
        }
    }
}

#[async_trait]
impl<A, B, DA, DB> BlockDataProvider<ComposedPolicy<A, B>> for ComposedDataProvider<A, B, DA, DB>
where
    A: AccumulationPolicy,
    B: AccumulationPolicy,
    DA: BlockDataProvider<A>,
    DB: BlockDataProvider<B>,
{
    async fn get_block_data(
        &self,
        hash: Hash,
    ) -> eyre::Result<Option<(A::BlockData, B::BlockData)>> {
        let (a, b) = tokio::try_join!(self.a.get_block_data(hash), self.b.get_block_data(hash))?;
        Ok(a.zip(b))
    }
}

/// Names the [`AccumulationPolicy`] that [`crate::or_sealing!`] builds for the same policies.
///
/// `compose_policy![A, B, C]` is `ComposedPolicy<A, ComposedPolicy<B, C>>`.
#[macro_export]
macro_rules! compose_policy {
    ($policy:ty $(,)?) => { $policy };
    ($policy:ty, $($rest:ty),+ $(,)?) => {
        $crate::sealing_policy::or_policy::ComposedPolicy<
            $policy,
            $crate::compose_policy!($($rest),+),
        >
    };
}

/// Builds an OR-combined sealing policy and data provider from `(sealing, provider)` pairs.
///
/// Returns `(sealing, provider)` where the group seals when **any** listed sealing policy
/// triggers. Pairs nest to the right, so the result implements
/// [`SealingPolicy`] / [`BlockDataProvider`] for the [`crate::compose_policy!`] of the same
/// policies in the same order.
///
/// ```ignore
/// type Policy = compose_policy![BlockCountPolicy, GasLimitPolicy, RotationPolicy];
/// let (sealing, provider) = or_sealing![
///     (FixedBlockCountSealing::new(10), BlockCountDataProvider),
///     (MaxGasSealing::new(gas_limit), gas_provider),
///     (SealOnRotation, RotationDataProvider::new(storage)),
/// ];
/// let acc: Accumulator<Policy> = Accumulator::new();
/// ```
#[macro_export]
macro_rules! or_sealing {
    (($sealing:expr, $provider:expr) $(,)?) => { ($sealing, $provider) };
    (($sealing:expr, $provider:expr), $($rest:tt)+) => {{
        let (rest_sealing, rest_provider) = $crate::or_sealing!($($rest)+);
        (
            $crate::sealing_policy::or_policy::OrSealing::new($sealing, rest_sealing),
            $crate::sealing_policy::or_policy::ComposedDataProvider::new($provider, rest_provider),
        )
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        sealing_policy::{
            block_count_policy::{
                BlockCountData, BlockCountDataProvider, BlockCountPolicy, FixedBlockCountSealing,
            },
            gas_limit_policy::{GasBlockData, GasLimitPolicy, MaxGasSealing},
            policy::Accumulator,
            rotation_policy::{RotationData, RotationPolicy, SealOnRotation},
        },
        test_utils::*,
    };

    type Combined = ComposedPolicy<BlockCountPolicy, GasLimitPolicy>;

    fn block_data(gas: u64) -> (BlockCountData, GasBlockData) {
        (BlockCountData, GasBlockData { gas_used: gas })
    }

    #[test]
    fn test_seals_on_block_count() {
        // max 2 blocks, high gas limit
        let sealing = OrSealing::new(FixedBlockCountSealing::new(2), MaxGasSealing::new(10_000));
        let mut acc: Accumulator<Combined> = Accumulator::new();

        // 1 accumulated + 1 incoming = 2 <= 2, no seal
        acc.add_block(test_blocknumhash(1), &block_data(10));
        assert!(!acc.would_exceed(&sealing, &block_data(10)));

        // 2 accumulated + 1 incoming = 3 > 2, seal on count
        acc.add_block(test_blocknumhash(2), &block_data(10));
        assert!(acc.would_exceed(&sealing, &block_data(10)));
    }

    #[test]
    fn test_seals_on_gas() {
        // high block count, max 50 gas
        let sealing = OrSealing::new(FixedBlockCountSealing::new(100), MaxGasSealing::new(50));
        let mut acc: Accumulator<Combined> = Accumulator::new();

        // 0 + 40 = 40 <= 50, no seal
        assert!(!acc.would_exceed(&sealing, &block_data(40)));

        // 40 accumulated + 20 incoming = 60 > 50, seal on gas
        acc.add_block(test_blocknumhash(1), &block_data(40));
        assert!(acc.would_exceed(&sealing, &block_data(20)));
    }

    #[test]
    fn test_neither_seals() {
        let sealing = OrSealing::new(FixedBlockCountSealing::new(100), MaxGasSealing::new(1000));
        let mut acc: Accumulator<Combined> = Accumulator::new();

        // 0 + 1 = 1 block (<< 100), 0 + 10 = 10 gas (<< 1000)
        acc.add_block(test_blocknumhash(1), &block_data(10));
        assert!(!acc.would_exceed(&sealing, &block_data(10)));
    }

    /// When gas limit is `u64::MAX` the gas policy never fires, so only
    /// block count matters. This mirrors the production path when
    /// `sequencer.chunk_sealing_gas_limit` is omitted from `--alpen-config`.
    #[test]
    fn test_gas_disabled_via_max() {
        let sealing = OrSealing::new(FixedBlockCountSealing::new(3), MaxGasSealing::new(u64::MAX));
        let mut acc: Accumulator<Combined> = Accumulator::new();

        // Accumulate huge gas — still shouldn't seal until block count exceeds 3
        acc.add_block(test_blocknumhash(1), &block_data(u64::MAX / 4));
        acc.add_block(test_blocknumhash(2), &block_data(u64::MAX / 4));
        // 2 + 1 = 3 <= 3, no seal
        assert!(!acc.would_exceed(&sealing, &block_data(0)));

        // 3 + 1 = 4 > 3, seal on count
        acc.add_block(test_blocknumhash(3), &block_data(0));
        assert!(acc.would_exceed(&sealing, &block_data(0)));
    }

    /// After draining the accumulator (sealing a batch), both halves of the
    /// composed value reset so the next batch starts fresh.
    #[test]
    fn test_drain_resets_both_values() {
        let sealing = OrSealing::new(FixedBlockCountSealing::new(2), MaxGasSealing::new(100));
        let mut acc: Accumulator<Combined> = Accumulator::new();

        acc.add_block(test_blocknumhash(1), &block_data(80));
        acc.add_block(test_blocknumhash(2), &block_data(80));

        // count: 2 + 1 = 3 > 2, gas: 160 + 10 = 170 > 100 — both would seal
        assert!(acc.would_exceed(&sealing, &block_data(10)));

        // Drain (seal the batch)
        let (inner, last) = acc.drain();
        assert_eq!(inner.len(), 1);
        assert_eq!(last, test_blocknumhash(2));

        // After drain, both counters are zero — neither policy seals
        assert!(!acc.would_exceed(&sealing, &block_data(0)));
        assert_eq!(acc.value().0.count, 0);
        assert_eq!(acc.value().1.total_gas, 0);
    }

    /// Reports a fixed gas figure for every block.
    struct FixedGasProvider(u64);

    #[async_trait]
    impl BlockDataProvider<GasLimitPolicy> for FixedGasProvider {
        async fn get_block_data(&self, _hash: Hash) -> eyre::Result<Option<GasBlockData>> {
            Ok(Some(GasBlockData { gas_used: self.0 }))
        }
    }

    /// Reports every block as consuming a rotation.
    struct AlwaysRotates;

    #[async_trait]
    impl BlockDataProvider<RotationPolicy> for AlwaysRotates {
        async fn get_block_data(&self, _hash: Hash) -> eyre::Result<Option<RotationData>> {
            Ok(Some(RotationData::new(true)))
        }
    }

    type Triple = compose_policy![BlockCountPolicy, GasLimitPolicy, RotationPolicy];

    fn triple_data(gas: u64, rotates: bool) -> <Triple as AccumulationPolicy>::BlockData {
        (
            BlockCountData,
            (GasBlockData { gas_used: gas }, RotationData::new(rotates)),
        )
    }

    #[tokio::test]
    async fn test_or_sealing_three_policies() {
        let (sealing, provider) = or_sealing![
            (FixedBlockCountSealing::new(100), BlockCountDataProvider),
            (MaxGasSealing::new(50), FixedGasProvider(7)),
            (SealOnRotation, AlwaysRotates),
        ];
        let mut acc: Accumulator<Triple> = Accumulator::new();

        // The middle policy (gas) seals: 40 accumulated + 20 incoming > 50.
        acc.add_block(test_blocknumhash(1), &triple_data(40, false));
        assert!(!acc.must_seal(&sealing));
        assert!(!acc.would_exceed(&sealing, &triple_data(5, false)));
        assert!(acc.would_exceed(&sealing, &triple_data(20, false)));

        // The provider fetches all three halves, nested like the policy type; the rotation
        // half it reports then forces a seal after the block.
        let data = provider
            .get_block_data(test_hash(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(data.1 .0.gas_used, 7);
        acc.add_block(test_blocknumhash(2), &data);
        assert!(acc.must_seal(&sealing));
    }

    #[test]
    fn test_or_sealing_single_pair_is_identity() {
        let (sealing, _provider) =
            or_sealing![(FixedBlockCountSealing::new(1), BlockCountDataProvider)];
        let mut acc: Accumulator<compose_policy![BlockCountPolicy]> = Accumulator::new();
        acc.add_block(test_blocknumhash(1), &BlockCountData);
        assert!(acc.would_exceed(&sealing, &BlockCountData));
    }
}
