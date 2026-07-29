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

use alloy_eips::eip1559::{calc_next_block_base_fee, BaseFeeParams};

/// Minimum base fee per gas (wei) — the floor under the EIP-1559 base fee.
///
/// TODO(fee-model, calibration): set to a governance-calibrated value such that
/// `BASE_FEE_FLOOR * expected_gas_per_block` recovers amortized execution + proving +
/// operational cost per block at target utilization. It is currently `0`, which makes the
/// floor **inert** (`max(0, eip1559) == eip1559`, i.e. pure EIP-1559): the flooring
/// machinery is fully wired but has no effect until a non-zero value is set here. Changing
/// it is a protocol upgrade (consensus-critical: build, host validation, and guest all read
/// this constant and must agree).
pub const BASE_FEE_FLOOR: u64 = 0;

/// Clamps an already-computed EIP-1559 base fee to [`BASE_FEE_FLOOR`].
///
/// Kept as the single clamping primitive so the floor logic (and the placeholder-`0`
/// allowance) lives in one place; both [`floored_next_base_fee`] and the block builder call
/// it.
// `BASE_FEE_FLOOR` is a calibration placeholder (currently 0), so the `.max` is a no-op
// today; it is intentionally future-proof for when a non-zero floor is set. The `expect`
// fires a reminder to remove this attribute once the floor becomes non-zero.
#[expect(
    clippy::unnecessary_min_or_max,
    reason = "BASE_FEE_FLOOR is a placeholder (0) pending calibration; the clamp is intentional"
)]
pub fn apply_base_fee_floor(base_fee: u64) -> u64 {
    base_fee.max(BASE_FEE_FLOOR)
}

/// Returns whether `base_fee` satisfies the protocol floor (`base_fee >= BASE_FEE_FLOOR`).
///
/// Used by the proof guest as the minimal base-fee check. (See the guest TODO: the full
/// check is the EIP-1559 recurrence capped at the floor, once the parent header is
/// available there.)
// Inert at the placeholder floor (`base_fee >= 0` is always true); intentional and
// future-proof for a non-zero floor.
#[expect(
    clippy::absurd_extreme_comparisons,
    reason = "BASE_FEE_FLOOR is a placeholder (0) pending calibration; the check is intentional"
)]
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

#[cfg(test)]
mod tests {
    use alloy_eips::eip1559::{calc_next_block_base_fee, BaseFeeParams};

    use super::{apply_base_fee_floor, floored_next_base_fee};

    // Parent at target utilization keeps the base fee unchanged; with the (0) floor inert,
    // the floored result equals the pure EIP-1559 result.
    #[test]
    fn floor_matches_eip1559_when_inert() {
        let params = BaseFeeParams::ethereum();
        let parent_gas_limit = 30_000_000;
        let parent_gas_used = parent_gas_limit / params.elasticity_multiplier as u64; // target
        let parent_base_fee = 1_000_000_000;

        let eip1559 =
            calc_next_block_base_fee(parent_gas_used, parent_gas_limit, parent_base_fee, params);
        let floored =
            floored_next_base_fee(parent_gas_used, parent_gas_limit, parent_base_fee, params);

        // With the inert (0) floor, the floored value is identical to pure EIP-1559.
        assert_eq!(floored, eip1559);
    }

    // An empty parent block decays the base fee under EIP-1559; the floored value equals the
    // clamp applied to that decayed value (inert at the placeholder floor).
    #[test]
    fn floor_clamps_decayed_base_fee() {
        let params = BaseFeeParams::ethereum();
        let raw = calc_next_block_base_fee(0, 30_000_000, 1_000_000_000, params);
        let floored = floored_next_base_fee(0, 30_000_000, 1_000_000_000, params);
        assert_eq!(floored, apply_base_fee_floor(raw));
    }
}
