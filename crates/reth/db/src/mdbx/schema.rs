//! MDBX table definitions for the reth state-diff / DA-context store.
//!
//! Keys are `B256`/`u64` in big-endian form. Values go through the table's
//! [`ValueCodec`](alpen_store_mdbx::ValueCodec), so callers store and read
//! domain types and never touch the encoding.

use alpen_reth_statediff::BlockStateChanges;
use alpen_store_mdbx::{
    define_table, define_table_bincode_be_key, define_table_versioned_be_key, impl_be_key_codec,
    impl_unit_value_codec, tables, TableSpec,
};
use revm_primitives::alloy_primitives::B256;

define_table_versioned_be_key! {
    /// Block state-diff data.
    (BlockStateChangesSchema) B256 => {
        1 => BlockStateChanges as bincode,
    }
}

define_table_bincode_be_key! {
    /// Block number to hash mapping.
    ///
    /// A bare identifier, and the canonical hash at a height changes on a
    /// reorg — nothing here to version.
    (BlockHashByNumber) u64 => B256
}

define_table! {
    /// Set of contract code hashes already published to DA.
    ///
    /// Membership is the whole record, so the value is `()` and occupies no
    /// bytes: the key says everything the table has to say.
    (PublishedCodeHashSchema) B256 => ()
}
impl_be_key_codec!(PublishedCodeHashSchema, B256);
impl_unit_value_codec!(PublishedCodeHashSchema);

/// The full set of tables backing the state-diff / DA-context store.
pub fn witness_tables() -> Vec<TableSpec> {
    tables![
        BlockStateChangesSchema,
        BlockHashByNumber,
        PublishedCodeHashSchema,
    ]
}
