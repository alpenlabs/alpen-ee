//! Error types for builder utilities.

use strata_snark_acct_runtime::ProgramError;
use thiserror::Error;

use crate::errors::AcctRuntimeError;

#[derive(Debug, Error)]
pub enum BuilderError {
    /// A chunk transition breaks a rule the verifier checks.
    #[error("account runtime: {0}")]
    Runtime(#[from] AcctRuntimeError),

    /// Program error during message processing or finalization.
    #[error("snark program: {0}")]
    Program(ProgramError<AcctRuntimeError>),
}

/// Manual impl for this trait due to macro inflexibility, I guess?
impl From<ProgramError<AcctRuntimeError>> for BuilderError {
    fn from(value: ProgramError<AcctRuntimeError>) -> Self {
        Self::Program(value)
    }
}

pub type BuilderResult<T> = Result<T, BuilderError>;
