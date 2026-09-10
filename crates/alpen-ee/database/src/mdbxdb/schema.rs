//! MDBX table definitions for the EE node database.
//!
//! Uses the `DB*` wrapper types and borsh encoding. Integer-keyed tables use
//! big-endian keys so MDBX's lexicographic cursor order matches numeric order
//! (relied on by `first`/`last`/range queries).
//!
//! # Versioned values
//!
//! A table storing a structured record names its shipped versions inline,
//! ascending, the last one current. Adding a version means adding the struct,
//! one [`UpConvert`](alpen_store_mdbx::UpConvert) impl from the previous
//! version, and one entry here; nothing else moves, and no other table's stored
//! bytes change.
//!
//! What stays untagged is deliberate: a value that is a bare identifier, an
//! index, a counter, a presence marker, or a collection of identifiers has no
//! field to add, and an opaque blob — an engine payload, EVM bytecode, an
//! encoded witness — has framing owned by whatever produced it rather than by
//! this store.

use alpen_ee_common::AccessedStateRecord;
use alpen_store_mdbx::{
    define_table, define_table_be_key, define_table_borsh, define_table_versioned,
    define_table_versioned_be_key, impl_be_key_codec, impl_cbor_value_codec, impl_raw_key_codec,
    impl_versioned_value_codec, tables, CodecError, KeyCodec, Schema, TableSpec,
};
use strata_acct_types::Hash;
use strata_db_types::{
    chunked_envelope::ChunkedEnvelopeEntry,
    fee_bump::{TxNodeId, TxNodeRecord},
    l1_broadcast::L1TxEntry,
};
use strata_identifiers::Buf32;
use strata_paas::TaskRecordData;
use zkaleido::ProofReceiptWithMetadata;

use crate::serialization_types::{
    DBAccountStateAtEpoch, DBBatchId, DBBatchWithStatus, DBChunkId, DBChunkWithStatus,
    DBExecBlockRecord, DBOLBlockId,
};

/// Raw 32-byte [`KeyCodec`] for [`TxNodeId`], which is a newtype over a hash
/// and carries no codec of its own.
macro_rules! impl_node_id_key_codec {
    ($schema:ty) => {
        impl KeyCodec<$schema> for TxNodeId {
            fn encode_key(&self) -> Result<Vec<u8>, CodecError> {
                Ok(self.0 .0.to_vec())
            }

            fn decode_key(bytes: &[u8]) -> Result<Self, CodecError> {
                let raw: [u8; 32] = bytes.try_into().map_err(|_| {
                    CodecError::decode(
                        <$schema as Schema>::NAME,
                        format!("expected 32-byte node id, got {}", bytes.len()),
                    )
                })?;
                Ok(Self(Buf32(raw)))
            }
        }
    };
}

define_table_be_key! {
    /// Canonical final OL block id at OL epoch.
    (OLBlockAtEpochSchema) u32 => DBOLBlockId
}

define_table_versioned! {
    /// EE account state at a specific OL block.
    (AccountStateAtOLEpochSchema) DBOLBlockId => {
        1 => DBAccountStateAtEpoch as borsh,
    }
}

define_table_versioned! {
    /// Exec block by block hash.
    ///
    /// Content-addressed and inserted once: `save_exec_block` skips a hash it
    /// already holds, so the record is fixed for the life of the block.
    (ExecBlockSchema) Hash => {
        1 => DBExecBlockRecord as borsh,
    }
}

define_table_be_key! {
    /// All exec block hashes at a given height (supports forks).
    (ExecBlocksAtHeightSchema) u64 => Vec<Hash>
}

define_table_be_key! {
    /// Canonical finalized chain: height to block hash.
    (ExecBlockFinalizedSchema) u64 => Hash
}

define_table_borsh! {
    /// Exec block payloads by block hash.
    ///
    /// An opaque engine payload, written alongside its exec block and never
    /// rewritten. The blob's own framing is the engine's, not this store's, so
    /// it carries no version tag.
    (ExecBlockPayloadSchema) Hash => Vec<u8>
}

define_table_versioned_be_key! {
    /// Batch by sequential idx to `(Batch, Status)`.
    (BatchByIdxSchema) u64 => {
        1 => DBBatchWithStatus as borsh,
    }
}

define_table_borsh! {
    /// `BatchId` to idx lookup.
    (BatchIdToIdxSchema) DBBatchId => u64
}

