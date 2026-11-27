use alpen_da_l1_extraction::{DaL1Ref, RecoveredDaBlob};
use alpen_da_types::{DaBlob, EvmHeaderSummary};
use alpen_params::AlpenSpecId;
use alpen_reth_statediff::BatchStateDiff;
use bitcoin::{hashes::Hash, Txid};
use strata_identifiers::{Buf32, L1BlockCommitment, L1BlockId};
use strata_l1_envelope_fmt::L1TxRef;

/// Offset of the block confirming a payload's commit.
const COMMIT_BLOCK_OFFSET: u8 = 0;

/// Offset of the block whose reveals complete a payload.
const COMPLETION_BLOCK_OFFSET: u8 = 1;

/// Builds an L1 reference whose every identifier derives from `seed`.
pub(crate) fn build_l1_ref(seed: u8) -> DaL1Ref {
    let commit_txref = L1TxRef::new(
        Txid::from_byte_array([seed; 32]),
        build_l1_block(seed, COMMIT_BLOCK_OFFSET),
    );
    DaL1Ref::new(commit_txref, build_l1_block(seed, COMPLETION_BLOCK_OFFSET))
}

/// Builds the L1 block carrying `seed`'s commit or completion.
fn build_l1_block(seed: u8, offset: u8) -> L1BlockCommitment {
    let mut blkid_bytes = [seed; 32];
    blkid_bytes[0] = offset;
    L1BlockCommitment::new(
        u32::from(seed) * 2 + u32::from(offset),
        L1BlockId::from(Buf32::from(blkid_bytes)),
    )
}

pub(crate) fn build_evm_header(block_num: u64) -> EvmHeaderSummary {
    EvmHeaderSummary {
        block_num,
        timestamp: 1_700_000_000 + block_num,
        base_fee: 100,
        gas_used: 21_000,
        gas_limit: 36_000_000,
        da_rate: None,
    }
}

pub(crate) fn build_da_blob(update_seq_no: u64, block_num: u64) -> DaBlob {
    DaBlob {
        spec_version: AlpenSpecId::V0,
        update_seq_no,
        evm_header: build_evm_header(block_num),
        state_diff: BatchStateDiff::new(),
    }
}

pub(crate) fn build_recovered_blob(
    update_seq_no: u64,
    block_num: u64,
    seed: u8,
) -> RecoveredDaBlob {
    RecoveredDaBlob::new(build_l1_ref(seed), build_da_blob(update_seq_no, block_num))
}
