//! MDBX table definitions for the reth state-diff / DA-context store.
//!
//! Keys are `B256`/`u64` in big-endian form. Values go through the table's
//! [`ValueCodec`](alpen_db_store_mdbx::ValueCodec), so callers store and read
//! domain types and never touch the encoding.

use alpen_db_store_mdbx::{
    define_table_bincode_be_key, define_table_raw_be_key, tables, TableSpec,
};
use alpen_reth_statediff::BlockStateChanges;
use revm_primitives::alloy_primitives::B256;

define_table_bincode_be_key! {
    /// Block state-diff data.
    (BlockStateChangesSchema) B256 => BlockStateChanges
}

define_table_bincode_be_key! {
    /// Block number to hash mapping.
    (BlockHashByNumber) u64 => B256
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
