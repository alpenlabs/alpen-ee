//! Database serialization types for Batch and Chunk storage.
//!
//! Every type here also derives serde, which the operator console reflects
//! through; serde is not the storage codec, so the on-disk borsh encoding is
//! untouched. Fixed 32-byte arrays are marked hex so they reflect as one
//! string rather than a list of integers.

use alpen_ee_common::{
    Batch, BatchId, BatchStatus, Chunk, ChunkId, ChunkStatus, L1DaBlockInfo, L1DaBlockRef, ProofId,
};
use alpen_ee_params::AlpenSpecId;
use bitcoin::{hashes::Hash as _, Txid, Wtxid};
use borsh::{io, BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use strata_acct_types::Hash;
use strata_identifiers::{Buf32, L1BlockCommitment, WtxidsRoot};

use super::hex_list;

/// Database representation of a (Txid, Wtxid) pair.
///
/// Uses named fields to avoid confusion between the two identically-typed 32-byte arrays.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
pub(crate) struct DBTxidPair {
    #[serde(with = "hex::serde")]
    txid: [u8; 32],
    #[serde(with = "hex::serde")]
    wtxid: [u8; 32],
}

impl DBTxidPair {
    fn new(txid: [u8; 32], wtxid: [u8; 32]) -> Self {
        Self { txid, wtxid }
    }

    fn into_parts(self) -> ([u8; 32], [u8; 32]) {
        (self.txid, self.wtxid)
    }
}

/// Database representation of a BatchId.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub(crate) struct DBBatchId {
    // Hex rather than a 32-element byte list wherever this type is reflected
    // (the console); serde is not this type's storage codec, so the on-disk
    // borsh encoding is untouched.
    #[serde(with = "hex::serde")]
    prev_block: [u8; 32],
    #[serde(with = "hex::serde")]
    last_block: [u8; 32],
}

impl From<BatchId> for DBBatchId {
    fn from(value: BatchId) -> Self {
        Self {
            prev_block: value.prev_block().into(),
            last_block: value.last_block().into(),
        }
    }
}

impl From<DBBatchId> for BatchId {
    fn from(value: DBBatchId) -> Self {
        BatchId::from_parts(Hash::from(value.prev_block), Hash::from(value.last_block))
    }
}

/// Database representation of a Batch.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
pub(crate) struct DBBatch {
    idx: u64,
    #[serde(with = "hex::serde")]
    prev_block: [u8; 32],
    #[serde(with = "hex::serde")]
    last_block: [u8; 32],
    last_blocknum: u64,
    #[serde(with = "hex_list")]
    inner_blocks: Vec<[u8; 32]>,
    /// `AlpenSpecId` discriminant, stored raw since `AlpenSpecId` doesn't
    /// derive Borsh (de)serialization.
    spec_version: u16,
}

impl From<Batch> for DBBatch {
    fn from(value: Batch) -> Self {
        Self {
            idx: value.idx(),
            prev_block: value.prev_block().into(),
            last_block: value.last_block().into(),
            last_blocknum: value.last_blocknum(),
            inner_blocks: value.inner_blocks().iter().map(|h| (*h).into()).collect(),
            spec_version: value.spec_version().into(),
        }
    }
}

impl TryFrom<DBBatch> for Batch {
    type Error = &'static str;

    /// Converts a database batch into a domain batch.
    ///
    /// Note: The return type is `Result` because `Batch::new` and `Batch::new_genesis_batch`
    /// already return `Result<Batch, &'static str>`, which is propagated directly here.
    fn try_from(value: DBBatch) -> Result<Self, Self::Error> {
        let inner_blocks: Vec<Hash> = value.inner_blocks.into_iter().map(Hash::from).collect();

        if value.idx == 0 {
            Batch::new_genesis_batch(Hash::from(value.last_block), value.last_blocknum)
        } else {
            let spec_version = AlpenSpecId::try_from(value.spec_version)
                .map_err(|_| "unknown AlpenSpecId discriminant in stored batch")?;
            Batch::new(
                value.idx,
                Hash::from(value.prev_block),
                Hash::from(value.last_block),
                value.last_blocknum,
                inner_blocks,
                spec_version,
            )
        }
    }
}

/// Database representation of L1DaBlockRef.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
pub(crate) struct DBL1DaBlockRef {
    /// L1BlockCommitment serialized via its Borsh impl.
    block: L1BlockCommitment,
    /// Witness transaction Merkle root for the L1 block.
    #[serde(with = "hex::serde")]
    wtxids_root: [u8; 32],
    /// This batch's DA txs in this L1 block as raw `(txid, wtxid)` pairs.
    txns: Vec<DBTxidPair>,
}

