//! Simple execution environment for testing and demonstration.
//!
//! This provides a simple account-based execution model where "accounts" are
//! identified by SubjectId and have a balance. Transactions can move value
//! between accounts and emit outputs.

mod errors;
mod execution;
mod types;

pub use errors::{SimpleEnvError, SimpleEnvResult};
pub use execution::SimpleExecutionEnvironment;
pub use types::{
    SimpleBlock, SimpleBlockBody, SimpleHeader, SimpleHeaderIntrinsics, SimplePartialState,
    SimpleTransaction, SimpleWriteBatch,
};
