//! Sealing policy for predicate rotations.
//!
//! A block that consumes a predicate rotation has to be the last one in its
//! group. Everything after it would otherwise stay in an update proven against
//! the predicate the rotation retires, which is what the rotation exists to
//! prevent. That is a protocol invariant rather than a tunable threshold, so it
//! is expressed as [`SealingPolicy::must_seal`] and never as `would_exceed`.

use std::sync::Arc;

use alpen_ee_common::ExecBlockStorage;
use async_trait::async_trait;
use eyre::eyre;
use strata_acct_types::Hash;

use super::policy::{AccumulationPolicy, BlockDataProvider, SealingPolicy};

/// Whether a block consumes a predicate rotation.
#[derive(Debug, Clone, Copy, Default)]
pub struct RotationData {
    consumes_rotation: bool,
}

/// Whether any block added to the group consumed a predicate rotation.
///
/// Latches: nothing can un-require a seal once a rotation is in the group. A
/// group should never take a block after a rotation, but if one ever did, the
/// seal is still owed rather than quietly dropped.
#[derive(Debug, Default)]
pub struct RotationValue {
    closes_group: bool,
}

/// Tracks whether the group's last block consumed a predicate rotation.
#[derive(Debug)]
pub struct RotationPolicy;

impl AccumulationPolicy for RotationPolicy {
    type BlockData = RotationData;
    type AccumulatedValue = RotationValue;

    fn accumulate(value: &mut Self::AccumulatedValue, data: &Self::BlockData) {
        value.closes_group |= data.consumes_rotation;
    }
}

/// Seals a group as soon as it ends on a rotation-consuming block.
#[derive(Debug)]
pub struct SealOnRotation;

impl SealingPolicy<RotationPolicy> for SealOnRotation {
    fn would_exceed(&self, _value: &RotationValue, _block_data: &RotationData) -> bool {
        // A rotation never seals the group *before* its own block: the block
        // belongs to the group it closes.
        false
    }

    fn must_seal(&self, value: &RotationValue) -> bool {
        value.closes_group
    }
}

/// Reads a block's rotation status from its stored execution record.
#[derive(Debug)]
pub struct RotationDataProvider<ES> {
    block_storage: Arc<ES>,
}

impl<ES> RotationDataProvider<ES> {
    pub fn new(block_storage: Arc<ES>) -> Self {
        Self { block_storage }
    }
}

#[async_trait]
impl<ES: ExecBlockStorage> BlockDataProvider<RotationPolicy> for RotationDataProvider<ES> {
    async fn get_block_data(&self, hash: Hash) -> eyre::Result<Option<RotationData>> {
        let record = self
            .block_storage
            .get_exec_block(hash)
            .await?
            .ok_or_else(|| eyre!("missing exec block: {hash}"))?;

        Ok(Some(RotationData {
            consumes_rotation: record.package().outputs().new_predicate().is_some(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sealing_policy::Accumulator, test_utils::test_blocknumhash};

    fn data(consumes_rotation: bool) -> RotationData {
        RotationData { consumes_rotation }
    }

    #[test]
    fn seals_after_a_rotation_block() {
        let mut acc: Accumulator<RotationPolicy> = Accumulator::new();
        acc.add_block(test_blocknumhash(1), &data(true));

        assert!(acc.must_seal(&SealOnRotation));
        // The rotation block belongs to the group it closes, so it must never
        // seal ahead of itself.
        assert!(!acc.would_exceed(&SealOnRotation, &data(true)));
    }

    #[test]
    fn does_not_seal_after_an_ordinary_block() {
        let mut acc: Accumulator<RotationPolicy> = Accumulator::new();
        acc.add_block(test_blocknumhash(1), &data(false));

        assert!(!acc.must_seal(&SealOnRotation));
    }

    #[test]
    fn a_later_block_cannot_un_require_the_seal() {
        let mut acc: Accumulator<RotationPolicy> = Accumulator::new();
        acc.add_block(test_blocknumhash(1), &data(true));
        acc.add_block(test_blocknumhash(2), &data(false));

        // Nothing should admit a block after a rotation, but if it did, the
        // seal must still be owed rather than silently dropped.
        assert!(acc.must_seal(&SealOnRotation));
    }

    #[test]
    fn empty_accumulator_never_seals() {
        let acc: Accumulator<RotationPolicy> = Accumulator::new();
        assert!(!acc.must_seal(&SealOnRotation));
    }
}