impl From<L1DaBlockRef> for DBL1DaBlockRef {
    fn from(value: L1DaBlockRef) -> Self {
        Self {
            block: value.block.commitment,
            wtxids_root: value.block.wtxids_root().as_ref().to_owned(),
            txns: value
                .txns
                .into_iter()
                .map(|(txid, wtxid)| DBTxidPair::new(txid.to_byte_array(), wtxid.to_byte_array()))
                .collect(),
        }
    }
}

impl From<DBL1DaBlockRef> for L1DaBlockRef {
    fn from(value: DBL1DaBlockRef) -> Self {
        Self {
            block: L1DaBlockInfo::new(
                value.block,
                WtxidsRoot::from(Buf32::from(value.wtxids_root)),
            ),
            txns: value
                .txns
                .into_iter()
                .map(|pair| {
                    let (txid_bytes, wtxid_bytes) = pair.into_parts();
                    (
                        Txid::from_byte_array(txid_bytes),
                        Wtxid::from_byte_array(wtxid_bytes),
                    )
                })
                .collect(),
        }
    }
}

/// Database representation of BatchStatus.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
pub(crate) enum DBBatchStatus {
    Genesis,
    Sealed,
    DaPending {
        envelope_idx: u64,
    },
    DaComplete {
        da: Vec<DBL1DaBlockRef>,
    },
    ProofPending {
        da: Vec<DBL1DaBlockRef>,
    },
    ProofReady {
        da: Vec<DBL1DaBlockRef>,
        #[serde(with = "hex::serde")]
        proof: [u8; 32],
    },
}

impl From<BatchStatus> for DBBatchStatus {
    fn from(value: BatchStatus) -> Self {
        match value {
            BatchStatus::Genesis => Self::Genesis,
            BatchStatus::Sealed => Self::Sealed,
            BatchStatus::DaPending { envelope_idx } => Self::DaPending { envelope_idx },
            BatchStatus::DaComplete { da } => Self::DaComplete {
                da: da.into_iter().map(Into::into).collect(),
            },
            BatchStatus::ProofPending { da } => Self::ProofPending {
                da: da.into_iter().map(Into::into).collect(),
            },
            BatchStatus::ProofReady { da, proof } => Self::ProofReady {
                da: da.into_iter().map(Into::into).collect(),
                proof: proof.into(),
            },
        }
    }
}

impl From<DBBatchStatus> for BatchStatus {
    fn from(value: DBBatchStatus) -> Self {
        match value {
            DBBatchStatus::Genesis => Self::Genesis,
            DBBatchStatus::Sealed => Self::Sealed,
            DBBatchStatus::DaPending { envelope_idx } => Self::DaPending { envelope_idx },
            DBBatchStatus::DaComplete { da } => Self::DaComplete {
                da: da.into_iter().map(Into::into).collect(),
            },
            DBBatchStatus::ProofPending { da } => Self::ProofPending {
                da: da.into_iter().map(Into::into).collect(),
            },
            DBBatchStatus::ProofReady { da, proof } => Self::ProofReady {
                da: da.into_iter().map(Into::into).collect(),
                proof: ProofId::from(proof),
            },
        }
    }
}

/// Database representation of a Batch with its status, stored together.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
pub(crate) struct DBBatchWithStatus {
    batch: DBBatch,
    status: DBBatchStatus,
}

impl DBBatchWithStatus {
    pub(crate) fn new(batch: Batch, status: BatchStatus) -> Self {
        Self {
            batch: batch.into(),
            status: status.into(),
        }
    }

    pub(crate) fn into_parts(self) -> Result<(Batch, BatchStatus), &'static str> {
        let batch = self.batch.try_into()?;
        let status = self.status.into();
        Ok((batch, status))
    }
}

/// Database representation of a ChunkId.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub(crate) struct DBChunkId {
    // Hex rather than a 32-element byte list wherever this type is reflected
    // (the console); serde is not this type's storage codec, so the on-disk
    // borsh encoding is untouched.
    #[serde(with = "hex::serde")]
    prev_block: [u8; 32],
    #[serde(with = "hex::serde")]
    last_block: [u8; 32],
}

impl From<ChunkId> for DBChunkId {
    fn from(value: ChunkId) -> Self {
        Self {
            prev_block: value.prev_block().into(),
            last_block: value.last_block().into(),
        }
    }
}

impl From<DBChunkId> for ChunkId {
    fn from(value: DBChunkId) -> Self {
        ChunkId::from_parts(Hash::from(value.prev_block), Hash::from(value.last_block))
    }
}

/// Database representation of a Chunk.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
pub(crate) struct DBChunk {
    idx: u64,
    #[serde(with = "hex::serde")]
    prev_block: [u8; 32],
    #[serde(with = "hex::serde")]
    last_block: [u8; 32],
    last_blocknum: u64,
    batch_idx: u64,
    #[serde(with = "hex_list")]
    inner_blocks: Vec<[u8; 32]>,
}

