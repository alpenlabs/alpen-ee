//! Generated EVM workloads for measuring the EE proofs.
//!
//! A workload is a run of consecutive V1 blocks with real signed transactions,
//! stored as the host-side data the provers build their inputs from: the blocks
//! and their deposits, the chunk prover's pre-state, and the account prover's
//! range witness and batch state diff. Proof inputs are rebuilt from these by
//! the consumer, so a change to a proof statement changes code, not fixtures.
//!
//! The `generator` feature adds the generator that builds a workload on a
//! throwaway reth database. See `crates/test-utils/data/README.md` for the
//! checked-in workloads and how to regenerate them.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[cfg(feature = "generator")]
pub mod generator;

/// Version of the [`Workload`] encoding. Bump it on any layout change, so an
/// old file fails to load with a clear message instead of decoding garbage.
pub const FORMAT_VERSION: u32 = 1;

/// File a workload is stored in, inside its directory.
pub const WORKLOAD_FILE: &str = "workload.bin";

/// Name of the checked-in workload the prover-perf guests run on.
pub const MIXED_WORKLOAD: &str = "mixed";

/// Returns the directory of the checked-in workload called `name`.
pub fn workload_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../data/workloads")
        .join(name)
}

/// A run of consecutive blocks and the data the provers need to prove them
/// as one chunk and one batch.
#[derive(Debug, Serialize, Deserialize)]
pub struct Workload {
    format_version: u32,

    /// RLP-encoded header of the block the range builds on.
    pub prev_header_rlp: Vec<u8>,

    /// The blocks in order.
    pub blocks: Vec<WorkloadBlock>,

    /// Codec-encoded `EvmPartialState` at the start of the range, built the
    /// way the chunk prover builds it: from the union of the blocks' witness
    /// records.
    pub chunk_pre_state: Vec<u8>,

    /// Codec-encoded `EvmPartialState` over the range, as the account prover's
    /// range witness extractor builds it for the DA check.
    pub range_pre_state: Vec<u8>,

    /// Codec-encoded `BatchStateDiff` of the range, aggregated from the
    /// blocks' state changes the way the DA blob provider does it, before it
    /// drops already published bytecodes.
    pub state_diff: Vec<u8>,

    /// Code hashes deployed before the range. Earlier batches published
    /// these, so the batch's DA blob leaves them out.
    pub published_code_hashes: Vec<[u8; 32]>,
}

impl Workload {
    pub fn new(
        prev_header_rlp: Vec<u8>,
        blocks: Vec<WorkloadBlock>,
        chunk_pre_state: Vec<u8>,
        range_pre_state: Vec<u8>,
        state_diff: Vec<u8>,
        published_code_hashes: Vec<[u8; 32]>,
    ) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            prev_header_rlp,
            blocks,
            chunk_pre_state,
            range_pre_state,
            state_diff,
            published_code_hashes,
        }
    }

    /// Loads the workload stored in `dir`.
    pub fn load(dir: &Path) -> io::Result<Self> {
        let path = dir.join(WORKLOAD_FILE);
        let bytes = fs::read(&path)?;
        let workload: Self = bincode::deserialize(&bytes).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("decode {}: {e}", path.display()),
            )
        })?;
        if workload.format_version != FORMAT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} has format version {}, expected {FORMAT_VERSION}; regenerate it",
                    path.display(),
                    workload.format_version
                ),
            ));
        }
        Ok(workload)
    }

    /// Stores the workload in `dir`, creating the directory if needed.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let bytes = bincode::serialize(self).map_err(io::Error::other)?;
        fs::write(dir.join(WORKLOAD_FILE), bytes)
    }
}

/// One block of a workload.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkloadBlock {
    /// RLP-encoded block.
    pub block_rlp: Vec<u8>,

    /// Deposits the block mints through its EIP-4895 list, in list order.
    pub deposits: Vec<Deposit>,
}

/// A deposit minted into the EVM.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deposit {
    /// Destination subject: 12 zero bytes, then the EVM address.
    pub dest_subject: [u8; 32],

    /// Amount in satoshis.
    pub sats: u64,
}
