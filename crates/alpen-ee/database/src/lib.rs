//! Database implementation for Alpen execution environment.

pub mod database;
pub mod error;
mod init;
mod instrumentation;
mod mdbxdb;
mod serialization_types;
mod storage;

pub use error::{DbError, DbResult};
#[cfg(feature = "test-utils")]
pub use init::open_da_ops;
pub use init::{open_ee_db, BroadcastDbOps, ChunkedEnvelopeOps, EeDb, SequencerDatabases};
pub use mdbxdb::{BatchTaskKey, ChunkTaskKey, EeNodeDbMdbx, EeProverDbMdbx, ProverTaskKey};
pub use storage::EeNodeStorage;
