//! Proof-side DA correctness checks for the EE outer proof.
//!
//! Reassembles the DA blob from witnessed commit/reveal transactions,
//! verifies the commit marker matches the active proof's magic/version,
//! ties the parsed `EvmHeaderSummary` to chunk public values, checks that
//! every code hash the diff references is either carried in the blob or held
//! by an account in the partial pre-state, and applies the state diff to the
//! partial pre-state to confirm it reproduces the last chunk's
//! `tip_state_root`.

mod error;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

use alloy_primitives::{keccak256, B256};
use alloy_trie::TrieAccount;
use alpen_acct_runtime::ArchivedEePrivateInput;
use alpen_chain_types::ChunkTransition;
use alpen_da_types::{
    compute_bitcoin_merkle_root_from_proof, ArchivedBitcoinMerkleProof, ArchivedDaBlockWitness,
    ArchivedDaWitness, ArchivedDedupWitness, DaBlob, EvmHeaderSummary,
};
use alpen_evm_ee::EvmPartialState;
use alpen_params::AlpenSpecId;
use alpen_reth_statediff::{
    apply_batch_state_diff_to_ethereum_state, AccountChange, BatchStateDiff,
};
use bitcoin::{consensus::deserialize as btc_deserialize, hashes::Hash as _, Transaction};
pub use error::{DaVerificationError, DaVerificationResult};
use revm_primitives::KECCAK_EMPTY;
use ssz::Decode;
use strata_acct_types::l1_block_record_leaf_hash;
use strata_codec::decode_buf_exact;
use strata_snark_acct_types::{LedgerRefs, UpdateProofPubParams};

use crate::payload::{recover_ee_da_blob, L1DaTransactions};

/// Runs DA correctness checks for the outer proof.
///
/// An update with no DA witness blocks AND no chunks is treated as a valid
/// empty-update no-op; any other combination of emptiness is an error. The
/// reassembled blob is verified against the chunk public values internally
/// (header summary, deployed bytecodes, state-diff applied to the partial
/// pre-state matches the last chunk's `tip_state_root`). There is no
/// downstream consumer for the blob itself, so nothing is returned.
///
/// `spec_version` is the version governing the batch. It selects the layout
/// both the blob and the last chunk's header summary are decoded under.
pub fn verify_da_witness(
    ee_input: &ArchivedEePrivateInput,
    da_witness: &ArchivedDaWitness,
    pub_params: &UpdateProofPubParams,
    expected_pre_state_root: [u8; 32],
    spec_version: AlpenSpecId,
) -> DaVerificationResult {
    if da_witness.blocks().is_empty() {
        return if ee_input.chunks().is_empty() {
            Ok(())
        } else {
            Err(DaVerificationError::MissingDaWitness)
        };
    }

    if ee_input.chunks().is_empty() {
        return Err(DaVerificationError::NoChunks);
    }

    let mut included_blocks = Vec::with_capacity(da_witness.blocks().len());
    for block in da_witness.blocks() {
        let transactions = verify_block_witness(block, pub_params.ledger_refs())?;
        included_blocks.push(L1DaTransactions::new(
            block.inclusion().l1_block_height(),
            transactions,
        ));
    }

    let blob = recover_ee_da_blob(included_blocks, spec_version)?;
    let last_chunk = decode_last_chunk_transition(ee_input)?;

    let raw_pre_state = ee_input.raw_partial_pre_state();
    if raw_pre_state.is_empty() {
        return Err(DaVerificationError::MissingPartialPreState);
    }
    let pre_state = decode_partial_pre_state(raw_pre_state, expected_pre_state_root)?;

    verify_da_blob_metadata(
        &blob,
        &last_chunk,
        pub_params,
        &pre_state,
        da_witness.dedup_da_witness(),
    )?;
    verify_state_diff_against_chunks(pre_state, &blob, &last_chunk)?;

    Ok(())
}

fn decode_last_chunk_transition(
    ee_input: &ArchivedEePrivateInput,
) -> DaVerificationResult<ChunkTransition> {
    let chunks = ee_input.chunks();
    let last = chunks.last().ok_or(DaVerificationError::NoChunks)?;
    ChunkTransition::from_ssz_bytes(last.chunk_transition_ssz())
        .map_err(DaVerificationError::LastChunkDecode)
}

