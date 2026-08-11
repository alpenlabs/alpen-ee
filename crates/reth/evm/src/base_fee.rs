//! Base-fee floor for the Alpen fee model.
//!
//! Alpen keeps a standard EIP-1559 base fee (so it still responds to congestion) but clamps
//! it to a minimum floor: `base_fee_next = max(BASE_FEE_FLOOR, eip1559_next(parent))`. The
//! floor keeps the base fee from decaying toward zero when blocks are under target (the
//! normal L2 regime), so it always recovers amortized execution + proving + operational
//! cost, and the effective-gas conversion (`da_fee / base_fee`, see [`crate::da_fee`]) stays
//! well-defined.
//!
//! [`expected_floored_base_fee`] is the single source of truth for the against-parent rule,
//! called by both the host consensus validator and the proof guest so they stay in lockstep.
//! The block builder applies the same floor to its freshly computed base fee via
//! [`apply_base_fee_floor`].

use alloy_consensus::BlockHeader;
use alloy_eips::eip1559::INITIAL_BASE_FEE;
use reth_chainspec::{EthChainSpec, EthereumHardfork, EthereumHardforks};

/// Minimum base fee per gas (wei) — the floor under the EIP-1559 base fee. Set to 1 gwei.
///
/// TODO(fee-model, calibration): 1 gwei is an initial value; it should be governance-
/// calibrated so `BASE_FEE_FLOOR * expected_gas_per_block` recovers amortized execution +
/// proving + operational cost per block at target utilization. Changing it is a protocol
/// upgrade (consensus-critical: the block builder, host consensus validation, and proof
/// guest all read this constant and must agree). Tracked in a follow-up ticket.
pub const BASE_FEE_FLOOR: u64 = 1_000_000_000;

/// Clamps an already-computed EIP-1559 base fee to [`BASE_FEE_FLOOR`].
///
/// The single clamping primitive, so the floor logic lives in one place; both
/// [`expected_floored_base_fee`] and the block builder call it.
pub fn apply_base_fee_floor(base_fee: u64) -> u64 {
    base_fee.max(BASE_FEE_FLOOR)
}

/// The protocol's expected base fee for `header`, given its `parent`: the floored EIP-1559
/// recurrence `max(BASE_FEE_FLOOR, next_block_base_fee(parent))`.
///
/// Returns `None` for pre-London blocks (no base fee is defined). This is the single source of
/// truth for the base-fee-against-parent rule: both the host consensus validator
/// (`AlpenConsensus::validate_header_against_parent`) and the proof guest (`evm-ee` block
/// validation) call it, so host and guest can never diverge (a mismatch would be a consensus
/// split).
///
/// Mirrors reth's `validate_against_parent_eip1559_base_fee` exactly, except for the
/// [`apply_base_fee_floor`] clamp on the recurrence result. The chain-spec's
/// [`next_block_base_fee`](EthChainSpec::next_block_base_fee) supplies the EIP-1559 params, so
/// callers cannot desync on parameter selection.
pub fn expected_floored_base_fee<ChainSpec>(
    header: &ChainSpec::Header,
    parent: &ChainSpec::Header,
    chain_spec: &ChainSpec,
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
        .map(apply_base_fee_floor)
}

#[cfg(test)]
mod tests {
    use super::{apply_base_fee_floor, BASE_FEE_FLOOR};

    #[test]
    fn floor_clamps_below_and_passes_through_above() {
        // Below the floor clamps up to it.
        assert_eq!(apply_base_fee_floor(0), BASE_FEE_FLOOR);
        assert_eq!(apply_base_fee_floor(BASE_FEE_FLOOR - 1), BASE_FEE_FLOOR);
        // At the floor stays at the floor.
        assert_eq!(apply_base_fee_floor(BASE_FEE_FLOOR), BASE_FEE_FLOOR);
        // Above the floor passes through unchanged.
        assert_eq!(apply_base_fee_floor(BASE_FEE_FLOOR + 1), BASE_FEE_FLOOR + 1);
        assert_eq!(
            apply_base_fee_floor(10 * BASE_FEE_FLOOR),
            10 * BASE_FEE_FLOOR
        );
    }
}
