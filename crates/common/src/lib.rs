//! Common traits and types for Alpen execution environment components.

#[cfg(test)]
use mockall as _;

mod traits;
mod types;
mod utils;

#[cfg(feature = "test-utils")]
pub use traits::{
    da::MockBatchDaProvider,
    prover::MockBatchProver,
    storage::{
        batch_storage_test_fns, chunk_storage_test_fns, exec_block_storage_test_fns,
        tests as storage_test_fns, InMemoryStorage, MockAccessedStateStore, MockBatchStorage,
        MockBlockWitnessStore, MockChunkStorage, MockExecBlockStorage, MockStorage,
    },
};
pub use traits::{
    da::{BatchDaProvider, DaStatus, HeaderSummaryProvider},
    prover::{BatchProver, ProofGenerationStatus},
    storage::{
        require_best_ee_account_state, require_best_finalized_block, require_genesis_batch,
        require_latest_batch, AccessedStateStore, BatchStorage, BlockWitnessStore, ChunkStorage,
        DaContext, ExecBlockStorage, OLBlockOrEpoch, StateDiffProvider, StateDiffStore, Storage,
        StorageError,
    },
};
pub use types::{
    accessed_state::{AccessedAccount, AccessedStateRecord},
    batch::{Batch, BatchId, BatchStatus, L1DaBlockInfo, L1DaBlockRef},
    blocknumhash::BlockNumHash,
    chunk::{Chunk, ChunkId, ChunkStatus},
    consensus_heads::ConsensusHeads,
    ee_account_state::EeAccountStateAtEpoch,
    exec_record::{ExecBlockPayload, ExecBlockRecord},
    prover::{Proof, ProofId},
    prover_task_key::{
        decode_batch_task_key, decode_chunk_task_key, encode_batch_task_key, encode_chunk_task_key,
        ProverTaskKeyDecodeError, ProverTaskKeyKind, RANGE_TASK_KEY_BYTES,
    },
};
pub use utils::{
    clock::{Clock, SystemClock},
    ledger_refs::build_ledger_refs_from_da,
};
