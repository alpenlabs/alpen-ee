//! Builds a workload on a throwaway reth database.
//!
//! Blocks are built with the sequencer's EVM config and stamped V1. Each
//! block's witness, state changes and accessed-state record come from the same
//! functions the node uses, and the range witness comes from the production
//! extractor.

mod accounts;
mod chain;
mod contracts;
mod plan;
mod store;

use std::{fs, path::PathBuf};

use clap::Parser;
use eyre::Context;
pub use plan::{generate, Config, Summary};
use tokio::task;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::{workload_dir, MIXED_WORKLOAD, WORKLOAD_FILE};

/// File the human-readable summary is stored in, next to the workload.
pub const SUMMARY_FILE: &str = "summary.json";

/// Generates an EVM workload and stores it as a fixture.
#[derive(Debug, Parser)]
pub struct Args {
    /// Directory to store the workload in. Defaults to the checked-in
    /// workload the prover-perf guests run on.
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Seeds the accounts and every random choice.
    #[arg(long, default_value_t = Config::default().seed)]
    pub seed: u64,

    /// Blocks in the stored range.
    #[arg(long, default_value_t = Config::default().blocks)]
    pub blocks: usize,
}

/// Runs the generator binary.
pub async fn run(args: Args) -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let out = args.out.unwrap_or_else(|| workload_dir(MIXED_WORKLOAD));
    let config = Config {
        seed: args.seed,
        blocks: args.blocks,
        ..Config::default()
    };

    // The range witness extractor blocks on async store reads, which only
    // works off the runtime's worker threads.
    let (workload, summary) = task::spawn_blocking(move || generate(&config)).await??;

    workload
        .save(&out)
        .with_context(|| format!("store workload in {}", out.display()))?;
    let summary_json = serde_json::to_string_pretty(&summary)? + "\n";
    fs::write(out.join(SUMMARY_FILE), summary_json)?;

    let size = fs::metadata(out.join(WORKLOAD_FILE))?.len();
    info!(out = %out.display(), size, "stored workload");
    Ok(())
}