impl From<Chunk> for DBChunk {
    fn from(value: Chunk) -> Self {
        Self {
            idx: value.idx(),
            prev_block: value.prev_block().into(),
            last_block: value.last_block().into(),
            last_blocknum: value.last_blocknum(),
            batch_idx: value.batch_idx(),
            inner_blocks: value.inner_blocks().iter().map(|h| (*h).into()).collect(),
        }
    }
}

impl From<DBChunk> for Chunk {
    fn from(value: DBChunk) -> Self {
        let inner_blocks: Vec<Hash> = value.inner_blocks.into_iter().map(Hash::from).collect();
        Chunk::new(
            value.idx,
            Hash::from(value.prev_block),
            Hash::from(value.last_block),
            value.last_blocknum,
            value.batch_idx,
            inner_blocks,
        )
    }
}

/// Database representation of ChunkStatus.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
pub(crate) enum DBChunkStatus {
    ProvingNotStarted,
    ProofPending(String),
    ProofReady(#[serde(with = "hex::serde")] [u8; 32]),
}

impl From<ChunkStatus> for DBChunkStatus {
    fn from(value: ChunkStatus) -> Self {
        match value {
            ChunkStatus::ProvingNotStarted => Self::ProvingNotStarted,
            ChunkStatus::ProofPending(s) => Self::ProofPending(s),
            ChunkStatus::ProofReady(proof) => Self::ProofReady(proof.into()),
        }
    }
}

impl From<DBChunkStatus> for ChunkStatus {
    fn from(value: DBChunkStatus) -> Self {
        match value {
            DBChunkStatus::ProvingNotStarted => Self::ProvingNotStarted,
            DBChunkStatus::ProofPending(s) => Self::ProofPending(s),
            DBChunkStatus::ProofReady(proof) => Self::ProofReady(ProofId::from(proof)),
        }
    }
}

/// Database representation of a Chunk with its status, stored together.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
pub(crate) struct DBChunkWithStatus {
    chunk: DBChunk,
    status: DBChunkStatus,
}

impl DBChunkWithStatus {
    pub(crate) fn new(chunk: Chunk, status: ChunkStatus) -> Self {
        Self {
            chunk: chunk.into(),
            status: status.into(),
        }
    }

    pub(crate) fn into_parts(self) -> (Chunk, ChunkStatus) {
        let chunk = self.chunk.into();
        let status = self.status.into();
        (chunk, status)
    }
}

/// The batch as the sled binary (alpen 0.3.0) stored it: every field of
/// [`DBBatch`] except `spec_version`, which did not exist.
#[cfg(feature = "console")]
#[derive(BorshSerialize, BorshDeserialize)]
struct SledEraBatch {
    idx: u64,
    prev_block: [u8; 32],
    last_block: [u8; 32],
    last_blocknum: u64,
    inner_blocks: Vec<[u8; 32]>,
}

/// [`DBBatchWithStatus`] in the sled binary's layout; the status is unchanged.
#[cfg(feature = "console")]
#[derive(BorshSerialize, BorshDeserialize)]
struct SledEraBatchWithStatus {
    batch: SledEraBatch,
    status: DBBatchStatus,
}

#[cfg(feature = "console")]
impl DBBatchWithStatus {
    /// Decodes the sled binary's layout, giving the batch the spec version in
    /// force when it was written, [`AlpenSpecId::V0`].
    pub(crate) fn from_sled_era(bytes: &[u8]) -> io::Result<Self> {
        let old = SledEraBatchWithStatus::try_from_slice(bytes)?;
        Ok(Self {
            batch: DBBatch {
                idx: old.batch.idx,
                prev_block: old.batch.prev_block,
                last_block: old.batch.last_block,
                last_blocknum: old.batch.last_blocknum,
                inner_blocks: old.batch.inner_blocks,
                spec_version: u16::from(AlpenSpecId::V0),
            },
            status: old.status,
        })
    }

    /// Encodes in the sled binary's layout; `None` when the batch carries a
    /// spec version that layout could not express.
    pub(crate) fn to_sled_era(&self) -> Option<Vec<u8>> {
        if self.batch.spec_version != u16::from(AlpenSpecId::V0) {
            return None;
        }
        let old = SledEraBatchWithStatus {
            batch: SledEraBatch {
                idx: self.batch.idx,
                prev_block: self.batch.prev_block,
                last_block: self.batch.last_block,
                last_blocknum: self.batch.last_blocknum,
                inner_blocks: self.batch.inner_blocks.clone(),
            },
            status: self.status.clone(),
        };
        borsh::to_vec(&old).ok()
    }
}
