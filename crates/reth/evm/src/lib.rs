//! This crate holds commong evm changes shared between native and prover runtimes
//! and should not include any dependencies that cannot be run in the prover.
pub mod base_fee;
pub mod config;
pub mod consensus;
pub mod constants;
pub mod da_fee;
mod utils;

pub use utils::accumulate_logs_bloom;

pub mod evm;
mod evm2_settlement;
pub use evm2_settlement::AlpenEvmTypes;
pub mod precompiles;