define_table_versioned_be_key! {
    /// Chunk by sequential idx to `(Chunk, Status)`.
    (ChunkByIdxSchema) u64 => {
        1 => DBChunkWithStatus as borsh,
    }
}

define_table_borsh! {
    /// `ChunkId` to idx lookup.
    (ChunkIdToIdxSchema) DBChunkId => u64
}

define_table_borsh! {
    /// Batch-to-chunks association.
    (BatchChunksSchema) DBBatchId => Vec<DBChunkId>
}

define_table_versioned! {
    /// Per-block accessed-state record, keyed by execution block hash.
    (BlockAccessedStateSchema) Hash => {
        1 => AccessedStateRecord as borsh,
    }
}

define_table_borsh! {
    /// Content-addressed bytecode cache, keyed by code hash.
    ///
    /// The key is the hash of the value, so a stored entry can never legitimately
    /// change. Bytecode has no framing of its own to version.
    (BytecodeSchema) Hash => Vec<u8>
}

define_table_borsh! {
    /// Per-block proof-witness (codec-encoded `EvmPartialState`), keyed by
    /// execution block hash.
    ///
    /// Stored as an opaque blob: the witness encoding belongs to whatever builds
    /// it, so versioning it is that producer's concern, not this table's. It
    /// stays mutable because a re-derived witness may legitimately differ once
    /// that encoding changes.
    (BlockWitnessSchema) Hash => Vec<u8>
}

// --- Prover-side tables (shared task store + proof receipts) ---

define_table! {
    /// Shared prover task store, keyed by tag-prefixed `ProofSpec::Task` bytes.
    ///
    /// The key is stored verbatim so the documented kind-tag prefixes sort as
    /// written; the record is serde-only, hence the CBOR payload.
    (ProverTaskSchema) Vec<u8> => TaskRecordData
}
impl_raw_key_codec!(ProverTaskSchema);
impl_versioned_value_codec!(ProverTaskSchema { 1 => TaskRecordData as cbor });

define_table_versioned! {
    /// Chunk proof receipts, keyed by chunk task bytes.
    (ChunkProofReceiptSchema) Vec<u8> => {
        1 => ProofReceiptWithMetadata as borsh,
    }
}

define_table_versioned! {
    /// Acct (outer/update) proof receipts keyed by [`DBBatchId`].
    (AcctProofReceiptSchema) DBBatchId => {
        1 => ProofReceiptWithMetadata as borsh,
    }
}

define_table_borsh! {
    /// Secondary index: `ProofId` to `BatchId`, so a receipt resolves without
    /// scanning.
    (AcctProofIdIndexSchema) Hash => DBBatchId
}

// --- DA-pipeline tables (L1 broadcast + chunked envelope) ---

define_table_be_key! {
    /// L1 broadcast: sequential index to transaction id.
    (L1BroadcastTxIdSchema) u64 => Buf32
}

define_table_versioned! {
    /// L1 broadcast: transaction id to its entry.
    (L1BroadcastTxSchema) Buf32 => {
        1 => L1TxEntry as cbor,
    }
}

define_table! {
    /// L1 broadcast: logical transaction replacement chains, keyed by the
    /// chain's [`TxNodeId`].
    (L1BroadcastTxNodeSchema) TxNodeId => TxNodeRecord
}
impl_node_id_key_codec!(L1BroadcastTxNodeSchema);
impl_versioned_value_codec!(L1BroadcastTxNodeSchema { 1 => TxNodeRecord as cbor });

define_table! {
    /// Presence marker: this replacement chain may still need fee bumping.
    ///
    /// The replacement pass scans this set instead of the whole node table,
    /// whose records are kept forever for crash-recovery point lookups. The
    /// value is empty, so there is nothing to version.
    (L1BroadcastActiveTxNodeSchema) TxNodeId => ()
}
impl_node_id_key_codec!(L1BroadcastActiveTxNodeSchema);
impl_cbor_value_codec!(L1BroadcastActiveTxNodeSchema, ());

define_table! {
    /// Chunked-envelope entry by sequential index.
    (L1ChunkedEnvelopeSchema) u64 => ChunkedEnvelopeEntry
}
impl_be_key_codec!(L1ChunkedEnvelopeSchema, u64);
impl_versioned_value_codec!(L1ChunkedEnvelopeSchema { 1 => ChunkedEnvelopeEntry as cbor });

