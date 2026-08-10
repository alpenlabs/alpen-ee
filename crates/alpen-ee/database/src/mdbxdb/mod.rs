//! MDBX-backed implementation of the EE node database.
//!
//! Backed by [`alpen_db_store_mdbx::MdbxEnv`].
//! Because MDBX serializes writers, each multi-table operation is one atomic
//! `update` closure — no optimistic-retry loops or in-transaction race
//! re-checks are needed.

mod db;
mod prover_db;
mod schema;

pub use db::EeNodeDbMdbx;
pub use prover_db::EeProverDbMdbx;
