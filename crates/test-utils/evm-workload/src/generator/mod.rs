//! Builds a workload on a throwaway reth database.
//!
//! Blocks are built with the sequencer's EVM config and stamped V2. Each
//! block's witness, state changes and accessed-state record come from the same
//! functions the node uses, and the range witness comes from the production
//! extractor.

mod accounts;
mod chain;
mod contracts;
mod plan;
mod precompiles;
mod store;

use std::{fs, path::PathBuf};

use clap::{Parser, ValueEnum};
use eyre::Context;
pub use plan::{generate, Config, Summary};
use tokio::task;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::{workload_dir, MIXED_WORKLOAD, PRECOMPILES_WORKLOAD, WORKLOAD_FILE};

/// File the human-readable summary is stored in, next to the workload.
pub const SUMMARY_FILE: &str = "summary.json";

/// The checked-in workloads, each with its own settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Preset {
    /// A hundred blocks of ordinary traffic.
    Mixed,
    /// Ten blocks that each call every precompile.
    Precompiles,
}

impl Preset {
    /// The directory name under `crates/test-utils/data/workloads/`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mixed => MIXED_WORKLOAD,
            Self::Precompiles => PRECOMPILES_WORKLOAD,
        }
    }

    pub fn config(self) -> Config {
        match self {
            Self::Mixed => Config::default(),
            Self::Precompiles => Config::precompiles(),
        }
    }
}

/// Generates an EVM workload and stores it as a fixture.
#[derive(Debug, Parser)]
pub struct Args {
    /// Which checked-in workload to build.
    #[arg(long, value_enum, default_value_t = Preset::Mixed)]
    pub workload: Preset,

    /// Directory to store the workload in. Defaults to the checked-in
    /// workload's directory.
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Seeds the accounts and every random choice. Defaults to the
    /// workload's seed.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Blocks in the stored range. Defaults to the workload's count.
    #[arg(long)]
    pub blocks: Option<usize>,
}

/// Runs the generator binary.
pub async fn run(args: Args) -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let out = args
        .out
        .unwrap_or_else(|| workload_dir(args.workload.name()));
    let mut config = args.workload.config();
    if let Some(seed) = args.seed {
        config.seed = seed;
    }
    if let Some(blocks) = args.blocks {
        config.blocks = blocks;
    }

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
