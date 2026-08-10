//! Database implementation for Alpen execution environment.

pub mod database;
pub mod error;
mod init;
mod instrumentation;
mod mdbxdb;
mod serialization_types;
mod sleddb;
mod storage;

pub use error::{DbError, DbResult};
pub use init::{open_ee_db, EeDb, SequencerDatabases};
pub use sleddb::{BroadcastDbOps, ChunkedEnvelopeOps, EeProverDbSled};
pub use mdbxdb::{EeNodeDbMdbx, EeProverDbMdbx};
pub use storage::EeNodeStorage;
