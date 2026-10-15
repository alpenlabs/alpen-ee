//! Error types for the chunk runtime.

use alpen_acct_types::Hash;
use alpen_chain_types::ChunkTransition;
use ssz::DecodeError;
use strata_codec::CodecError;
use thiserror::Error;

/// Why a chunk failed verification.
///
/// `EnvErr` is the execution environment's own error, which says which of its
/// checks the block broke.
#[derive(Debug, Error)]
pub enum ChunkRuntimeError<EnvErr> {
    /// A check or the execution in the execution environment failed.
    #[error("execution environment: {0}")]
    Env(#[from] EnvErr),

    /// The claimed chunk transition could not be decoded.
    #[error("decode chunk transition: {0:?}")]
    DecodeChunkTransition(DecodeError),

    /// The parent header could not be decoded.
    #[error("decode parent header: {0}")]
    DecodeParentHeader(CodecError),

    /// The pre state could not be decoded.
    #[error("decode pre state: {0}")]
    DecodePreState(CodecError),

    /// A block in the chunk could not be decoded.
    #[error("decode block {index}: {source}")]
    DecodeBlock { index: usize, source: CodecError },

    /// A block's inputs could not be decoded.
    #[error("decode inputs of block {index}: {source:?}")]
    DecodeBlockInputs { index: usize, source: DecodeError },

    /// A block's outputs could not be decoded.
    #[error("decode outputs of block {index}: {source:?}")]
    DecodeBlockOutputs { index: usize, source: DecodeError },

    /// The chunk has no blocks.
    #[error("chunk has no blocks")]
    EmptyChunk,

    /// The pre state does not match the parent header's state root.
    #[error("pre state root mismatch: expected {expected:?}, got {got:?}")]
    PreStateRootMismatch {
        /// State root in the parent header.
        expected: Hash,
        /// State root computed from the pre state.
        got: Hash,
    },

    /// The state after a block does not match the block's state root.
    #[error("post state root mismatch: expected {expected:?}, got {got:?}")]
    PostStateRootMismatch {
        /// State root in the block's header.
        expected: Hash,
        /// State root computed after executing the block.
        got: Hash,
    },

    /// A block follows a predicate rotation in the same chunk.
    #[error("block follows a predicate rotation in the same chunk")]
    NonTerminalRotation,

    /// The transition computed from the chunk differs from the claimed one.
    #[error("chunk transition mismatch: expected {expected:?}, got {got:?}")]
    TransitionMismatch {
        /// Transition claimed in the private input.
        expected: Box<ChunkTransition>,
        /// Transition computed by executing the chunk.
        got: Box<ChunkTransition>,
    },
}

pub type ChunkRuntimeResult<T, EnvErr> = Result<T, ChunkRuntimeError<EnvErr>>;
