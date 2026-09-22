//! Base-fee floor for the Alpen fee model.
//!
//! Alpen keeps a standard EIP-1559 base fee (so it still responds to congestion) but clamps
//! it to a minimum floor: `base_fee_next = max(base_fee_floor, eip1559_next(parent))`. The
//! floor keeps the base fee from decaying toward zero when blocks are under target (the
//! normal L2 regime), so it always recovers amortized execution + proving + operational
//! cost, and the effective-gas conversion (`da_fee / base_fee`, see [`crate::da_fee`]) stays
//! well-defined.
//!
//! [`next_floored_base_fee`] is the single source of truth for both payload construction and
//! against-parent validation, including the London activation block.
//!
//! # Activation
//!
//! The floor applies to every post-London block; there is no separate fee-model activation
//! height. Alpen runs with the floor in force from genesis, so no canonical pre-floor
//! history exists for it to invalidate. This is deliberate for the initial deployment. If a
//! network ever needs blocks that predate the floor to stay valid under a clean sync, this
//! rule must be gated behind a fee-model activation height/timestamp in both the payload builder
//! and host validator.

use alloy_consensus::BlockHeader;
use alloy_eips::eip1559::INITIAL_BASE_FEE;
use alpen_ee_params::DEFAULT_BASE_FEE_FLOOR;
use reth_chainspec::{EthChainSpec, EthereumHardfork, EthereumHardforks};

/// Production default for the minimum base fee per gas, in wei.
///
/// Kept as a re-export for callers that need the production value. Nodes use the value from
/// their Alpen params artifact instead.
pub const BASE_FEE_FLOOR: u64 = DEFAULT_BASE_FEE_FLOOR;

/// Clamps an already-computed EIP-1559 base fee to `base_fee_floor`.
///
/// The single clamping primitive used by [`next_floored_base_fee`].
pub fn apply_base_fee_floor(base_fee: u64, base_fee_floor: u64) -> u64 {
    base_fee.max(base_fee_floor)
}

/// The protocol's base fee for the next block built on `parent`.
///
/// Returns `None` for pre-London blocks (no base fee is defined). This is the single source of
/// truth shared by the payload builder and host against-parent validator.
///
/// Mirrors reth's `validate_against_parent_eip1559_base_fee` exactly, except for the
/// [`apply_base_fee_floor`] clamp on the recurrence result. The chain-spec's
/// [`next_block_base_fee`](EthChainSpec::next_block_base_fee) supplies the EIP-1559 params, so
/// callers cannot desync on parameter selection.
pub fn next_floored_base_fee<ChainSpec>(
    parent: &ChainSpec::Header,
    chain_spec: &ChainSpec,
    next_number: u64,
    next_timestamp: u64,
    base_fee_floor: u64,
) -> Option<u64>
where
    ChainSpec: EthChainSpec + EthereumHardforks,
{
    // Pre-London blocks have no base fee.
    if !chain_spec.is_london_active_at_block(next_number) {
        return None;
    }
    // The London-activation block itself uses the fixed initial base fee (no parent recurrence).
    if chain_spec
        .ethereum_fork_activation(EthereumHardfork::London)
        .transitions_at_block(next_number)
    {
        return Some(apply_base_fee_floor(INITIAL_BASE_FEE, base_fee_floor));
    }
    // Otherwise: the floored EIP-1559 recurrence from the parent.
    chain_spec
        .next_block_base_fee(parent, next_timestamp)
        .map(|base_fee| apply_base_fee_floor(base_fee, base_fee_floor))
}

/// The expected fee for `header` as checked against `parent` by host consensus.
pub fn expected_floored_base_fee<ChainSpec>(
    header: &ChainSpec::Header,
    parent: &ChainSpec::Header,
    chain_spec: &ChainSpec,
    base_fee_floor: u64,
) -> Option<u64>
where
    ChainSpec: EthChainSpec + EthereumHardforks,
{
    next_floored_base_fee(
        parent,
        chain_spec,
        header.number(),
        header.timestamp(),
        base_fee_floor,
    )
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_eips::eip1559::INITIAL_BASE_FEE;
    use alpen_ee_params::{AlpenSpecId, EvmSpec};

    use super::{apply_base_fee_floor, next_floored_base_fee, BASE_FEE_FLOOR};

    #[test]
    fn floor_clamps_below_and_passes_through_above() {
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

    #[test]
    fn zero_floor_preserves_zero_base_fee() {
        assert_eq!(apply_base_fee_floor(0, 0), 0);
    }

    #[test]
    fn delayed_london_activation_uses_initial_fee_with_zero_floor() {
        let evm_spec: EvmSpec =
            serde_json::from_str(r#"{"config":{"chainId":2892,"londonBlock":2}}"#)
                .expect("genesis document parses");
        let chain_spec = evm_spec.chain_spec(AlpenSpecId::V0);
        let parent = Header {
            number: 1,
            gas_limit: 30_000_000,
            ..Default::default()
        };

        assert_eq!(
            next_floored_base_fee(&parent, chain_spec.as_ref(), 1, 0, 0),
            None
        );
        assert_eq!(
            next_floored_base_fee(&parent, chain_spec.as_ref(), 2, 1, 0),
            Some(INITIAL_BASE_FEE)
        );
        assert_eq!(
            next_floored_base_fee(&parent, chain_spec.as_ref(), 2, 1, INITIAL_BASE_FEE + 1),
            Some(INITIAL_BASE_FEE + 1)
        );
    }
}
