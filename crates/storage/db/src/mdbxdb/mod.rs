//! MDBX-backed implementation of the EE node database.
//!
//! Backed by [`alpen_storage_mdbx::MdbxEnv`].
//! Because MDBX serializes writers, each multi-table operation is one atomic
//! `update` closure — no optimistic-retry loops or in-transaction race
//! re-checks are needed.

use alpen_storage_mdbx::DbError as MdbxError;
use strata_db_types::errors::DbError;

mod broadcast_db;
mod db;
mod envelope_db;
mod prover_db;
pub(crate) mod schema;
mod task_key;
mod witness_db;
pub(crate) mod witness_schema;

pub(crate) use broadcast_db::L1BroadcastDbMdbx;
pub use db::EeNodeDbMdbx;
pub(crate) use envelope_db::L1ChunkedEnvelopeDbMdbx;
pub use prover_db::EeProverDbMdbx;
pub use task_key::{BatchTaskKey, ChunkTaskKey, ProverTaskKey};
pub use witness_db::{EeDaContextDbMdbx, WitnessDbMdbx};

/// Maps a storage-engine error into the database error type.
fn to_db_error(err: MdbxError) -> DbError {
    DbError::Other(format!("mdbx: {err}"))
}

// The schema modules are re-exported whole: `init` and `test_db` open
// environments by their table lists, and the crate root's `schema` module
// publishes the markers for tooling.
pub(crate) use schema::*;
pub(crate) use witness_schema::*;