/// The full set of tables backing the EE node database, for
/// [`MdbxEnv::open`](alpen_store_mdbx::MdbxEnv::open).
pub(crate) fn node_tables() -> Vec<TableSpec> {
    tables![
        OLBlockAtEpochSchema,
        AccountStateAtOLEpochSchema,
        ExecBlockSchema,
        ExecBlocksAtHeightSchema,
        ExecBlockFinalizedSchema,
        ExecBlockPayloadSchema,
        BatchByIdxSchema,
        BatchIdToIdxSchema,
        ChunkByIdxSchema,
        ChunkIdToIdxSchema,
        BatchChunksSchema,
        BlockAccessedStateSchema,
        BytecodeSchema,
        BlockWitnessSchema,
    ]
}

/// The full set of tables backing the EE prover database.
pub(crate) fn prover_tables() -> Vec<TableSpec> {
    tables![
        ProverTaskSchema,
        ChunkProofReceiptSchema,
        AcctProofReceiptSchema,
        AcctProofIdIndexSchema,
    ]
}

/// The full set of tables backing the EE DA pipeline (L1 broadcast + chunked
/// envelope).
pub(crate) fn da_tables() -> Vec<TableSpec> {
    tables![
        L1BroadcastTxIdSchema,
        L1BroadcastTxSchema,
        L1BroadcastTxNodeSchema,
        L1BroadcastActiveTxNodeSchema,
        L1ChunkedEnvelopeSchema,
    ]
}

#[cfg(test)]
mod tests {
    use alpen_store_mdbx::{CodecError, UpgradeCtx, VersionedTable};

    use super::*;

    /// Runs the chain invariants over every versioned table in this module, so a
    /// new one is covered by adding it to this list.
    macro_rules! for_each_versioned_table {
        ($check:ident) => {
            $check::<AccountStateAtOLEpochSchema>();
            $check::<ExecBlockSchema>();
            $check::<BatchByIdxSchema>();
            $check::<ChunkByIdxSchema>();
            $check::<BlockAccessedStateSchema>();
            $check::<ProverTaskSchema>();
            $check::<ChunkProofReceiptSchema>();
            $check::<AcctProofReceiptSchema>();
            $check::<L1BroadcastTxSchema>();
            $check::<L1BroadcastTxNodeSchema>();
            $check::<L1ChunkedEnvelopeSchema>();
        };
    }

    /// The declared chain must start at 1, ascend by one, and end at the version
    /// this binary writes — a gap would leave stored bytes undecodable.
    fn check_chain<S: VersionedTable>() {
        let versions = S::VERSIONS;
        let name = <S as Schema>::NAME;
        assert_eq!(
            versions.first(),
            Some(&1),
            "`{name}`: chain must start at 1"
        );
        for (position, version) in versions.iter().enumerate() {
            assert_eq!(
                *version,
                position as u8 + 1,
                "`{name}`: chain must ascend without gaps, got {versions:?}"
            );
        }
        assert_eq!(
            versions.last(),
            Some(&S::CURRENT_VERSION),
            "`{name}`: the last declared version must be the one written"
        );
    }

    /// A value written by a newer binary must be refused, not misread.
    fn check_refuses_newer<S: VersionedTable>() {
        let name = <S as Schema>::NAME;
        let newer = [S::CURRENT_VERSION + 1, 0, 0, 0, 0];
        let err = S::decode_tagged(&newer, &UpgradeCtx::detached())
            .err()
            .unwrap_or_else(|| panic!("`{name}`: a newer tag decoded"));
        assert!(
            matches!(err, CodecError::NewerVersion { .. }),
            "`{name}`: expected a newer-version refusal, got {err:?}"
        );
    }

    /// An empty value carries no tag and must be reported as such.
    fn check_reports_missing_tag<S: VersionedTable>() {
        let name = <S as Schema>::NAME;
        let err = S::decode_tagged(&[], &UpgradeCtx::detached())
            .err()
            .unwrap_or_else(|| panic!("`{name}`: empty bytes decoded"));
        assert!(
            matches!(err, CodecError::MissingVersionTag { .. }),
            "`{name}`: expected a missing-tag error, got {err:?}"
        );
    }

    #[test]
    fn every_table_declares_a_gapless_chain() {
        for_each_versioned_table!(check_chain);
    }

    #[test]
    fn every_table_refuses_a_newer_version() {
        for_each_versioned_table!(check_refuses_newer);
    }

    #[test]
    fn every_table_reports_a_missing_tag() {
        for_each_versioned_table!(check_reports_missing_tag);
    }
}