// L1 inclusion checks

fn bitcoin_merkle_root_from_archived_proof(
    leaf_hash: [u8; 32],
    proof: &ArchivedBitcoinMerkleProof,
) -> [u8; 32] {
    compute_bitcoin_merkle_root_from_proof(leaf_hash, proof.siblings(), proof.position())
}

/// Verifies every witnessed DA transaction in one L1 block.
fn verify_block_witness(
    block: &ArchivedDaBlockWitness,
    ledger_refs: &LedgerRefs,
) -> DaVerificationResult<Vec<Transaction>> {
    verify_l1_ref_binding(block, ledger_refs)?;

    if block.txs().is_empty() {
        return Err(DaVerificationError::MissingDaTransactions);
    }

    let expected_root = *block.inclusion().wtxids_root();
    let mut decoded = Vec::with_capacity(block.txs().len());
    for tx_witness in block.txs() {
        let tx: Transaction = btc_deserialize(tx_witness.raw_tx())
            .map_err(|e| DaVerificationError::DaTxDecode(e.to_string()))?;
        let computed_root = bitcoin_merkle_root_from_archived_proof(
            tx.compute_wtxid().to_byte_array(),
            tx_witness.wtxid_inclusion_proof(),
        );
        if computed_root != expected_root {
            return Err(DaVerificationError::WtxidsRootMismatch {
                expected: expected_root,
                computed: computed_root,
            });
        }
        decoded.push(tx);
    }

    Ok(decoded)
}

fn verify_l1_ref_binding(
    block: &ArchivedDaBlockWitness,
    ledger_refs: &LedgerRefs,
) -> DaVerificationResult {
    let inclusion = block.inclusion();
    let idx = u64::from(inclusion.l1_block_height());
    let expected_hash =
        l1_block_record_leaf_hash(inclusion.l1_block_hash(), inclusion.wtxids_root());

    let found = ledger_refs
        .l1_block_refs()
        .iter()
        .any(|claim| claim.idx() == idx && claim.entry_hash().as_ref() == expected_hash.as_slice());
    if !found {
        return Err(DaVerificationError::L1DaBlockRefNotInLedgerRefs { idx });
    }

    Ok(())
}

// Blob metadata + bytecode + state-diff verification

fn verify_da_blob_metadata(
    blob: &DaBlob,
    last_chunk: &ChunkTransition,
    pub_params: &UpdateProofPubParams,
    pre_state: &EvmPartialState,
    dedup_da_witness: &ArchivedDedupWitness,
) -> DaVerificationResult {
    let expected_seq_no = *pub_params.seq_no().inner();
    if blob.update_seq_no != expected_seq_no {
        return Err(DaVerificationError::UpdateSeqNoMismatch {
            expected: expected_seq_no,
            actual: blob.update_seq_no,
        });
    }

    let expected_header = EvmHeaderSummary::decode_exact(
        blob.spec_version,
        last_chunk.tip_exec_header_summary().opaque_bytes(),
    )
    .map_err(DaVerificationError::ExecHeaderSummaryDecode)?;
    if blob.evm_header != expected_header {
        return Err(DaVerificationError::EvmHeaderMismatch {
            expected: expected_header,
            actual: blob.evm_header,
        });
    }

    verify_deployed_bytecodes(&blob.state_diff, pre_state, dedup_da_witness)
}

