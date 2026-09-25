//! Database implementation for Alpen execution environment.

// Referenced only from `#[serde(with = "hex::serde")]` attributes, which the
// unused-crate-dependencies lint cannot see.
use hex as _;
// Only the console's reflection tests use it, but it is a dev-dependency of
// every test build.
#[cfg(all(test, not(feature = "console")))]
use proptest as _;

#[cfg(feature = "console")]
pub mod console;
pub mod database;
pub mod error;
mod init;
mod instrumentation;
mod mdbxdb;
mod serialization_types;
mod storage;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_db;

pub use error::{DbError, DbResult};
#[cfg(feature = "test-utils")]
pub use init::open_da_ops;
pub use init::{
    create_ee_envs, open_ee_db, BroadcastDbOps, ChunkedEnvelopeOps, EeDb, SequencerDatabases,
};
pub use mdbxdb::{EeNodeDbMdbx, EeProverDbMdbx};
pub use storage::EeNodeStorage;
