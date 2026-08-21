use alpen_ee_params::AlpenSpecId;
use strata_acct_types::{Hash, MessageEntry};
use strata_ee_acct_types::EeAccountState;
use strata_ee_chain_types::ExecBlockPackage;
use strata_identifiers::OLBlockCommitment;

use crate::BlockNumHash;

/// Additional metadata associated with the block.
///
/// Two different kinds of field live here. `blocknum`, `parent_blockhash`,
/// `timestamp_ms`, and `ol_block` are facts about *this* block; most of them
/// are derivable from `package`/`account_state` and are cached here for ease
/// of access. `next_inbox_msg_idx`, `next_deposit_idx`, and
/// `next_spec_version` are a different kind of thing entirely: block-builder
/// resumption cursors, not properties of this block. Each describes what
/// governs the block built *after* this one, and is stored here — rather
/// than recomputed — so the block builder can resume purely from
/// `ExecBlockRecord` (e.g. after a restart) without replaying history.
#[derive(Debug, Clone)]
struct ExecPackageMetadata {
    /// Blocknumber of the exec chain block.
    blocknum: u64,
    /// Blockhash of the parent exec chain block.
    parent_blockhash: Hash,
    /// Timestamp of the exec block.
    timestamp_ms: u64,
    /// Commitment of the last ol chain block whose inbox messages were used in this exec block.
    ///
    /// Note:
    /// 1. `package.inputs` are derived according to this this ol block and previous exec block.
    /// 2. This does not uniquely identify a package or exec block. One `ol_block` can be linked
    ///    with multiple records.
    ol_block: OLBlockCommitment,

    /// Next inbox message index at this ol_block.
    next_inbox_msg_idx: u64,
    /// Monotonically incrementing index for next deposit to use.
    next_deposit_idx: u64,
    /// Alpen spec version governing the block built *after* this record's
    /// block.
    ///
    /// The block that *consumes* a queued predicate rotation ends with this
    /// already bumped to the successor, even though that same block was
    /// itself still built under the predecessor version.
    next_spec_version: AlpenSpecId,
}

/// `ExecBlockPackage` with additional block metadata
#[derive(Debug, Clone)]
pub struct ExecBlockRecord {
    /// Additional metadata associated with this block.
    metadata: ExecPackageMetadata,
    /// OL Account messages processed in this block.
    messages: Vec<MessageEntry>,
    /// The execution block package with additional block data.
    package: ExecBlockPackage,
    /// The final account state as a result of this execution.
    account_state: EeAccountState,
}

impl ExecBlockRecord {
    #[expect(clippy::too_many_arguments, reason = "need them")]
    pub fn new(
        package: ExecBlockPackage,
        account_state: EeAccountState,
        blocknum: u64,
        ol_block: OLBlockCommitment,
        timestamp_ms: u64,
        parent_blockhash: Hash,
        next_inbox_msg_idx: u64,
        next_deposit_idx: u64,
        next_spec_version: AlpenSpecId,
        messages: Vec<MessageEntry>,
    ) -> Self {
        Self {
            package,
            account_state,
            messages,
            metadata: ExecPackageMetadata {
                blocknum,
                ol_block,
                timestamp_ms,
                parent_blockhash,
                next_inbox_msg_idx,
                next_deposit_idx,
                next_spec_version,
            },
        }
    }

    pub fn package(&self) -> &ExecBlockPackage {
        &self.package
    }

    pub fn account_state(&self) -> &EeAccountState {
        &self.account_state
    }

    pub fn blocknumhash(&self) -> BlockNumHash {
        BlockNumHash::new(self.blockhash(), self.blocknum())
    }

    pub fn blocknum(&self) -> u64 {
        self.metadata.blocknum
    }

    pub fn ol_block(&self) -> &OLBlockCommitment {
        &self.metadata.ol_block
    }

    pub fn timestamp_ms(&self) -> u64 {
        self.metadata.timestamp_ms
    }

    pub fn blockhash(&self) -> Hash {
        self.account_state.last_exec_blkid()
    }

    pub fn parent_blockhash(&self) -> Hash {
        self.metadata.parent_blockhash
    }

    pub fn next_inbox_msg_idx(&self) -> u64 {
        self.metadata.next_inbox_msg_idx
    }

    pub fn next_deposit_idx(&self) -> u64 {
        self.metadata.next_deposit_idx
    }

    pub fn next_spec_version(&self) -> AlpenSpecId {
        self.metadata.next_spec_version
    }

    pub fn messages(&self) -> &[MessageEntry] {
        &self.messages
    }

    pub fn into_parts(self) -> (ExecBlockPackage, EeAccountState, Vec<MessageEntry>) {
        (self.package, self.account_state, self.messages)
    }
}

/// Wrapper for exec block payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecBlockPayload(Vec<u8>);

impl ExecBlockPayload {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn to_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