/// Collects the code hashes the dedup witness vouches for.
///
/// DA dedup lets the published blob omit bytecode an account already holds in
/// the batch's pre-state. The dedup witness names each holder by its state
/// trie key, and the holder's code hash is read from the pre-state, whose root
/// was checked against the EE account's previous execution root. The holder
/// took on that code in an earlier batch, whose blob was checked the same way,
/// so by induction from genesis the bytes are on L1. Future dedup kinds
/// (account/storage serials) extend this with their own contributions.
///
/// NOTE: bytecode preimages are still accepted too. A preimage proves identity
/// (the bytes hash to the referenced code hash), not that the bytes were
/// published on L1 before.
/// TODO(SECFND-513): stop accepting preimages once the prover sends holders.
fn dedup_witness_code_hashes(
    pre_state: &EvmPartialState,
    dedup_da_witness: &ArchivedDedupWitness,
) -> DaVerificationResult<BTreeSet<B256>> {
    let preimage_code_hashes = dedup_da_witness
        .deduped_bytecode_preimages()
        .iter()
        .map(|preimage| Ok(keccak256(preimage.bytecode())));
    let state_trie = &pre_state.ethereum_state().state_trie;
    let holder_code_hashes = dedup_da_witness.code_holder_keys().iter().map(|key| {
        let holder = state_trie
            .get_rlp::<TrieAccount>(key)
            .map_err(|e| DaVerificationError::CodeHolderLookup {
                key: *key,
                error: e.to_string(),
            })?
            .ok_or(DaVerificationError::CodeHolderAbsent(*key))?;
        Ok(holder.code_hash)
    });
    preimage_code_hashes.chain(holder_code_hashes).collect()
}

fn verify_deployed_bytecodes(
    diff: &BatchStateDiff,
    pre_state: &EvmPartialState,
    dedup_da_witness: &ArchivedDedupWitness,
) -> DaVerificationResult {
    let mut available_code_hashes = BTreeSet::new();

    // Bytecodes carried inline in the blob: re-derive and check their hash.
    for (code_hash, bytecode) in &diff.deployed_bytecodes {
        let computed = keccak256(bytecode.as_ref());
        if computed != *code_hash {
            return Err(DaVerificationError::DeployedBytecodeHashMismatch {
                expected: code_hash.0,
                computed: computed.0,
            });
        }
        available_code_hashes.insert(*code_hash);
    }

    // Bytecodes the blob deduped: held by a pre-state account the dedup
    // witness names.
    available_code_hashes.extend(dedup_witness_code_hashes(pre_state, dedup_da_witness)?);

    for change in diff.accounts.values() {
        let account_diff = match change {
            AccountChange::Created(diff) | AccountChange::Updated(diff) => diff,
            AccountChange::Deleted => continue,
        };
        let Some(code_hash) = account_diff.code_hash.new_value().map(|hash| hash.0) else {
            continue;
        };
        if code_hash == KECCAK_EMPTY {
            continue;
        }
        if !available_code_hashes.contains(&code_hash) {
            return Err(DaVerificationError::MissingDeployedBytecode(code_hash.0));
        }
    }

    Ok(())
}

/// Decodes the partial pre-state witness and checks it against the EE account
/// state's previous execution root.
fn decode_partial_pre_state(
    raw_pre_state: &[u8],
    expected_pre_state_root: [u8; 32],
) -> DaVerificationResult<EvmPartialState> {
    let pre_state: EvmPartialState =
        decode_buf_exact(raw_pre_state).map_err(DaVerificationError::PartialPreStateDecode)?;
    let actual_pre_state_root = pre_state.ethereum_state().state_root().0;
    if actual_pre_state_root != expected_pre_state_root {
        return Err(DaVerificationError::PartialPreStateRootMismatch {
            expected: expected_pre_state_root,
            actual: actual_pre_state_root,
        });
    }
    Ok(pre_state)
}

/// Applies the DA blob state diff to the partial pre-state witness.
///
/// The post-apply root must match the last chunk transition's public
/// `tip_state_root`.
fn verify_state_diff_against_chunks(
    mut pre_state: EvmPartialState,
    blob: &DaBlob,
    last_chunk: &ChunkTransition,
) -> DaVerificationResult {
    apply_batch_state_diff_to_ethereum_state(pre_state.ethereum_state_mut(), &blob.state_diff)?;

    let computed: [u8; 32] = pre_state.ethereum_state().state_root().0;
    let expected: [u8; 32] = last_chunk.tip_state_root().0;
    if computed != expected {
        return Err(DaVerificationError::PostApplyStateRootMismatch { computed, expected });
    }
    Ok(())
}
