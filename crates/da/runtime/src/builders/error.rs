//! Error raised while building a DA witness on the host.

use crate::DaBlobRecoveryError;

/// Error raised while building a [`DaWitness`](alpen_da_types::DaWitness).
///
/// Spans both layers of the build — L1 inclusion (block fetch, wtxid roots,
/// blob reassembly) and EVM dedup resolution (code holder lookups) —
/// and carries enough categorization for the prover to map onto its own
/// retry semantics: provider/store read failures vs. genuinely missing data
/// vs. structurally inconsistent inputs.
///
/// Message convention matches the verifier's `DaVerificationError`: parameters
/// go in parens, never after a colon.
#[derive(Debug, thiserror::Error)]
pub enum DaWitnessBuildError {
    /// A non-genesis batch referenced no DA blocks.
    #[error("non-genesis batch has no DA refs")]
    EmptyDaRefs,
    /// Reading an L1 block from the Bitcoin backend failed.
    #[error("L1 block fetch failed for {block} ({error})")]
    GetBlock { block: String, error: String },
    /// A referenced L1 block carried no transactions.
    #[error("L1 block {0} has no transactions")]
    BlockHasNoTransactions(String),
    /// A fetched L1 block's wtxids root disagreed with its DA ref.
    #[error("L1 block {block} wtxids_root mismatch (DA ref has {expected}, fetched block has {computed})")]
    WtxidsRootMismatch {
        block: String,
        expected: String,
        computed: String,
    },
    /// A DA transaction named by a DA ref was absent from its L1 block.
    #[error("DA tx {txid}/{wtxid} not found in L1 block {block}")]
    DaTxNotFound {
        txid: String,
        wtxid: String,
        block: String,
    },
    /// Recovering the EE DA blob from its L1 transactions failed.
    #[error("recover DA blob ({0})")]
    DaBlobRecovery(#[from] DaBlobRecoveryError),
    /// The DA filter failed to look up a code hash's holder.
    #[error("code holder read failed for {hash} ({error})")]
    CodeHolderStore { hash: String, error: String },
    /// The DA filter knows no holder for a code hash the blob omits.
    #[error("no holder for deduped code hash {0} while building DA witness")]
    CodeHolderMissing(String),
}
