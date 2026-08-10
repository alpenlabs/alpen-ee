//! MDBX table definitions for the EE node database.
//!
//! Reuses the same `DB*` wrapper types and borsh encoding as the previous sled
//! schema, one-for-one. Integer-keyed tables use
//! big-endian keys so MDBX's lexicographic cursor order matches numeric order
//! (relied on by `first`/`last`/range queries).

use alpen_db_store_mdbx::{define_table_be_key, define_table_borsh, tables, TableSpec};
use alpen_ee_common::AccessedStateRecord;
use strata_acct_types::Hash;
use strata_db_types::{chunked_envelope::ChunkedEnvelopeEntry, l1_broadcast::L1TxEntry};
use strata_identifiers::Buf32;
use strata_paas::TaskRecordData;
use zkaleido::ProofReceiptWithMetadata;

use crate::serialization_types::{
    DBAccountStateAtEpoch, DBBatchId, DBBatchWithStatus, DBChunkId, DBChunkWithStatus,
    DBExecBlockRecord, DBOLBlockId,
};

define_table_be_key! {
    /// Canonical final OL block id at OL epoch.
    (OLBlockAtEpochSchema) u32 => DBOLBlockId
}

define_table_borsh! {
    /// EE account state at a specific OL block.
    (AccountStateAtOLEpochSchema) DBOLBlockId => DBAccountStateAtEpoch
}

define_table_borsh! {
    /// Exec block by block hash.
    (ExecBlockSchema) Hash => DBExecBlockRecord
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
    (ExecBlockPayloadSchema) Hash => Vec<u8>
}

define_table_be_key! {
    /// Batch by sequential idx to `(Batch, Status)`.
    (BatchByIdxSchema) u64 => DBBatchWithStatus
}

define_table_borsh! {
    /// `BatchId` to idx lookup.
    (BatchIdToIdxSchema) DBBatchId => u64
}

define_table_be_key! {
    /// Chunk by sequential idx to `(Chunk, Status)`.
    (ChunkByIdxSchema) u64 => DBChunkWithStatus
}

define_table_borsh! {
    /// `ChunkId` to idx lookup.
    (ChunkIdToIdxSchema) DBChunkId => u64
}

define_table_borsh! {
    /// Batch-to-chunks association.
    (BatchChunksSchema) DBBatchId => Vec<DBChunkId>
}

define_table_borsh! {
    /// Per-block accessed-state record, keyed by execution block hash.
    (BlockAccessedStateSchema) Hash => AccessedStateRecord
}

define_table_borsh! {
    /// Content-addressed bytecode cache, keyed by code hash.
    (BytecodeSchema) Hash => Vec<u8>
}

define_table_borsh! {
    /// Per-block proof-witness (codec-encoded `EvmPartialState`), keyed by
    /// execution block hash.
    (BlockWitnessSchema) Hash => Vec<u8>
}

// --- Prover-side tables (shared task store + proof receipts) ---

define_table_borsh! {
    /// Shared prover task store, keyed by tag-prefixed `ProofSpec::Task` bytes.
    (ProverTaskSchema) Vec<u8> => TaskRecordData
}

define_table_borsh! {
    /// Chunk proof receipts, keyed by chunk task bytes.
    (ChunkProofReceiptSchema) Vec<u8> => ProofReceiptWithMetadata
}

define_table_borsh! {
    /// Acct (outer/update) proof receipts keyed by [`DBBatchId`].
    (AcctProofReceiptSchema) DBBatchId => ProofReceiptWithMetadata
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

define_table_borsh! {
    /// L1 broadcast: transaction id to its [`L1TxEntry`].
    (L1BroadcastTxSchema) Buf32 => L1TxEntry
}

define_table_be_key! {
    /// Chunked-envelope entry by sequential index.
    (L1ChunkedEnvelopeSchema) u64 => ChunkedEnvelopeEntry
}

/// The full set of tables backing the EE node database, for
/// [`MdbxEnv::open`](alpen_db_store_mdbx::MdbxEnv::open).
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
        L1ChunkedEnvelopeSchema,
    ]
}
