//! Records for tests that need plausible exec blocks.

use alpen_acct_types::EeAccountState;
use alpen_chain_types::{ExecBlockCommitment, ExecBlockPackage, ExecInputs, ExecOutputs};
use alpen_common::ExecBlockRecord;
use alpen_params::AlpenSpecId;
use strata_acct_types::{AccountId, BitcoinAmount, Hash, MessageEntry, MsgPayload};
use strata_identifiers::{Buf32, OLBlockCommitment, OLBlockId};

/// Helper to create a test OL block commitment
fn create_ol_block(slot: u64, value: u8) -> OLBlockCommitment {
    OLBlockCommitment::new(slot, OLBlockId::from(Buf32::new([value; 32])))
}

/// Helper to create a minimal valid ExecBlockPackage for testing
fn create_package(exec_block_id: Hash, raw_block_encoded_hash: Hash) -> ExecBlockPackage {
    ExecBlockPackage::new(
        ExecBlockCommitment::new(exec_block_id, raw_block_encoded_hash),
        ExecInputs::new_empty(),
        ExecOutputs::new_empty(),
    )
}

/// Helper to create an EeAccountState with a specific block hash
fn create_account_state(blockhash: Hash) -> EeAccountState {
    EeAccountState::new(blockhash, Hash::zero(), Vec::new(), Vec::new())
}

/// Helper to create a test MessageEntry
pub fn create_message_entry(source_id: u8, epoch: u32, data: Vec<u8>) -> MessageEntry {
    let source = AccountId::from([source_id; 32]);
    let payload = MsgPayload::from_bytes(BitcoinAmount::default(), data)
        .expect("message payload bytes must fit within SSZ max length");
    MessageEntry::new(source, epoch, payload)
}

/// Helper to create a test ExecBlockRecord with proper parent-child relationship
pub fn create_exec_block(
    blocknum: u64,
    parent_hash: Hash,
    block_hash: Hash,
    ol_slot: u64,
) -> ExecBlockRecord {
    create_exec_block_with_messages(blocknum, parent_hash, block_hash, ol_slot, Vec::new())
}

/// Helper to create a test ExecBlockRecord with messages
pub fn create_exec_block_with_messages(
    blocknum: u64,
    parent_hash: Hash,
    block_hash: Hash,
    ol_slot: u64,
    messages: Vec<MessageEntry>,
) -> ExecBlockRecord {
    let package = create_package(block_hash, block_hash);
    let account_state = create_account_state(block_hash);
    let ol_block = create_ol_block(ol_slot, blocknum as u8);
    let timestamp_ms = 1_000_000 + blocknum * 1_000;

    ExecBlockRecord::new(
        package,
        account_state,
        blocknum,
        ol_block,
        timestamp_ms,
        parent_hash,
        0,
        0,
        AlpenSpecId::V0,
        messages,
    )
}
