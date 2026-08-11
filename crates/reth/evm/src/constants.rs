use revm::primitives::{address, Address};

/// The address for the Bridgeout precompile contract.
pub const BRIDGEOUT_PRECOMPILE_ADDRESS: Address =
    address!("5400000000000000000000000000000000000001");

/// Custom PrecompileId for the Bridgeout precompile contract.
pub const BRIDGEOUT_PRECOMPILE_ID: &str = "alpen-bridgeout-precompile";

/// The address for the Schnorr precompile contract.
pub const SCHNORR_PRECOMPILE_ADDRESS: Address =
    address!("5400000000000000000000000000000000000002");

/// Custom PrecompileId for the Schnorr precompile contract.
pub const SCHNORR_PRECOMPILE_PRECOMPILE_ID: &str = "alpen-schnorr-precompile";

/// The address of the DA fee vault predeploy.
///
/// Per-transaction Bitcoin data-availability fees are credited here as a direct balance
/// mutation. It is an ordinary genesis account rather than a precompile so that the
/// accrued fees live in normal, proven EVM state: the balance is part of the state root,
/// reconstructible from DA, and auditable (and later withdrawable) from genesis onward
/// without any special host support to read or move it.
pub const DA_FEE_VAULT_ADDRESS: Address = address!("5400000000000000000000000000000000000003");
