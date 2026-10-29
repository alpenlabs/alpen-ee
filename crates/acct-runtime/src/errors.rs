//! Error types for the account runtime.

use alpen_acct_types::UpdateExtraData;
use ssz::DecodeError;
use strata_acct_types::Hash;
use strata_snark_acct_runtime::{ProgramError, ProgramResult};
use strata_snark_acct_types::UpdateOutputs;
use thiserror::Error;

/// Why an account update failed verification.
#[derive(Debug, Error)]
pub enum AcctRuntimeError {
    /// A chunk transition could not be decoded.
    #[error("decode transition of chunk {index}: {source:?}")]
    DecodeChunkTransition { index: usize, source: DecodeError },

    /// A chunk proof failed verification against the chunk predicate key.
    #[error("invalid proof for chunk {index}")]
    InvalidChunkProof { index: usize },

    /// A chunk does not build on the current tip.
    #[error("chunk parent {parent} does not extend tip {tip}")]
    ChainLinkage {
        /// The tip the chunk had to build on.
        tip: Hash,
        /// The parent the chunk builds on.
        parent: Hash,
    },

    /// A chunk deposit does not match the next pending input.
    #[error("chunk deposit does not match pending input {position}")]
    DepositMismatch {
        /// Position of the pending input in the account's queue.
        position: usize,
    },

    /// A chunk rotation does not match the next pending input.
    #[error("chunk rotation does not match pending input {position}")]
    RotationMismatch {
        /// Position of the pending input in the account's queue.
        position: usize,
    },

    /// A chunk follows a consumed predicate rotation.
    ///
    /// A rotation has to be the last thing in its update. Anything after it
    /// would still be authorized by the predecessor predicate, which is
    /// exactly what the rotation is meant to stop.
    #[error("chunk follows a consumed predicate rotation")]
    NonTerminalRotation,

    /// The pending input queue is full, so a message's input can't be queued.
    ///
    /// Dropping the input instead would lose a deposit the OL already
    /// delivered.
    #[error("pending input queue is full")]
    PendingInputsFull,

    /// Accumulated output transfers or messages exceed the update's capacity.
    #[error("outputs exceed the update's capacity")]
    OutputOverflow,

    /// The claimed outputs differ from the ones the chunks produce.
    #[error("outputs mismatch: claimed {claimed:?}, computed {computed:?}")]
    OutputsMismatch {
        claimed: Box<UpdateOutputs>,
        computed: Box<UpdateOutputs>,
    },

    /// The claimed extra data differs from the one the chunks produce.
    #[error("extra data mismatch: claimed {claimed:?}, computed {computed:?}")]
    ExtraDataMismatch {
        claimed: Box<UpdateExtraData>,
        computed: Box<UpdateExtraData>,
    },
}

pub type AcctRuntimeResult<T> = Result<T, AcctRuntimeError>;

impl From<AcctRuntimeError> for ProgramError<AcctRuntimeError> {
    fn from(value: AcctRuntimeError) -> Self {
        Self::Internal(value)
    }
}

/// Result of the snark account program's hooks.
pub type AcctProgramResult<T> = ProgramResult<T, AcctRuntimeError>;
