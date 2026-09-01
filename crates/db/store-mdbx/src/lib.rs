//! MDBX-backed typed table store for the Alpen codebase.
//!
//! A small, engine-focused toolkit for defining typed tables over a single
//! [`libmdbx`](signet_libmdbx) environment. It provides the schema/codec traits,
//! environment configuration, and the transaction accessors; concrete table
//! schemas and domain databases live in the crates that use it.
//!
//! # Model
//!
//! One [`MdbxEnv`] is one MDBX environment: a single writer (transactions
//! serialize on the environment write-lock) alongside MVCC readers that never
//! block it, up to the [`MdbxConfig::max_readers`] reader slots. All
//! access goes through [`MdbxEnv::view`] (read) and [`MdbxEnv::update`] (write)
//! closures, which scope a transaction to a single call — the discipline that
//! keeps writes short and readers from stalling page reclamation.
//!
//! # Defining tables
//!
//! Table markers are crate-private, so define them inside your crate. The
//! key and value types below are just examples — any Borsh-serializable type
//! works for either side (and `define_table_be_key!` additionally requires the
//! key to be a big-endian-encodable integer or byte array):
//!
//! ```
//! mod schema {
//!     use alpen_db_store_mdbx::{define_table_be_key, define_table_borsh};
//!
//!     define_table_borsh! {
//!         /// Maps a block id to its record.
//!         (BlockById) [u8; 32] => Vec<u8>
//!     }
//!
//!     define_table_be_key! {
//!         /// Canonical chain by height (numeric cursor order).
//!         (BlockAtHeight) u64 => [u8; 32]
//!     }
//! }
//! ```
//!
//! # Evolving what a table stores
//!
//! Stored formats change across binary releases, under rolling upgrades with no
//! drain and no maintenance window, so nothing here blocks startup: a value
//! carries its own version tag and is decoded through the chain of decoders this
//! binary retains (see [`version`]).
//!
//! A read never writes back, so a stored value keeps its old format until the
//! application naturally writes that key again, at which point it lands in the
//! current one. Nothing sweeps the cold keys, so every decoder is kept
//! indefinitely. A value written by a *newer* binary is refused with a typed
//! [`CodecError::NewerVersion`] rather than misread.

// These crates are used only inside exported macros, whose bodies expand in
// downstream crates, so they look unused from within this crate.
use bincode as _;
use borsh as _;
use ciborium as _;
use strata_codec as _;

mod codec;
mod config;
mod env;
mod error;
mod macros;
pub mod version;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod version_tests;

pub use codec::{BoxError, CodecError, KeyCodec, Schema, ValueCodec};
pub use config::{MdbxConfig, MdbxSyncMode, GIB, TIB};
pub use env::{MdbxEnv, Reader, TableSpec, Writer};
pub use error::{DbError, DbResult};
pub use version::{
    split_version_tag, unknown_version_error, LiftToCurrent, RawGet, SchemaVersion, UpConvert,
    UpgradeCtx, VersionedValue, MAX_UPGRADE_DEPTH,
};
