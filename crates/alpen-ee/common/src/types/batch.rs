use std::{fmt, iter};

use bitcoin::{Txid, Wtxid};
use strata_acct_types::Hash;
use strata_codec::Codec;
use strata_identifiers::{L1BlockCommitment, L1BlockId, L1Height, WtxidsRoot};
use strata_predicate::PredicateKey;

use crate::{BlockNumHash, ProofId};

/// Unique, deterministic identifier for a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Codec)]
pub struct BatchId {
    prev_block: Hash,
    last_block: Hash,
}

impl fmt::Display for BatchId {
    /// Emits `<prev_block_hex>:<last_block_hex>` with full 32-byte hashes.
    ///
    /// `Buf32`'s `Display` impl truncates to a `prefix..suffix` form, which
    /// is fine for at-a-glance logs but lossy if you ever want to round-trip
    /// the id (e.g. paste it into `strata-dbtool ee-get-acct-proof`). The
    /// `{:x}` formatter on each half uses `LowerHex`, which is the full
    /// hex form, so the rendered string is directly parseable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}:{:x}", self.prev_block(), self.last_block())
    }
}

impl BatchId {
    fn new(prev_block: Hash, last_block: Hash) -> Self {
        Self {
            prev_block,
            last_block,
        }
    }

    /// Create a BatchId from its component parts.
    pub fn from_parts(prev_block: Hash, last_block: Hash) -> Self {
        Self::new(prev_block, last_block)
    }

    /// Get the prev_block component.
    pub fn prev_block(&self) -> Hash {
        self.prev_block
    }

    /// Get the last_block component.
    pub fn last_block(&self) -> Hash {
        self.last_block
    }
}

/// L1 block data committed by the OL ledger-ref MMR for EE DA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L1DaBlockInfo {
    /// L1 block containing this batch's DA transactions.
    pub commitment: L1BlockCommitment,

    /// Witness transaction Merkle root for the L1 block.
    pub wtxids_root: WtxidsRoot,
}

impl L1DaBlockInfo {
    pub fn new(commitment: L1BlockCommitment, wtxids_root: WtxidsRoot) -> Self {
        Self {
            commitment,
            wtxids_root,
        }
    }

    pub fn height(&self) -> L1Height {
        self.commitment.height()
    }

    pub fn blkid(&self) -> &L1BlockId {
        self.commitment.blkid()
    }

    pub fn wtxids_root(&self) -> &WtxidsRoot {
        &self.wtxids_root
    }
}

impl fmt::Display for L1DaBlockInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} wtxids_root={}", self.commitment, self.wtxids_root)
    }
}

/// EE DA txs for a specific batch that confirmed in a single L1 block.
///
/// A batch's DA is published as one commit tx plus one reveal tx per chunk.
/// The commit tx carries the DA marker in output 0 and funds each reveal from
/// subsequent P2TR outputs. Reveal txs spend those commit outputs and carry DA
/// chunk bytes in their tapscripts.
///
/// If a batch's commit and reveal txs confirm across different Bitcoin blocks,
/// that batch has multiple [`L1DaBlockRef`] values: one per L1 block that
/// contains at least one of that batch's EE DA txs. If several batches publish
/// DA txs into the same Bitcoin block, each batch still gets its own
/// [`L1DaBlockRef`] containing only the txs that belong to that batch.
#[derive(Debug, Clone)]
pub struct L1DaBlockRef {
    /// L1 block and witness-root data for the DA txs below.
    pub block: L1DaBlockInfo,
    /// This batch's DA txs confirmed in this block as `(txid, wtxid)` pairs.
    pub txns: Vec<(Txid, Wtxid)>,
}

impl L1DaBlockRef {
    pub fn new(block: L1DaBlockInfo, txns: Vec<(Txid, Wtxid)>) -> Self {
        Self { block, txns }
    }
}

/// Formats `(txid, wtxid)` pairs as a compact comma-separated list for logs.
///
/// This is kept local to the module because it only supports [`L1DaBlockRef`]'s
/// [`fmt::Display`] output.
struct DisplayTxPairs<'a>(&'a [(Txid, Wtxid)]);

impl fmt::Display for DisplayTxPairs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;

        for (idx, (txid, wtxid)) in self.0.iter().enumerate() {
            if idx > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{txid}/{wtxid}")?;
        }

        f.write_str("]")
    }
}

impl fmt::Display for L1DaBlockRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} txns={}", self.block, DisplayTxPairs(&self.txns))
    }
}

/// Batch lifecycle states
#[derive(Debug, Clone)]
pub enum BatchStatus {
    /// Genesis batch.
    Genesis,
    /// Newly sealed batch.
    Sealed,
    /// DA txn(s) posted, waiting for inclusion in block.
    DaPending { envelope_idx: u64 },
    /// DA txn(s) included in block(s).
    DaComplete { da: Vec<L1DaBlockRef> },
    /// Proving started, waiting for proof generation.
    ProofPending { da: Vec<L1DaBlockRef> },
    /// Proof ready. Update ready to be posted to OL.
    ProofReady {
        da: Vec<L1DaBlockRef>,
        proof: ProofId,
    },
}

