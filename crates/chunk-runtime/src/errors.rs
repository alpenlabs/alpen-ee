//! Error types for the chunk runtime.

use alpen_acct_types::{EnvError, Hash};
use alpen_chain_types::{ChunkTransition, ExecOutputs};
use ssz::DecodeError;
use strata_codec::CodecError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChunkRuntimeError {
    /// Execution environment error.
    #[error("execution environment: {0}")]
    Env(#[from] EnvError),

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

    /// A block's header does not commit to its body.
    #[error("block header does not match its body")]
    HeaderBodyMismatch,

    /// A block's claimed outputs differ from what executing it produced.
    #[error("block output mismatch: expected {expected:?}, got {got:?}")]
    OutputMismatch {
        /// Outputs claimed for the block.
        expected: Box<ExecOutputs>,
        /// Outputs produced by executing the block.
        got: Box<ExecOutputs>,
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

pub type ChunkRuntimeResult<T> = Result<T, ChunkRuntimeError>;
