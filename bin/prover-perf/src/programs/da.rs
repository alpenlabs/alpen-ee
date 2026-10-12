//! The batch's DA as the alpen-acct guest checks it.
//!
//! The DA blob is built the way the blob provider builds it, split into the
//! SPS-51 commit/reveal envelopes the sequencer posts, and placed in a
//! synthetic L1 block. The block is filled out to mainnet size, so the wtxid
//! inclusion proofs the guest checks have mainnet depth.

use std::collections::BTreeSet;

use alloy_primitives::{keccak256, B256};
use alpen_chain_types::ChunkTransition;
use alpen_da_types::{
    compute_bitcoin_inclusion_proof, compute_bitcoin_merkle_root_from_leaves, da_blob_version,
    BitcoinMerkleProof, BytecodePreimage, DaBlob, DaBlockWitness, DaTxWitness, DaWitness,
    DedupWitness, EvmHeaderSummary, L1DaBlockInclusion, EE_DA_MAGIC_BYTES,
};
use alpen_reth_statediff::{AccountChange, BatchStateDiff};
use alpen_test_utils_evm_workload::Workload;
use bitcoin::{
    absolute::LockTime,
    consensus::serialize,
    hashes::Hash as _,
    key::Keypair,
    opcodes::all::OP_RETURN,
    script,
    secp256k1::{SecretKey, SECP256K1},
    taproot::{LeafVersion, TaprootBuilder},
    transaction::Version,
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, WPubkeyHash, Witness,
};
use strata_acct_types::l1_block_record_leaf_hash;
use strata_codec::decode_buf_exact;
use strata_l1_envelope_fmt::{EnvelopeScriptBuilder, MAX_ENVELOPE_PAYLOAD_SIZE};
use strata_snark_acct_types::{AccumulatorClaim, LedgerRefs};

use super::alpen_chunk::PERF_SPEC_VERSION;

/// Height of the L1 block carrying the batch's DA.
const L1_BLOCK_HEIGHT: u32 = 900_000;

/// Hash of the L1 block carrying the batch's DA. The guest only checks it
/// against the ledger refs, so any value works.
const L1_BLOCK_HASH: [u8; 32] = [0x11; 32];

/// Transactions in the L1 block besides the DA ones, about a full mainnet
/// block.
const L1_OTHER_TXS: usize = 3_000;

/// Fee-paying input and change amounts of the DA transactions. The guest does
/// not check them; they only give the transactions realistic sizes.
const DA_INPUT_SATS: u64 = 1_000_000;
const DUST_SATS: u64 = 546;

/// The batch's DA witness and the ledger refs binding it to its L1 block.
#[derive(Debug)]
pub(super) struct BatchDa {
    pub(super) witness: DaWitness,
    pub(super) ledger_refs: LedgerRefs,
}

/// Builds the DA of a batch that is `workload`'s range, ending in
/// `last_chunk`, posted as update `seq_no`.
pub(super) fn build_batch_da(
    workload: &Workload,
    last_chunk: &ChunkTransition,
    seq_no: u64,
) -> BatchDa {
    let full_diff: BatchStateDiff =
        decode_buf_exact(&workload.state_diff).expect("decode batch state diff");
    let published: BTreeSet<B256> = workload
        .published_code_hashes
        .iter()
        .copied()
        .map(B256::from)
        .collect();

    let mut state_diff = full_diff.clone();
    state_diff
        .deployed_bytecodes
        .retain(|hash, _| !published.contains(hash));
    let blob = DaBlob {
        spec_version: PERF_SPEC_VERSION,
        update_seq_no: seq_no,
        evm_header: EvmHeaderSummary::decode_exact(
            PERF_SPEC_VERSION,
            last_chunk.tip_exec_header_summary().opaque_bytes(),
        )
        .expect("decode tip header summary"),
        state_diff,
    };
    let dedup = DedupWitness::new(deduped_bytecode_preimages(&blob, &full_diff));

    let encoded_blob = blob.encode_to_vec().expect("encode DA blob");
    let (commit, reveals) = envelope_txs(&encoded_blob);

    // Coinbase leaf (zero per BIP-141), the block's other transactions, then
    // the DA ones.
    let mut leaves = vec![[0u8; 32]];
    leaves.extend((0..L1_OTHER_TXS).map(|index| keccak256(index.to_be_bytes()).0));
    let first_da_leaf = leaves.len();
    let da_txs: Vec<Transaction> = [commit].into_iter().chain(reveals).collect();
    leaves.extend(da_txs.iter().map(|tx| tx.compute_wtxid().to_byte_array()));
    let wtxids_root = compute_bitcoin_merkle_root_from_leaves(&leaves);

    let tx_witnesses = da_txs
        .iter()
        .enumerate()
        .map(|(offset, tx)| {
            let position = (first_da_leaf + offset) as u32;
            DaTxWitness::new(
                serialize(tx),
                BitcoinMerkleProof::new(
                    compute_bitcoin_inclusion_proof(&leaves, position),
                    position,
                ),
            )
        })
        .collect();

    BatchDa {
        witness: DaWitness::new(
            vec![DaBlockWitness::new(
                L1DaBlockInclusion::new(L1_BLOCK_HEIGHT, L1_BLOCK_HASH, wtxids_root),
                tx_witnesses,
            )],
            dedup,
        ),
        ledger_refs: LedgerRefs::new(vec![AccumulatorClaim::new(
            L1_BLOCK_HEIGHT.into(),
            l1_block_record_leaf_hash(&L1_BLOCK_HASH, &wtxids_root),
        )]),
    }
}

