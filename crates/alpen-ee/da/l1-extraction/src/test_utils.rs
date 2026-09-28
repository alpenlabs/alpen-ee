use alpen_ee_da_types::{DaBlob, EvmHeaderSummary};
use bitcoin::{
    block::{Header, Version},
    hashes::Hash,
    pow::CompactTarget,
    secp256k1::XOnlyPublicKey,
    Block, BlockHash, Transaction, TxMerkleNode,
};
use strata_identifiers::L1Height;
use strata_l1_envelope_fmt::test_utils as commit_reveal_fixtures;
use strata_l1_txfmt::MagicBytes;

use crate::L1BlockData;

pub(crate) const SEQUENCER_KEY_SEED: u8 = 7;

pub(crate) fn make_alpen_magic_bytes() -> MagicBytes {
    "ALPN".parse().expect("valid ASCII magic")
}

pub(crate) fn make_sequencer_pubkey() -> XOnlyPublicKey {
    commit_reveal_fixtures::make_xonly_pubkey(SEQUENCER_KEY_SEED)
}

pub(crate) fn build_l1_block_data(height: L1Height, txs: Vec<Transaction>) -> L1BlockData {
    let block = Block {
        header: Header {
            version: Version::from_consensus(1),
            prev_blockhash: BlockHash::all_zeros(),
            merkle_root: TxMerkleNode::all_zeros(),
            time: 0,
            bits: CompactTarget::from_consensus(0),
            nonce: 0,
        },
        txdata: txs,
    };
    L1BlockData::new(height, block)
}

pub(crate) fn make_da_blob() -> DaBlob {
    DaBlob {
        update_seq_no: 3,
        evm_header: EvmHeaderSummary {
            block_num: 9,
            timestamp: 1_700_000_000,
            base_fee: 100,
            gas_used: 21_000,
            gas_limit: 36_000_000,
        },
        state_diff: Default::default(),
    }
}
