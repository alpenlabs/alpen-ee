//! Common traits and types for Alpen execution environment components.

#[cfg(test)]
use mockall as _;

mod traits;
mod types;
mod utils;

pub use alpen_reth_primitives::sats_to_gwei;
#[cfg(feature = "test-utils")]
pub use traits::{
    da::MockBatchDaProvider,
    ol_client::{MockOLClient, MockSequencerOLClient},
    prover::MockBatchProver,
    storage::{
        MockAccessedStateStore, MockBatchStorage, MockBlockWitnessStore, MockChunkStorage,
        MockExecBlockStorage, MockStorage,
    },
};
pub use traits::{
    da::{BatchDaProvider, DaStatus, HeaderSummaryProvider},
    engine::{
        EnginePayload, ExecutionEngine, ExecutionEngineError, ForkchoiceState, PayloadBuilderEngine,
    },
    ol_client::{
        chain_status_checked, get_inbox_messages_checked, OLAccountStateView, OLBlockData,
        OLBlockLink, OLClient, OLClientError, SequencerOLClient,
    },
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
    ol_account_epoch_summary::{SnarkAccountEpochSummary, SnarkAccountUpdateInfo},
    ol_chain_status::{OLChainStatus, OLFinalizedStatus},
    payload_builder::{DepositInfo, PayloadBuildAttributes},
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
