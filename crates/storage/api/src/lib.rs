//! Storage traits for the EE node: what each service reads and writes, and
//! the errors a store reports.
//!
//! The `test-utils` feature adds mocks of every trait and the `testing`
//! module: an in-memory store and the acceptance suites a store must pass.

mod accessed_state;
mod account;
mod batch;
mod block_witness;
mod chunk;
mod da_context;
mod errors;
mod exec_block;
mod state_diff;
#[cfg(any(test, feature = "test-utils"))]
pub mod testing;
mod utils;

pub use accessed_state::AccessedStateStore;
#[cfg(feature = "test-utils")]
pub use accessed_state::MockAccessedStateStore;
#[cfg(feature = "test-utils")]
pub use account::MockStorage;
pub use account::{OLBlockOrEpoch, Storage};
pub use batch::BatchStorage;
#[cfg(feature = "test-utils")]
pub use batch::MockBatchStorage;
pub use block_witness::BlockWitnessStore;
#[cfg(feature = "test-utils")]
pub use block_witness::MockBlockWitnessStore;
pub use chunk::ChunkStorage;
#[cfg(feature = "test-utils")]
pub use chunk::MockChunkStorage;
pub use da_context::DaContext;
pub use errors::StorageError;
pub use exec_block::ExecBlockStorage;
#[cfg(feature = "test-utils")]
pub use exec_block::MockExecBlockStorage;
pub use state_diff::{StateDiffProvider, StateDiffStore};
pub use utils::{
    require_best_ee_account_state, require_best_finalized_block, require_genesis_batch,
    require_latest_batch,
};
