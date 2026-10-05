//! Database implementation for Alpen execution environment.

// Referenced only from `#[serde(with = "hex::serde")]` attributes, which the
// unused-crate-dependencies lint cannot see.
use hex as _;
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
    create_ee_envs, open_stores, BroadcastDbOps, ChunkedEnvelopeOps, SequencerDatabases, Stores,
};
pub use mdbxdb::{
    BatchTaskKey, ChunkTaskKey, DaContextDbMdbx, NodeDbMdbx, ProverDbMdbx, ProverTaskKey,
    WitnessDbMdbx,
};
pub use storage::NodeStorage;

/// The store's on-disk model, for tooling and tests: every table marker and
/// the table list each environment is opened with.
///
/// This is not a service API. The node reads and writes through
/// [`NodeStorage`] and the traits in `alpen-common`; the operator console
/// (`alpen-dbconsole-core`) reflects the tables through the codecs these
/// markers carry, which is the one reason they are public.
pub mod schema {
    pub use crate::mdbxdb::schema::{
        da_tables, node_tables, prover_tables, witness_tables, AccountStateAtOLEpochSchema,
        AcctProofIdIndexSchema, AcctProofReceiptSchema, AcctProverTaskSchema, BatchByIdxSchema,
        BatchChunksSchema, BatchIdToIdxSchema, BlockAccessedStateSchema, BlockHashByNumber,
        BlockStateChangesSchema, BlockWitnessSchema, BytecodeSchema, ChunkByIdxSchema,
        ChunkIdToIdxSchema, ChunkProofReceiptSchema, ChunkProverTaskSchema,
        ExecBlockFinalizedSchema, ExecBlockPayloadSchema, ExecBlockSchema,
        ExecBlocksAtHeightSchema, L1BroadcastActiveTxNodeSchema, L1BroadcastTxIdSchema,
        L1BroadcastTxNodeSchema, L1BroadcastTxSchema, L1ChunkedEnvelopeSchema,
        OLBlockAtEpochSchema, PublishedCodeHashSchema,
    };
}

/// The database representations of the node's types, exactly as the tables
/// store them. Public for the same reason as [`schema`].
pub mod records {
    #[cfg(feature = "migration")]
    pub use crate::serialization_types::{package_from_sled_era, package_to_sled_era};
    pub use crate::serialization_types::{
        DBAccountStateAtEpoch, DBBatch, DBBatchId, DBBatchStatus, DBBatchWithStatus, DBChunk,
        DBChunkId, DBChunkStatus, DBChunkWithStatus, DBEeAccountState, DBExecBlockRecord,
        DBL1DaBlockRef, DBOLBlockId, DBTxidPair,
    };
}