/// Represents a sequence of blocks that are treated as a unit for DA and posting updates to OL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Sequential batch index.
    idx: u64,
    /// Last block of (idx - 1)th batch.
    prev_block: Hash,
    /// Last block in this batch.
    last_block: Hash,
    /// Blocknum of last block in this batch
    last_blocknum: u64,
    /// Rest of the blocks in this batch, cached here for easier processing.
    inner_blocks: Vec<Hash>,
    /// The account update VK this batch must be proven under.
    ///
    /// Stamped at seal time by the batch builder — the component that
    /// observes VK-update messages in the inbox ordering — so the prover
    /// never has to re-derive which key a batch needs.
    update_vk: PredicateKey,
    /// The update VK in effect for the batch after this one.
    ///
    /// Differs from `update_vk` exactly when this batch consumed a VK-update
    /// message: the consuming batch is the last one proven under the old
    /// key. Persisting it lets a restarted builder resume the current key
    /// from the latest sealed batch without rescanning block records.
    next_update_vk: PredicateKey,
}

impl Batch {
    /// Create a new batch.
    pub fn new(
        idx: u64,
        prev_block: Hash,
        last_block: Hash,
        last_blocknum: u64,
        inner_blocks: Vec<Hash>,
        update_vk: PredicateKey,
        next_update_vk: PredicateKey,
    ) -> Result<Self, &'static str> {
        if idx == 0 {
            return Err("non-genesis batch cannot have idx == 0");
        }
        if prev_block.is_zero() {
            return Err("non-genesis batch cannot have ZERO prev_block");
        }
        if last_block.is_zero() {
            return Err("batch cannot have ZERO last_block");
        }
        if prev_block == last_block {
            return Err("batch cannot be empty");
        }
        Ok(Self {
            idx,
            prev_block,
            last_block,
            last_blocknum,
            inner_blocks,
            update_vk,
            next_update_vk,
        })
    }

    /// Create genesis batch.
    ///
    /// Genesis batch is a special marker, which must always exist in storage, defined as a batch
    /// with idx == 0 AND prev_block == ZERO and last_block == genesis block. A genesis batch must
    /// always exist in storage. This is mainly to make reorg related operations simpler.
    pub fn new_genesis_batch(
        genesis_hash: Hash,
        genesis_blocknum: u64,
        genesis_update_vk: PredicateKey,
    ) -> Result<Self, &'static str> {
        if genesis_hash.is_zero() {
            return Err("genesis block cannot be ZERO");
        }

        Ok(Self {
            idx: 0,
            prev_block: Hash::zero(),
            last_block: genesis_hash,
            last_blocknum: genesis_blocknum,
            inner_blocks: Vec::new(),
            update_vk: genesis_update_vk.clone(),
            next_update_vk: genesis_update_vk,
        })
    }

    pub fn is_genesis_batch(&self) -> bool {
        self.idx() == 0
    }

    /// Get deterministic id.
    pub fn id(&self) -> BatchId {
        BatchId::new(self.prev_block, self.last_block)
    }

    /// Get sequential index.
    pub fn idx(&self) -> u64 {
        self.idx
    }

    // NOTE: Currently, sequence no = batch index - 1. This may change in the future.
    /// Returns the OL snark-account update sequence number for this batch.
    ///
    /// Genesis is stored as batch index 0 and has no update. The first real
    /// batch has index 1 and maps to update sequence number 0.
    pub fn update_seq_no(&self) -> Option<u64> {
        self.idx.checked_sub(1)
    }

    /// last block of the previous batch.
    pub fn prev_block(&self) -> Hash {
        self.prev_block
    }

    /// last block of this batch.
    pub fn last_block(&self) -> Hash {
        self.last_block
    }

    pub fn last_blocknum(&self) -> u64 {
        self.last_blocknum
    }

    pub fn last_blocknumhash(&self) -> BlockNumHash {
        BlockNumHash::new(self.last_block(), self.last_blocknum())
    }

    /// Get the inner blocks (blocks between prev_block and last_block, exclusive of last_block).
    pub fn inner_blocks(&self) -> &[Hash] {
        &self.inner_blocks
    }

    /// The account update VK this batch must be proven under.
    pub fn update_vk(&self) -> &PredicateKey {
        &self.update_vk
    }

    /// The update VK in effect for the batch after this one.
    pub fn next_update_vk(&self) -> &PredicateKey {
        &self.next_update_vk
    }

    /// Iterate over all blocks in range of this batch.
    pub fn blocks_iter(&self) -> impl Iterator<Item = Hash> + '_ {
        self.inner_blocks
            .iter()
            .copied()
            .chain(iter::once(self.last_block()))
    }
}
