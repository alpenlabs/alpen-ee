//! Generates an EVM workload for the prover-perf guests.
//!
//! ```text
//! cargo run --release -p alpen-test-utils-evm-workload --features generator
//! ```
#![allow(
    unused_crate_dependencies,
    reason = "binary uses package dependencies through the library crate"
)]

use alpen_test_utils_evm_workload::generator::{run, Args};
use clap::Parser;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run(Args::parse()).await
}
