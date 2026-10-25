//! Primitives shared by the Alpen EVM, the proofs and the node.
//!
//! Holds the unit constants and conversions (BTC amounts and basis points),
//! the mapping between OL subjects and EVM addresses, and the bridge-out
//! withdrawal format. It depends on alloy and strata types only, not on reth,
//! so every part of the EE can link it, the proof guests included.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod bridge;
mod subject;
mod units;

pub use bridge::{
    extract_withdrawal_intents, WithdrawalCalldata, WithdrawalIntent, WithdrawalIntentEvent,
    WithdrawalIntentExtractionError, BRIDGEOUT_PRECOMPILE_ADDRESS,
};
pub use subject::{address_to_subject, subject_to_address, subject_to_address_unchecked};
pub use units::{
    sats_to_gwei, wei_to_sats, BPS_DENOMINATOR, GWEI_PER_SAT, WEI_PER_BTC, WEI_PER_SAT,
};
