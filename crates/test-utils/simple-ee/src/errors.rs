//! Error types for the simple execution environment.

use thiserror::Error;

/// Why the simple execution environment rejected a block.
#[derive(Debug, Error)]
pub enum SimpleEnvError {
    /// A header does not build on its parent.
    #[error("header does not build on its parent")]
    InvalidHeader,

    /// A block's deposits differ from its claimed inputs.
    #[error("block deposits differ from the claimed inputs")]
    InputMismatch,

    /// A block's claimed outputs differ from what executing it produced.
    #[error("block outputs differ from its execution")]
    OutputMismatch,

    /// A transaction or deposit can't be applied to the state.
    #[error("invalid transaction")]
    InvalidTx,
}

pub type SimpleEnvResult<T> = Result<T, SimpleEnvError>;
