//! Base-fee floor for the Alpen fee model.
//!
//! Alpen keeps a standard EIP-1559 base fee (so it still responds to congestion) but clamps
//! it to a configured minimum floor: `base_fee_next = max(floor, eip1559_next(parent))`. The
//! A positive floor keeps the base fee from decaying toward zero when blocks are under target
//! (the normal L2 regime), so it recovers amortized execution + proving + operational cost and
//! keeps effective-gas fee quoting stable.
//!
//! [`expected_floored_base_fee`] is the single source of truth for the host
//! against-parent rule. The block builder applies the same configured floor
//! to its freshly computed base fee via [`apply_base_fee_floor`].
//!
//! # Activation
//!
//! The floor applies to every post-London block; there is no separate fee-model activation
//! height. Alpen runs with the floor in force from genesis, so no canonical pre-floor
//! history exists for it to invalidate. This is deliberate for the initial deployment. If a
//! network ever needs blocks that predate the floor to stay valid under a clean sync or
//! historical proof, this rule must be gated behind a fee-model activation height/timestamp
//! — retaining the standard recurrence before it — in both the block builder and host
//! validator.

use alloy_consensus::BlockHeader;
use alloy_eips::eip1559::INITIAL_BASE_FEE;
use reth_chainspec::{EthChainSpec, EthereumHardfork, EthereumHardforks};

/// Clamps an already-computed EIP-1559 base fee to `base_fee_floor`.
///
/// The single clamping primitive, so the floor logic lives in one place; both
/// [`expected_floored_base_fee`] and the block builder call it.
pub fn apply_base_fee_floor(base_fee: u64, base_fee_floor: u64) -> u64 {
    base_fee.max(base_fee_floor)
}

/// The protocol's expected base fee for `header`, given its `parent`: the floored EIP-1559
/// recurrence `max(base_fee_floor, next_block_base_fee(parent))`.
///
/// Returns `None` for pre-London blocks (no base fee is defined). This is the host consensus
/// validator's single source of truth for the base-fee-against-parent rule; the payload builder
/// applies the same floor through [`apply_base_fee_floor`].
///
/// Mirrors reth's `validate_against_parent_eip1559_base_fee` exactly, except for the
/// [`apply_base_fee_floor`] clamp on the recurrence result. The chain-spec's
/// [`next_block_base_fee`](EthChainSpec::next_block_base_fee) supplies the EIP-1559 params, so
/// callers cannot desync on parameter selection.
pub fn expected_floored_base_fee<ChainSpec>(
    header: &ChainSpec::Header,
    parent: &ChainSpec::Header,
    chain_spec: &ChainSpec,
    base_fee_floor: u64,
) -> Option<u64>
where
    ChainSpec: EthChainSpec + EthereumHardforks,
{
    // Pre-London blocks have no base fee.
    if !chain_spec.is_london_active_at_block(header.number()) {
        return None;
    }
    // The London-activation block itself uses the fixed initial base fee (no parent recurrence).
    if chain_spec
        .ethereum_fork_activation(EthereumHardfork::London)
        .transitions_at_block(header.number())
    {
        return Some(INITIAL_BASE_FEE);
    }
    // Otherwise: the floored EIP-1559 recurrence from the parent.
    chain_spec
        .next_block_base_fee(parent, header.timestamp())
        .map(|base_fee| apply_base_fee_floor(base_fee, base_fee_floor))
}

#[cfg(test)]
mod tests {
    use super::apply_base_fee_floor;

    #[test]
    fn floor_clamps_below_and_passes_through_above() {
        const BASE_FEE_FLOOR: u64 = 1_000_000_000;

        // Below the floor clamps up to it.
        assert_eq!(apply_base_fee_floor(0, BASE_FEE_FLOOR), BASE_FEE_FLOOR);
        assert_eq!(
            apply_base_fee_floor(BASE_FEE_FLOOR - 1, BASE_FEE_FLOOR),
            BASE_FEE_FLOOR
        );
        // At the floor stays at the floor.
        assert_eq!(
            apply_base_fee_floor(BASE_FEE_FLOOR, BASE_FEE_FLOOR),
            BASE_FEE_FLOOR
        );
        // Above the floor passes through unchanged.
        assert_eq!(
            apply_base_fee_floor(BASE_FEE_FLOOR + 1, BASE_FEE_FLOOR),
            BASE_FEE_FLOOR + 1
        );
        assert_eq!(
            apply_base_fee_floor(10 * BASE_FEE_FLOOR, BASE_FEE_FLOOR),
            10 * BASE_FEE_FLOOR
        );
    }
}
