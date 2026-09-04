//! MDBX-backed state-diff / DA-context store.
//!
//! Backed by a single [`alpen_db_store_mdbx::MdbxEnv`].

mod db;
mod schema;

pub use db::{EeDaContextDbMdbx, WitnessDbMdbx};
pub use schema::witness_tables;
