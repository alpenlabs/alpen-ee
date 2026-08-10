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
//! serialize on the environment write-lock) with unlimited MVCC readers. All
//! access goes through [`MdbxEnv::view`] (read) and [`MdbxEnv::update`] (write)
//! closures, which scope a transaction to a single call — the discipline that
//! keeps writes short and readers from stalling page reclamation.
//!
//! # Defining tables
//!
//! Table markers are crate-private, so define them inside your crate:
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

// These crates are used only inside exported macros, whose bodies expand in
// downstream crates, so they look unused from within this crate.
use bincode as _;
use borsh as _;
use strata_codec as _;

mod codec;
mod config;
mod env;
mod macros;

#[cfg(test)]
mod tests;

pub use codec::{BoxError, CodecError, KeyCodec, Schema, ValueCodec};
pub use config::{MdbxConfig, MdbxSyncMode, GIB, TIB};
pub use env::{MdbxEnv, Reader, TableSpec, Writer};
pub use error::{DbError, DbResult};

mod error;
