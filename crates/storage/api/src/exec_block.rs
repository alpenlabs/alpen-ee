use alpen_common::{ExecBlockPayload, ExecBlockRecord};
use async_trait::async_trait;
use strata_acct_types::Hash;

use super::StorageError;

#[cfg_attr(feature = "test-utils", mockall::automock)]
#[async_trait]
/// Persistence for Exec Blocks
///
/// This expects blocks to be stored as "finalized" or "unfinalized".
/// "finalized" blocks should only ever have a single canonical chain.
/// "unfinalized" blocks may have forks, and all such blocks need to be persisted.
pub trait ExecBlockStorage: Send + Sync {
    /// Save block data and payload for a given block hash.
    ///
    /// Blocks are uniquely identified by `ExecBlockRecord::blockhash()`. If a block with the same
    /// hash already exists, the original data is preserved (no overwrite). Succeeds in both cases.
    async fn save_exec_block(
        &self,
        block: ExecBlockRecord,
        payload: ExecBlockPayload,
    ) -> Result<(), StorageError>;

    /// Initialize the finalized chain with a genesis block.
    ///
    /// The block must have been previously saved via `save_exec_block`. Should be idempotent:
    /// calling multiple times with the same hash should succeed. Fails if block doesn't exist.
    async fn init_finalized_chain(&self, hash: Hash) -> Result<(), StorageError>;

    /// Extend the finalized chain up to and including `new_tip`.
    ///
    /// Walks parent links from `new_tip` back to the current best finalized block and inserts
    /// each missing canonical block in order. Atomic: either all intermediate finalized entries
    /// are inserted or none are. Works for both single-block and multi-block extensions.
    /// Fails if chain is empty, `new_tip` is unknown, an intermediate parent is missing, or
    /// `new_tip` does not descend from the current finalized tip.
    async fn extend_finalized_chain(&self, new_tip: Hash) -> Result<(), StorageError>;

    /// Revert the finalized chain to a specified height.
    ///
    /// All blocks above `to_height` are removed from the finalized chain and become unfinalized.
    /// Block data is preserved. Fails if chain is empty.
    async fn revert_finalized_chain(&self, to_height: u64) -> Result<(), StorageError>;

    /// Permanently delete all block data and payloads below the specified height.
    ///
    /// Removes blocks and their payloads at heights < `to_height`. Block data cannot be recovered
    /// after pruning.
    async fn prune_block_data(&self, to_height: u64) -> Result<(), StorageError>;

    /// Get the highest block in the finalized chain.
    ///
    /// Returns `None` if the finalized chain is empty (not yet initialized). Returns the block
    /// with the highest block number that has been finalized.
    async fn best_finalized_block(&self) -> Result<Option<ExecBlockRecord>, StorageError>;

    /// Get the finalized block at a specific height.
    ///
    /// Returns `None` if no block is finalized at the given height. Returns the block
    /// that is finalized at the specified height.
    async fn get_finalized_block_at_height(
        &self,
        height: u64,
    ) -> Result<Option<ExecBlockRecord>, StorageError>;

    /// Get the finalized height of a block, if it exists on the finalized chain.
    ///
    /// Returns `None` if the block doesn't exist, hasn't been finalized, or exists at a height
    /// but a different block hash is finalized at that height (fork case).
    async fn get_finalized_height(&self, hash: Hash) -> Result<Option<u64>, StorageError>;

    /// Get all unfinalized blocks (height > best finalized height).
    ///
    /// Returns block hashes ordered by incrementing height. May include multiple blocks at the
    /// same height (forks). Fails if finalized chain is empty.
    async fn get_unfinalized_blocks(&self) -> Result<Vec<Hash>, StorageError>;

    /// Get block data for a block by its hash.
    ///
    /// Returns `None` if the block doesn't exist. Works for both finalized and unfinalized blocks.
    async fn get_exec_block(&self, hash: Hash) -> Result<Option<ExecBlockRecord>, StorageError>;

    /// Get block payload for a block by its hash.
    ///
    /// Returns `None` if the block doesn't exist. Works for both finalized and unfinalized blocks.
    async fn get_block_payload(&self, hash: Hash)
        -> Result<Option<ExecBlockPayload>, StorageError>;

    /// Delete a single block and its payload by hash.
    ///
    /// Removes the block data, payload, and updates the height index. Returns an error if the
    /// block is in the finalized chain. Returns `Ok(())` if the block doesn't exist (idempotent).
    async fn delete_exec_block(&self, hash: Hash) -> Result<(), StorageError>;
}

#[cfg(feature = "test-utils")]
pub mod exec_block_storage_test_fns {
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
}
