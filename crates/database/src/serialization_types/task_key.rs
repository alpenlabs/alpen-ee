//! The keys of the two prover task tables, and the three forms one task
//! identity takes on its way between paas and the store.
//!
//! # What a task is
//!
//! A prover task names a block range: `prev_block` to `last_block`. A chunk
//! task proves the chunk over that range, an acct task the batch over it.
//! The two kinds have the same shape, and the same range can be both a chunk
//! and a batch, so a key has to carry its kind somewhere. It carries it in
//! *which table* it lives in: [`ChunkTaskKey`] is only ever stored in
//! `ChunkProverTaskSchema`, [`BatchTaskKey`] only in `AcctProverTaskSchema`,
//! and the typed accessors on the prover store cannot cross them.
//!
//! # Why the owning spec version is part of the key
//!
//! During a protocol upgrade the node runs one `strata-paas` `Prover` per
//! resident [`AlpenSpecId`], each with its own program and verifying key.
//! paas's tick and recovery loops re-spawn whatever unfinished tasks the
//! store lists, with no notion of which prover submitted them. If two
//! versions' provers shared rows, one version's loop would claim a task
//! meant for the other and prove it with the wrong key. So every row is
//! owned by a spec version, the version is the first thing in the key, and
//! the listing accessors on the store take the version whose rows to return.
//! Putting the version first also makes one version's tasks a contiguous
//! run in cursor order, which is what a prefix walk (`0000` for V0) relies
//! on.
//!
//! # The three representations
//!
//! | Form | Bytes | Who holds it |
//! | --- | --- | --- |
//! | paas task bytes | `prev ‖ last`, 64 | `strata-paas`: its `TaskStore` is keyed by opaque bytes, and `ProofSpec::Task` (`ChunkTask`, `BatchTask` in the node) converts to and from exactly this through `encode_chunk_task_key` and friends in `alpen-common`. paas never sees the version. |
//! | typed key | [`ChunkTaskKey`] / [`BatchTaskKey`] | the store's API and everything above it: the node's task-store adapter, the console. |
//! | stored key | `version(u16 BE) ‖ prev ‖ last`, 66 | the MDBX row, through the [`KeyCodec`](alpen_mdbx::KeyCodec) impls in the schema module, which call [`encode_versioned_range`] and [`decode_versioned_range`]. |
//!
//! The conversions sit where the knowledge is. [`ChunkTaskKey::from_task_bytes`]
//! and [`ChunkTaskKey::task_bytes`] (and their batch twins) add and strip the
//! version at the boundary between paas and the store; the node's task-store
//! adapter, which is instantiated once per resident version, is the one place
//! that knows which version to add. The stored form is the codec's business
//! and nothing else reads it, except the operator console, which renders a
//! key as the hex of its stored bytes so that a version's tasks share a
//! prefix.
//!
//! # Where the kind goes
//!
//! Earlier layouts kept both kinds in one table and put a kind tag byte into
//! the paas bytes themselves, which leaked storage layout into paas's key and
//! made each prover's listing trip over the other kind's rows. The kind now
//! lives only in the table choice, and the paas bytes are the bare range.

use alpen_common::{
    decode_batch_task_key, decode_chunk_task_key, encode_batch_task_key, encode_chunk_task_key,
    BatchId, ChunkId, ProverTaskKeyDecodeError, RANGE_TASK_KEY_BYTES,
};
use alpen_mdbx::CodecError;
use alpen_params::AlpenSpecId;
use strata_acct_types::Hash;

/// Key of a chunk prover task: the owning spec version and the chunk range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkTaskKey {
    spec_version: AlpenSpecId,
    chunk_id: ChunkId,
}

impl ChunkTaskKey {
    /// Builds the key of `chunk_id`'s task under `spec_version`'s prover.
    pub fn new(spec_version: AlpenSpecId, chunk_id: ChunkId) -> Self {
        Self {
            spec_version,
            chunk_id,
        }
    }

    /// The spec version whose prover owns the task.
    pub fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }

    /// The chunk this task proves.
    pub fn chunk_id(&self) -> ChunkId {
        self.chunk_id
    }

    /// The key of the task paas identifies by `bytes` (the bare range),
    /// owned by `spec_version`.
    pub fn from_task_bytes(
        spec_version: AlpenSpecId,
        bytes: &[u8],
    ) -> Result<Self, ProverTaskKeyDecodeError> {
        decode_chunk_task_key(bytes).map(|chunk_id| Self::new(spec_version, chunk_id))
    }

    /// The paas task key: the bare range bytes, without the version.
    pub fn task_bytes(&self) -> Vec<u8> {
        encode_chunk_task_key(self.chunk_id)
    }
}

/// Key of an acct prover task: the owning spec version and the batch range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BatchTaskKey {
    spec_version: AlpenSpecId,
    batch_id: BatchId,
}

impl BatchTaskKey {
    /// Builds the key of `batch_id`'s task under `spec_version`'s prover.
    pub fn new(spec_version: AlpenSpecId, batch_id: BatchId) -> Self {
        Self {
            spec_version,
            batch_id,
        }
    }

