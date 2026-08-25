//! MDBX table definitions for the reth state-diff / DA-context store.
//!
//! `B256`/`u64` big-endian keys and raw `Vec<u8>` values, since the stored
//! payloads are already bincode-encoded and served verbatim.

use alpen_db_store_mdbx::{define_table_raw_be_key, tables, TableSpec};
use revm_primitives::alloy_primitives::B256;

define_table_raw_be_key! {
    /// Block state-diff data, stored as serialized bytes for direct RPC serving.
    (BlockStateChangesSchema) B256 => Vec<u8>
}

define_table_raw_be_key! {
    /// Block number to hash mapping.
    (BlockHashByNumber) u64 => Vec<u8>
}

define_table_raw_be_key! {
    /// Set of contract code hashes already published to DA (presence-only).
    (PublishedCodeHashSchema) B256 => Vec<u8>
}

/// The full set of tables backing the state-diff / DA-context store.
pub fn witness_tables() -> Vec<TableSpec> {
    tables![
        BlockStateChangesSchema,
        BlockHashByNumber,
        PublishedCodeHashSchema,
    ]
}
