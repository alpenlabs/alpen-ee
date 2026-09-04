//! MDBX-backed implementation of the EE node database.
//!
//! Backed by [`alpen_db_store_mdbx::MdbxEnv`].
//! Because MDBX serializes writers, each multi-table operation is one atomic
//! `update` closure — no optimistic-retry loops or in-transaction race
//! re-checks are needed.

use alpen_db_store_mdbx::DbError as MdbxError;
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
