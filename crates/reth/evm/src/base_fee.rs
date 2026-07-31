//! Base-fee floor for the Alpen fee model.
//!
//! Alpen keeps a standard EIP-1559 base fee (so it still responds to congestion) but clamps
//! it to a minimum floor: `base_fee_next = max(BASE_FEE_FLOOR, eip1559_next(parent))`. The
//! floor keeps the base fee from decaying toward zero when blocks are under target (the
//! normal L2 regime), so it always recovers amortized execution + proving + operational
//! cost, and the effective-gas conversion (`da_fee / base_fee`, see [`crate::da_fee`]) stays
//! well-defined.
//!
//! [`floored_next_base_fee`] is the single source of truth for the rule, shared by the block
//! builder (build side), the host consensus validation, and the proof guest, so all three
//! stay in lockstep.

use alloy_consensus::BlockHeader;
use alloy_eips::eip1559::{calc_next_block_base_fee, BaseFeeParams, INITIAL_BASE_FEE};
use reth_chainspec::{EthChainSpec, EthereumHardfork, EthereumHardforks};

/// Minimum base fee per gas (wei) — the floor under the EIP-1559 base fee. Set to 1 gwei.
///
/// TODO(fee-model, calibration): 1 gwei is an initial value; it should be governance-
/// calibrated so `BASE_FEE_FLOOR * expected_gas_per_block` recovers amortized execution +
/// proving + operational cost per block at target utilization. Changing it is a protocol
/// upgrade (consensus-critical: the block builder, host consensus validation, and proof
/// guest all read this constant and must agree).
pub const BASE_FEE_FLOOR: u64 = 1_000_000_000;

/// Clamps an already-computed EIP-1559 base fee to [`BASE_FEE_FLOOR`].
///
/// Kept as the single clamping primitive so the floor logic lives in one place; both
/// [`floored_next_base_fee`] and the block builder call it.
pub fn apply_base_fee_floor(base_fee: u64) -> u64 {
    base_fee.max(BASE_FEE_FLOOR)
}

/// Returns whether `base_fee` satisfies the protocol floor (`base_fee >= BASE_FEE_FLOOR`).
///
/// Used by the proof guest as the minimal base-fee check. (See the guest TODO: the full
/// check is the EIP-1559 recurrence capped at the floor, once the parent header is
/// available there.)
pub fn meets_base_fee_floor(base_fee: u64) -> bool {
    base_fee >= BASE_FEE_FLOOR
}

/// Computes the floored next-block base fee from the parent block's fields:
/// `max(BASE_FEE_FLOOR, eip1559_next(parent))`.
///
/// `parent_gas_used`, `parent_gas_limit`, and `parent_base_fee` are the parent header's
/// values; `base_fee_params` are the chain's EIP-1559 parameters. This is the canonical
/// base-fee rule — the block builder, host consensus, and guest all call it.
pub fn floored_next_base_fee(
    parent_gas_used: u64,
    parent_gas_limit: u64,
    parent_base_fee: u64,
    base_fee_params: BaseFeeParams,
) -> u64 {
    apply_base_fee_floor(calc_next_block_base_fee(
        parent_gas_used,
        parent_gas_limit,
        parent_base_fee,
        base_fee_params,
    ))
}

/// The protocol's expected base fee for `header`, given its `parent`: the floored EIP-1559
/// recurrence `max(BASE_FEE_FLOOR, next_block_base_fee(parent))`.
///
/// Returns `None` for pre-London blocks (no base fee is defined). This is the **single source
/// of truth** for the base-fee-against-parent rule: both the host consensus validator
/// ([`AlpenConsensus::validate_header_against_parent`](../../../alpen_reth_node/consensus))
/// and the proof guest (`evm-ee` block validation) call it, so host and guest can never
/// diverge (a mismatch would be a consensus split).
///
/// Mirrors reth's `validate_against_parent_eip1559_base_fee` exactly, except for the
/// [`apply_base_fee_floor`] clamp on the recurrence result. The chain-spec's
/// [`next_block_base_fee`](EthChainSpec::next_block_base_fee) supplies the EIP-1559 params,
/// so callers cannot desync on parameter selection.
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
    use alloy_eips::eip1559::{calc_next_block_base_fee, BaseFeeParams};

    use super::{
        apply_base_fee_floor, floored_next_base_fee, meets_base_fee_floor, BASE_FEE_FLOOR,
    };

    // Above the floor the clamp is a no-op: a parent at target keeps its (already-high) base
    // fee, and the floored result equals the pure EIP-1559 result.
    #[test]
    fn floor_is_noop_above_floor() {
        let params = BaseFeeParams::ethereum();
        let parent_gas_limit = 30_000_000;
        let parent_gas_used = parent_gas_limit / params.elasticity_multiplier as u64; // target
        let parent_base_fee = 10 * BASE_FEE_FLOOR; // well above the floor

        let eip1559 =
            calc_next_block_base_fee(parent_gas_used, parent_gas_limit, parent_base_fee, params);
        let floored =
            floored_next_base_fee(parent_gas_used, parent_gas_limit, parent_base_fee, params);

        assert!(eip1559 >= BASE_FEE_FLOOR);
        assert_eq!(floored, eip1559);
    }

    // When EIP-1559 would decay the base fee below the floor, the floor binds and holds it at
    // `BASE_FEE_FLOOR`.
    #[test]
    fn floor_binds_when_eip1559_decays_below_it() {
        let params = BaseFeeParams::ethereum();
        // An empty parent starting exactly at the floor decays below it under EIP-1559.
        let raw = calc_next_block_base_fee(0, 30_000_000, BASE_FEE_FLOOR, params);
        let floored = floored_next_base_fee(0, 30_000_000, BASE_FEE_FLOOR, params);

        assert!(
            raw < BASE_FEE_FLOOR,
            "expected EIP-1559 to decay below the floor"
        );
        assert_eq!(floored, BASE_FEE_FLOOR);
        assert_eq!(floored, apply_base_fee_floor(raw));
    }

    #[test]
    fn meets_floor_boundary() {
        assert!(!meets_base_fee_floor(BASE_FEE_FLOOR - 1));
        assert!(meets_base_fee_floor(BASE_FEE_FLOOR));
        assert!(meets_base_fee_floor(BASE_FEE_FLOOR + 1));
    }
}
