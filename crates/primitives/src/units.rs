use alloy_primitives::U256;

/// Number of wei per satoshi (1e10).
pub const WEI_PER_SAT: u64 = 10_000_000_000;

/// Converts satoshis to gwei for EVM compatibility.
///
/// In Alpen: 1 BTC = 10^8 sats = 10^9 gwei
/// Therefore: 1 sat = 10 gwei
///
/// Per EIP-4895, withdrawal amounts are stored in Gwei.
pub fn sats_to_gwei(sats: u64) -> Option<u64> {
    sats.checked_mul(10)
}

/// Converts wei to satoshis.
/// Returns a tuple of (satoshis, remainder_in_wei).
pub fn wei_to_sats(wei: U256) -> (U256, U256) {
    wei.div_rem(U256::from(WEI_PER_SAT))
}
