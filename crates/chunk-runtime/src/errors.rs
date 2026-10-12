//! Error types for the chunk runtime.

use alpen_acct_types::EnvError;
use alpen_chain_types::ChunkTransition;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChunkRuntimeError {
    /// Execution environment error.
    #[error("execution environment: {0}")]
    Env(#[from] EnvError),

    /// The transition computed from the chunk differs from the claimed one.
    #[error("chunk transition mismatch: expected {expected:?}, got {got:?}")]
    TransitionMismatch {
        /// Transition claimed in the private input.
        expected: Box<ChunkTransition>,
        /// Transition computed by executing the chunk.
        got: Box<ChunkTransition>,
    },
}

pub type ChunkRuntimeResult<T> = Result<T, ChunkRuntimeError>;
