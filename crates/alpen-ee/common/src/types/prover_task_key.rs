//! Shared EE prover task-key encoding.
//!
//! A paas task key is the task's `(prev_block, last_block)` range as 64 raw
//! bytes. Chunk and acct tasks live in separate tables, so the encoding
//! carries no kind tag: the table a key is read from says what it is.

use std::fmt;

use strata_acct_types::Hash;

use super::{batch::BatchId, chunk::ChunkId};

/// The `(prev_block, last_block)` range, 32 bytes each.
pub const RANGE_TASK_KEY_BYTES: usize = 32 + 32;

/// Encodes an EE chunk-prover task key.
pub fn encode_chunk_task_key(chunk_id: ChunkId) -> Vec<u8> {
    range_key(chunk_id.prev_block(), chunk_id.last_block())
}

/// Encodes an EE acct-prover task key.
pub fn encode_batch_task_key(batch_id: BatchId) -> Vec<u8> {
    range_key(batch_id.prev_block(), batch_id.last_block())
}

/// Decodes an EE chunk-prover task key.
pub fn decode_chunk_task_key(bytes: &[u8]) -> Result<ChunkId, ProverTaskKeyDecodeError> {
    let (prev_block, last_block) = decode_range_key(ProverTaskKeyKind::Chunk, bytes)?;
    Ok(ChunkId::from_parts(prev_block, last_block))
}

/// Decodes an EE acct-prover task key.
pub fn decode_batch_task_key(bytes: &[u8]) -> Result<BatchId, ProverTaskKeyDecodeError> {
    let (prev_block, last_block) = decode_range_key(ProverTaskKeyKind::Batch, bytes)?;
    Ok(BatchId::from_parts(prev_block, last_block))
}

/// EE prover task-key kind, named in decode errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProverTaskKeyKind {
    /// Chunk proof task key.
    Chunk,
    /// Account proof task key.
    Batch,
}

impl fmt::Display for ProverTaskKeyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chunk => f.write_str("ChunkTask"),
            Self::Batch => f.write_str("BatchTask"),
        }
    }
}

/// Error returned when decoding an EE prover task key.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProverTaskKeyDecodeError {
    /// The key length did not match the fixed range-key size.
    #[error("invalid {kind} byte length: expected {expected}, got {actual}")]
    InvalidLength {
        /// Task-key kind being decoded.
        kind: ProverTaskKeyKind,
        /// Expected byte length.
        expected: usize,
        /// Actual byte length.
        actual: usize,
    },
}

fn range_key(prev_block: Hash, last_block: Hash) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RANGE_TASK_KEY_BYTES);
    let prev: [u8; 32] = prev_block.into();
    let last: [u8; 32] = last_block.into();
    buf.extend_from_slice(&prev);
    buf.extend_from_slice(&last);
    buf
}

fn decode_range_key(
    kind: ProverTaskKeyKind,
    bytes: &[u8],
) -> Result<(Hash, Hash), ProverTaskKeyDecodeError> {
    if bytes.len() != RANGE_TASK_KEY_BYTES {
        return Err(ProverTaskKeyDecodeError::InvalidLength {
            kind,
            expected: RANGE_TASK_KEY_BYTES,
            actual: bytes.len(),
        });
    }

    let mut prev = [0u8; 32];
    let mut last = [0u8; 32];
    prev.copy_from_slice(&bytes[..32]);
    last.copy_from_slice(&bytes[32..]);

    Ok((Hash::from(prev), Hash::from(last)))
}

#[cfg(test)]
mod tests {
    use strata_identifiers::Buf32;

    use super::*;

    fn test_hash(byte: u8) -> Hash {
        let mut bytes = [0u8; 32];
        bytes[31] = byte;
        Buf32(bytes)
    }

    #[test]
    fn chunk_and_batch_keys_are_bare_ranges() {
        let prev = test_hash(1);
        let last = test_hash(2);

        let chunk_key = encode_chunk_task_key(ChunkId::from_parts(prev, last));
        assert_eq!(chunk_key.len(), RANGE_TASK_KEY_BYTES);
        assert_eq!(&chunk_key[..32], <[u8; 32]>::from(prev).as_slice());
        assert_eq!(&chunk_key[32..], <[u8; 32]>::from(last).as_slice());

        let acct_key = encode_batch_task_key(BatchId::from_parts(prev, last));
        assert_eq!(acct_key, chunk_key);
    }

    #[test]
    fn task_keys_roundtrip() {
        let prev = test_hash(1);
        let last = test_hash(2);

        let chunk_id = ChunkId::from_parts(prev, last);
        let batch_id = BatchId::from_parts(prev, last);

        assert_eq!(
            decode_chunk_task_key(&encode_chunk_task_key(chunk_id)).unwrap(),
            chunk_id
        );
        assert_eq!(
            decode_batch_task_key(&encode_batch_task_key(batch_id)).unwrap(),
            batch_id
        );
    }

    #[test]
    fn task_key_decoder_rejects_wrong_length() {
        let err = decode_chunk_task_key(&[0u8; RANGE_TASK_KEY_BYTES + 1]).unwrap_err();

        assert_eq!(
            err,
            ProverTaskKeyDecodeError::InvalidLength {
                kind: ProverTaskKeyKind::Chunk,
                expected: RANGE_TASK_KEY_BYTES,
                actual: RANGE_TASK_KEY_BYTES + 1,
            }
        );
    }
}
