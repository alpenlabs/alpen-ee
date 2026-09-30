//! Typed keys for the two prover task tables.
//!
//! Every resident prover program runs its own paas `Prover`, and paas's
//! tick/recovery loop re-spawns whatever unfinished task it finds in its
//! store. A task therefore has to be owned by the spec version whose prover
//! submitted it, or one version's loop could claim and sign a task meant
//! for another. The owning version is part of the key, ahead of the range,
//! so a version's tasks sit together in cursor order.
//!
//! On disk a key is `[spec_version: u16 BE][prev_block][last_block]`,
//! 66 bytes. The paas-facing form is the bare 64-byte range from
//! [`encode_chunk_task_key`] / [`encode_batch_task_key`]; the version is
//! added and removed at the storage boundary.

use alpen_ee_common::{
    decode_batch_task_key, decode_chunk_task_key, encode_batch_task_key, encode_chunk_task_key,
    BatchId, ChunkId, ProverTaskKeyDecodeError, RANGE_TASK_KEY_BYTES,
};
use alpen_ee_params::AlpenSpecId;
use alpen_store_mdbx::{CodecError, DbResult, Reader, Writer};
use strata_acct_types::Hash;
use strata_paas::TaskRecordData;

use super::schema::{AcctProverTaskSchema, ChunkProverTaskSchema};

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
}

mod sealed {
    use alpen_store_mdbx::{DbResult, Reader, Writer};
    use strata_paas::TaskRecordData;

    /// The table operations behind a key type, so the table itself stays
    /// crate-private. Private so the set of task tables is closed.
    pub trait Sealed: Sized {
        fn get(reader: &Reader<'_>, key: &Self) -> DbResult<Option<TaskRecordData>>;
        fn get_in_write(writer: &Writer<'_>, key: &Self) -> DbResult<Option<TaskRecordData>>;
        fn put(writer: &Writer<'_>, key: &Self, record: &TaskRecordData) -> DbResult<()>;
        fn delete(writer: &Writer<'_>, key: &Self) -> DbResult<bool>;
        fn for_each(
            reader: &Reader<'_>,
            f: impl FnMut(Self, TaskRecordData) -> DbResult<()>,
        ) -> DbResult<()>;
    }
}

/// A key of one of the prover task tables.
///
/// Implemented by [`ChunkTaskKey`] and [`BatchTaskKey`] only; each reads and
/// writes its own table, so the typed accessors on
/// [`EeProverDbMdbx`](super::EeProverDbMdbx) cannot cross them.
pub trait ProverTaskKey: sealed::Sealed + Copy + Send + Sync + 'static {
    /// The spec version whose prover owns the task.
    fn spec_version(&self) -> AlpenSpecId;

    /// Adds `spec_version` to a paas task key (the bare range bytes).
    fn from_task_bytes(
        spec_version: AlpenSpecId,
        bytes: &[u8],
    ) -> Result<Self, ProverTaskKeyDecodeError>;

    /// The paas task key: the bare range bytes, without the version.
    fn task_bytes(&self) -> Vec<u8>;
}

impl sealed::Sealed for ChunkTaskKey {
    fn get(reader: &Reader<'_>, key: &Self) -> DbResult<Option<TaskRecordData>> {
        reader.get::<ChunkProverTaskSchema>(key)
    }

    fn get_in_write(writer: &Writer<'_>, key: &Self) -> DbResult<Option<TaskRecordData>> {
        writer.get::<ChunkProverTaskSchema>(key)
    }

    fn put(writer: &Writer<'_>, key: &Self, record: &TaskRecordData) -> DbResult<()> {
        writer.put::<ChunkProverTaskSchema>(key, record)
    }

    fn delete(writer: &Writer<'_>, key: &Self) -> DbResult<bool> {
        writer.delete::<ChunkProverTaskSchema>(key)
    }

    fn for_each(
        reader: &Reader<'_>,
        f: impl FnMut(Self, TaskRecordData) -> DbResult<()>,
    ) -> DbResult<()> {
        reader.for_each::<ChunkProverTaskSchema>(f)
    }
}

impl ProverTaskKey for ChunkTaskKey {
    fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }

    fn from_task_bytes(
        spec_version: AlpenSpecId,
        bytes: &[u8],
    ) -> Result<Self, ProverTaskKeyDecodeError> {
        decode_chunk_task_key(bytes).map(|chunk_id| Self::new(spec_version, chunk_id))
    }

    fn task_bytes(&self) -> Vec<u8> {
        encode_chunk_task_key(self.chunk_id)
    }
}

impl sealed::Sealed for BatchTaskKey {
    fn get(reader: &Reader<'_>, key: &Self) -> DbResult<Option<TaskRecordData>> {
        reader.get::<AcctProverTaskSchema>(key)
    }

    fn get_in_write(writer: &Writer<'_>, key: &Self) -> DbResult<Option<TaskRecordData>> {
        writer.get::<AcctProverTaskSchema>(key)
    }

    fn put(writer: &Writer<'_>, key: &Self, record: &TaskRecordData) -> DbResult<()> {
        writer.put::<AcctProverTaskSchema>(key, record)
    }

    fn delete(writer: &Writer<'_>, key: &Self) -> DbResult<bool> {
        writer.delete::<AcctProverTaskSchema>(key)
    }

    fn for_each(
        reader: &Reader<'_>,
        f: impl FnMut(Self, TaskRecordData) -> DbResult<()>,
    ) -> DbResult<()> {
        reader.for_each::<AcctProverTaskSchema>(f)
    }
}

impl ProverTaskKey for BatchTaskKey {
    fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }

    fn from_task_bytes(
        spec_version: AlpenSpecId,
        bytes: &[u8],
    ) -> Result<Self, ProverTaskKeyDecodeError> {
        decode_batch_task_key(bytes).map(|batch_id| Self::new(spec_version, batch_id))
    }

    fn task_bytes(&self) -> Vec<u8> {
        encode_batch_task_key(self.batch_id)
    }
}

/// Bytes of a stored task key: the version, big-endian, then the range.
const VERSIONED_RANGE_KEY_BYTES: usize = 2 + RANGE_TASK_KEY_BYTES;

pub(super) fn encode_versioned_range(
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

pub(super) fn decode_versioned_range(
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
    use alpen_store_mdbx::KeyCodec;

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
