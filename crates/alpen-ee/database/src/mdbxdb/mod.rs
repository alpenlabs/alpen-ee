//! MDBX-backed implementation of the EE node database.
//!
//! Backed by [`alpen_store_mdbx::MdbxEnv`].
//! Because MDBX serializes writers, each multi-table operation is one atomic
//! `update` closure — no optimistic-retry loops or in-transaction race
//! re-checks are needed.

use alpen_store_mdbx::DbError as MdbxError;
use strata_db_types::errors::DbError;

mod broadcast_db;
mod db;
mod envelope_db;
mod prover_db;
mod schema;

pub(crate) use broadcast_db::L1BroadcastDbMdbx;
pub use db::EeNodeDbMdbx;
pub(crate) use envelope_db::L1ChunkedEnvelopeDbMdbx;
pub use prover_db::EeProverDbMdbx;
pub(crate) use schema::da_tables;

/// Maps a storage-engine error into the database error type.
fn to_db_error(err: MdbxError) -> DbError {
    DbError::Other(format!("mdbx: {err}"))
}

// The prover tables, for the seeding helpers in `test_db`.
#[cfg(any(test, feature = "test-utils"))]
pub(crate) use schema::{
    prover_tables, AcctProofIdIndexSchema, AcctProofReceiptSchema, ChunkProofReceiptSchema,
    ProverTaskSchema,
};
// Re-exported for the operator console (`console` feature), which reflects
// these tables through the production codecs.
#[cfg(feature = "console")]
pub(crate) use schema::{
    AccountStateAtOLEpochSchema, BatchByIdxSchema, BatchChunksSchema, BatchIdToIdxSchema,
    BlockAccessedStateSchema, BlockWitnessSchema, BytecodeSchema, ChunkByIdxSchema,
    ChunkIdToIdxSchema, ExecBlockFinalizedSchema, ExecBlockPayloadSchema, ExecBlockSchema,
    ExecBlocksAtHeightSchema, L1BroadcastActiveTxNodeSchema, L1BroadcastTxIdSchema,
    L1BroadcastTxNodeSchema, L1BroadcastTxSchema, L1ChunkedEnvelopeSchema, OLBlockAtEpochSchema,
};
#[cfg(all(feature = "console", not(any(test, feature = "test-utils"))))]
pub(crate) use schema::{
    AcctProofIdIndexSchema, AcctProofReceiptSchema, ChunkProofReceiptSchema, ProverTaskSchema,
};