    /// The spec version whose prover owns the task.
    pub fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }

    /// The batch this task proves.
    pub fn batch_id(&self) -> BatchId {
        self.batch_id
    }

    /// The key of the task paas identifies by `bytes` (the bare range),
    /// owned by `spec_version`.
    pub fn from_task_bytes(
        spec_version: AlpenSpecId,
        bytes: &[u8],
    ) -> Result<Self, ProverTaskKeyDecodeError> {
        decode_batch_task_key(bytes).map(|batch_id| Self::new(spec_version, batch_id))
    }

    /// The paas task key: the bare range bytes, without the version.
    pub fn task_bytes(&self) -> Vec<u8> {
        encode_batch_task_key(self.batch_id)
    }
}

/// Bytes of a stored task key: the version, big-endian, then the range.
const VERSIONED_RANGE_KEY_BYTES: usize = 2 + RANGE_TASK_KEY_BYTES;

/// The stored form of a task key: `spec_version` as a big-endian `u16`, then
/// the two hashes raw, so that byte order is key order.
pub(crate) fn encode_versioned_range(
    spec_version: AlpenSpecId,
    prev_block: Hash,
    last_block: Hash,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(VERSIONED_RANGE_KEY_BYTES);
    let prev: [u8; 32] = prev_block.into();
    let last: [u8; 32] = last_block.into();
    buf.extend_from_slice(&u16::from(spec_version).to_be_bytes());
    buf.extend_from_slice(&prev);
    buf.extend_from_slice(&last);
    buf
}

/// The inverse of [`encode_versioned_range`]; `table` names the table in the
/// error. A key of another length, or of a version this binary does not
/// know, is a decode error rather than a guess.
pub(crate) fn decode_versioned_range(
    table: &'static str,
    bytes: &[u8],
) -> Result<(AlpenSpecId, Hash, Hash), CodecError> {
    if bytes.len() != VERSIONED_RANGE_KEY_BYTES {
        return Err(CodecError::decode(
            table,
            format!(
                "expected {VERSIONED_RANGE_KEY_BYTES}-byte task key, got {}",
                bytes.len()
            ),
        ));
    }
    let raw_version = u16::from_be_bytes([bytes[0], bytes[1]]);
    let spec_version = AlpenSpecId::try_from(raw_version).map_err(|raw| {
        CodecError::decode(table, format!("unknown spec version {raw} in task key"))
    })?;
    let mut prev = [0u8; 32];
    let mut last = [0u8; 32];
    prev.copy_from_slice(&bytes[2..34]);
    last.copy_from_slice(&bytes[34..]);
    Ok((spec_version, Hash::from(prev), Hash::from(last)))
}

#[cfg(test)]
mod tests {
    use alpen_mdbx::KeyCodec;

    use super::*;

    fn hash(byte: u8) -> Hash {
        let mut bytes = [0u8; 32];
        bytes[31] = byte;
        Hash::from(bytes)
    }

    #[test]
    fn stored_key_is_version_then_range() {
        let key = ChunkTaskKey::new(AlpenSpecId::V1, ChunkId::from_parts(hash(1), hash(2)));
        let bytes = key.encode_key().unwrap();
        assert_eq!(bytes.len(), VERSIONED_RANGE_KEY_BYTES);
        assert_eq!(&bytes[..2], &[0, 1]);
        assert_eq!(&bytes[2..], key.task_bytes().as_slice());
        assert_eq!(ChunkTaskKey::decode_key(&bytes).unwrap(), key);

        let key = BatchTaskKey::new(AlpenSpecId::V0, BatchId::from_parts(hash(3), hash(4)));
        let bytes = key.encode_key().unwrap();
        assert_eq!(&bytes[..2], &[0, 0]);
        assert_eq!(BatchTaskKey::decode_key(&bytes).unwrap(), key);
    }

    #[test]
    fn task_bytes_roundtrip_under_a_version() {
        let chunk = ChunkTaskKey::new(AlpenSpecId::V1, ChunkId::from_parts(hash(1), hash(2)));
        assert_eq!(
            ChunkTaskKey::from_task_bytes(AlpenSpecId::V1, &chunk.task_bytes()).unwrap(),
            chunk
        );
        let batch = BatchTaskKey::new(AlpenSpecId::V0, BatchId::from_parts(hash(1), hash(2)));
        assert_eq!(
            BatchTaskKey::from_task_bytes(AlpenSpecId::V0, &batch.task_bytes()).unwrap(),
            batch
        );
    }

    #[test]
    fn stored_key_rejects_bad_length_and_unknown_version() {
        fn reason(err: CodecError) -> String {
            match err {
                CodecError::Decode { source, .. } => source.to_string(),
                other => panic!("expected a decode error, got {other:?}"),
            }
        }

        let err = reason(ChunkTaskKey::decode_key(&[0u8; 65]).unwrap_err());
        assert!(err.contains("66-byte task key"), "{err}");

        let mut bytes = vec![0xff, 0xff];
        bytes.extend_from_slice(&[0u8; RANGE_TASK_KEY_BYTES]);
        let err = reason(BatchTaskKey::decode_key(&bytes).unwrap_err());
        assert!(err.contains("unknown spec version 65535"), "{err}");
    }
}
