//! Perf input for the alpen-acct SP1 guest.
//!
//! Proves one account update over the checked-in mixed workload. The batch is
//! the workload's one chunk, its deposits arrive as inbox messages in the same
//! update, and its DA is posted to L1. The update carries the chunk proof that
//! `--generate-proof` saved, so under SP1 the guest runs a real Groth16 verify.
//! The input comes from [`build_workload_update`], the builder the account
//! proof crate's own native test runs.

use alpen_chain_types::ChunkTransition;
use alpen_proof_acct::{test_utils::build_workload_update, EeAcctProgram, EeAcctProofInput};
use ssz::Decode;
use tracing::info;
use zkaleido::{ExecutionSummary, ProofReceiptWithMetadata, ZkVmHost, ZkVmProgram};

use super::alpen_chunk::{workload, workload_chunk, PERF_SPEC_VERSION};

/// Builds the update over the workload's chunk, carrying `chunk_proof`.
///
/// Panics if `chunk_proof` proves a different chunk than the workload's, which
/// means one of the two was regenerated without the other.
pub(super) fn prepare_input(chunk_proof: &ProofReceiptWithMetadata) -> EeAcctProofInput {
    let chunk = workload_chunk();
    let proven = ChunkTransition::from_ssz_bytes(chunk_proof.receipt().public_values().as_bytes())
        .expect("chunk proof public values are a chunk transition");
    assert_eq!(
        proven, chunk.transition,
        "the chunk proof is for a different chunk than the workload; regenerate it with \
         `just prover-proof`"
    );
    build_workload_update(
        workload(),
        &chunk.transition,
        chunk_proof.receipt().proof().as_bytes().to_vec(),
        PERF_SPEC_VERSION,
    )
}

pub(crate) fn gen_perf_report(
    host: &impl ZkVmHost,
    chunk_proof: &ProofReceiptWithMetadata,
) -> (String, ExecutionSummary) {
    info!("Generating execution summary for Alpen Acct");
    let input = prepare_input(chunk_proof);
    let summary =
        <EeAcctProgram as ZkVmProgram>::execute(&input, host).expect("alpen-acct execution");
    (EeAcctProgram::name(), summary)
}

pub(crate) fn gen_proof(
    host: &impl ZkVmHost,
    chunk_proof: &ProofReceiptWithMetadata,
) -> ProofReceiptWithMetadata {
    info!("Generating proof for Alpen Acct");
    let input = prepare_input(chunk_proof);
    <EeAcctProgram as ZkVmProgram>::prove(&input, host).expect("alpen-acct proof")
}