/// Preimages of the bytecodes the blob's account diffs point at but the blob
/// leaves out, because an earlier batch published them.
///
/// Mirrors `bytecode_preimages_from_batch_diff` in alpen-da-runtime's
/// builders, which resolves them from the batch's full diff the same way.
fn deduped_bytecode_preimages(blob: &DaBlob, full_diff: &BatchStateDiff) -> Vec<BytecodePreimage> {
    let empty_code_hash = keccak256([]);
    let deduped: BTreeSet<B256> = blob
        .state_diff
        .accounts
        .values()
        .filter_map(|change| match change {
            AccountChange::Created(diff) | AccountChange::Updated(diff) => {
                diff.code_hash.new_value().map(|hash| hash.0)
            }
            AccountChange::Deleted => None,
        })
        .filter(|hash| {
            *hash != empty_code_hash && !blob.state_diff.deployed_bytecodes.contains_key(hash)
        })
        .collect();

    deduped
        .into_iter()
        .map(|hash| {
            let bytecode = full_diff
                .deployed_bytecodes
                .get(&hash)
                .expect("deduped bytecode is deployed in the batch");
            BytecodePreimage::new(bytecode.to_vec())
        })
        .collect()
}

/// The commit transaction and one reveal per chunk of `blob`, as SPS-51 lays
/// them out: the commit's vout 0 is the `magic || version` marker, vouts
/// `1..=n` are the reveals' taproot outputs, and a non-taproot change output
/// follows.
fn envelope_txs(blob: &[u8]) -> (Transaction, Vec<Transaction>) {
    let secret = SecretKey::from_slice(&[0x42; 32]).expect("valid secret key");
    let keypair = Keypair::from_secret_key(SECP256K1, &secret);
    let (internal_key, _) = keypair.x_only_public_key();

    let reveal_scripts: Vec<ScriptBuf> = blob
        .chunks(MAX_ENVELOPE_PAYLOAD_SIZE)
        .map(|chunk| {
            EnvelopeScriptBuilder::with_pubkey(&internal_key.serialize())
                .expect("valid envelope pubkey")
                .add_envelopes(&[chunk.to_vec()])
                .expect("chunk fits one envelope")
                .build_without_min_check()
                .expect("build envelope script")
        })
        .collect();
    let spend_infos: Vec<_> = reveal_scripts
        .iter()
        .map(|reveal_script| {
            TaprootBuilder::new()
                .add_leaf(0, reveal_script.clone())
                .expect("single leaf")
                .finalize(SECP256K1, internal_key)
                .expect("finalize taproot tree")
        })
        .collect();

    let mut marker = EE_DA_MAGIC_BYTES.to_vec();
    marker.extend_from_slice(&da_blob_version(PERF_SPEC_VERSION).to_be_bytes());
    let marker: [u8; 8] = marker.try_into().expect("marker is 8 bytes");
    let mut outputs = vec![TxOut {
        value: Amount::ZERO,
        script_pubkey: script::Builder::new()
            .push_opcode(OP_RETURN)
            .push_slice(marker)
            .into_script(),
    }];
    outputs.extend(spend_infos.iter().map(|spend_info| TxOut {
        value: Amount::from_sat(DUST_SATS),
        script_pubkey: ScriptBuf::new_p2tr_tweaked(spend_info.output_key()),
    }));
    outputs.push(change_output());
    let commit = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![key_spend_input(OutPoint::new(Txid::all_zeros(), 0))],
        output: outputs,
    };
    let commit_txid = commit.compute_txid();

    let reveals = reveal_scripts
        .into_iter()
        .zip(&spend_infos)
        .enumerate()
        .map(|(index, (reveal_script, spend_info))| {
            let control_block = spend_info
                .control_block(&(reveal_script.clone(), LeafVersion::TapScript))
                .expect("leaf is in the tree");
            let mut witness = Witness::new();
            witness.push([0u8; 64]);
            witness.push(reveal_script.as_bytes());
            witness.push(control_block.serialize());
            Transaction {
                version: Version::TWO,
                lock_time: LockTime::ZERO,
                input: vec![TxIn {
                    previous_output: OutPoint::new(commit_txid, index as u32 + 1),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness,
                }],
                output: vec![change_output()],
            }
        })
        .collect();

    (commit, reveals)
}

/// A taproot key-path spend of `previous_output`, signature left zeroed.
fn key_spend_input(previous_output: OutPoint) -> TxIn {
    let mut witness = Witness::new();
    witness.push([0u8; 64]);
    TxIn {
        previous_output,
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness,
    }
}

fn change_output() -> TxOut {
    TxOut {
        value: Amount::from_sat(DA_INPUT_SATS),
        script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::all_zeros()),
    }
}
