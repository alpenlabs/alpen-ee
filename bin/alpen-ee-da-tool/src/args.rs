//! Command-line arguments for the EE DA reconstruction tool.

use std::{
    fs,
    path::{Path, PathBuf},
};

use alpen_ee_params::AlpenParams;
use clap::Parser;
use eyre::Context;
use strata_identifiers::L1Height;

/// Reconstructs Alpen EVM state from a reorg-safe Bitcoin block range.
#[derive(Parser)]
pub(crate) struct Args {
    /// Path to the JSON-serialized Alpen params artifact.
    #[arg(long, value_name = "PATH")]
    pub(crate) alpen_params: PathBuf,

    /// Path to the tool's TOML configuration file.
    #[arg(long, value_name = "PATH")]
    pub(crate) config: PathBuf,

    /// First Bitcoin block height to scan, inclusive.
    #[arg(long)]
    pub(crate) start_height: L1Height,

    /// Last Bitcoin block height to scan, inclusive.
    #[arg(long)]
    pub(crate) end_height: L1Height,

    /// Path to load and update the reconstruction and verification snapshot.
    #[arg(long, value_name = "PATH")]
    pub(crate) snapshot: Option<PathBuf>,
}

pub(crate) fn load_alpen_params(path: impl AsRef<Path>) -> eyre::Result<AlpenParams> {
    let path = path.as_ref();
    let bytes = fs::read(path)
        .wrap_err_with(|| format!("failed to read Alpen params from {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("failed to decode Alpen params from {}", path.display()))
}
