//! Host-side helpers for constructing the [`DaWitness`] the
//! [`verification`](crate::verification) module checks. Gated behind the
//! `builders` feature so guest/proof builds link only the verifier.
//!
//! - `inclusion` — the generic L1 byte-blob inclusion layer (walking the batch's L1 blocks, wtxid
//!   proofs, blob reassembly), execution-environment agnostic.
//! - `dedup` — the EVM bytecode-dedup layer: which bytecodes the blob omits, naming a pre-state
//!   holder for each, and the [`DedupWitnessResolver`] seam that finds those holders.
//! - [`build_da_witness`] — the single entry point that orchestrates the two into a [`DaWitness`].

mod dedup;
mod error;
mod inclusion;

use alloy_primitives::Address;
use alpen_common::L1DaBlockRef;
use alpen_da_types::DaWitness;
use alpen_params::AlpenSpecId;
use bitcoind_async_client::traits::Reader;
pub(crate) use dedup::dedup_witness_for_holders;
#[cfg(feature = "test-utils")]
pub(crate) use dedup::deduped_account_code_hashes;
pub use dedup::{DaDedupResolver, DedupWitnessResolver};
pub use error::DaWitnessBuildError;

use self::inclusion::collect_l1_inclusion_blocks;
use crate::payload::recover_ee_da_blob;

/// A batch's DA witness, plus the accounts its dedup witness names.
#[derive(Debug)]
pub struct BatchDaWitness {
    /// The witness the account proof guest checks.
    pub witness: DaWitness,

    /// Accounts holding, in the batch's pre-state, the bytecodes the blob
    /// omits. The prover must add them to the batch's partial pre-state, where
    /// the guest reads their code hashes.
    pub code_holders: Vec<Address>,
}

/// Builds the DA witness for a batch: the generic L1 tx-inclusion proofs plus the
/// supplementary dedup witness built from the holders `resolver` names.
///
/// This is the single entry point the prover calls. It walks the batch's L1
/// blocks, reassembles the published blob (the "established as published" handoff
/// between the generic inclusion layer and the EVM dedup resolution), and returns
/// the [`DaWitness`]. `spec_version` is the version governing the batch, which
/// names the layout the blob was published under.
pub async fn build_da_witness(
    da_refs: &[L1DaBlockRef],
    spec_version: AlpenSpecId,
    btc: &(impl Reader + Sync),
    resolver: &(impl DedupWitnessResolver + Sync),
) -> Result<BatchDaWitness, DaWitnessBuildError> {
    let (blocks, included_blocks) = collect_l1_inclusion_blocks(da_refs, btc).await?;

    let blob = recover_ee_da_blob(included_blocks, spec_version)?;

    let code_holders = resolver.resolve_code_holders(&blob).await?;
    let witness = DaWitness::new(blocks, dedup_witness_for_holders(&code_holders));

    Ok(BatchDaWitness {
        witness,
        code_holders,
    })
}
