//! MDBX-backed implementation of the EE node database.
//!
//! Backed by [`alpen_mdbx::MdbxEnv`].
//! Because MDBX serializes writers, each multi-table operation is one atomic
//! `update` closure — no optimistic-retry loops or in-transaction race
//! re-checks are needed.

use alpen_mdbx::DbError as MdbxError;
use strata_db_types::errors::DbError;

mod broadcast_db;
mod db;
mod envelope_db;
mod prover_db;
mod recovered_da_db;
pub(crate) mod schema;
mod witness_db;

pub(crate) use broadcast_db::L1BroadcastDbMdbx;
pub use db::NodeDbMdbx;
pub(crate) use envelope_db::L1ChunkedEnvelopeDbMdbx;
pub use prover_db::{ProverDbMdbx, ProverTaskKey};
pub(crate) use recovered_da_db::RecoveredDaDbMdbx;
pub use witness_db::{DaContextDbMdbx, WitnessDbMdbx};

/// Maps a storage-engine error into the database error type.
fn to_db_error(err: MdbxError) -> DbError {
    DbError::Other(format!("mdbx: {err}"))
}

// The schema module is re-exported whole: `init` and `test_db` open
// environments by their table lists, and the crate root's `schema` module
// publishes the markers for tooling.
pub(crate) use schema::*;
