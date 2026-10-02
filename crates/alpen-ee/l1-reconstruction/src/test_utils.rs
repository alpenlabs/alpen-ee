use alpen_ee_da_l1_extraction::{EeDaL1Ref, RecoveredDaBlob};
use alpen_ee_da_types::{DaBlob, EvmHeaderSummary};
use alpen_reth_statediff::BatchStateDiff;
use bitcoin::{hashes::Hash, Txid};
use strata_identifiers::{L1BlockCommitment, L1BlockId};

pub(crate) fn make_l1_ref(seed: u8) -> EeDaL1Ref {
    EeDaL1Ref::new(
        Txid::from_byte_array([seed; 32]),
        L1BlockCommitment::new(u32::from(seed), L1BlockId::default()),
    )
}

pub(crate) fn build_evm_header(block_num: u64) -> EvmHeaderSummary {
    EvmHeaderSummary {
        block_num,
        timestamp: 1_700_000_000 + block_num,
        base_fee: 100,
        gas_used: 21_000,
        gas_limit: 36_000_000,
    }
}

pub(crate) fn build_da_blob(update_seq_no: u64, block_num: u64) -> DaBlob {
    DaBlob {
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
    RecoveredDaBlob::new(make_l1_ref(seed), build_da_blob(update_seq_no, block_num))
}
