//! Errors of the EVM execution environment.

use alpen_chain_types::ExecOutputs;
use alpen_params::AlpenSpecId;
use alpen_primitives::WithdrawalIntentExtractionError;
use reth_consensus::ConsensusError;
use reth_evm::execute::BlockExecutionError;
use revm_primitives::B256;
use strata_acct_types::SubjectId;
use thiserror::Error;

/// Why an EVM block failed one of the environment's checks or its execution.
#[derive(Debug, Error)]
pub enum EvmEnvError {
    /// The block breaks the node's consensus rules.
    #[error("block breaks consensus rules: {0}")]
    ConsensusRule(#[source] Box<ConsensusError>),

    /// The header is stamped with a spec version other than the one this
    /// environment runs.
    #[error("header stamped with spec version {got:?}, environment runs {expected:?}")]
    SpecVersionMismatch {
        expected: AlpenSpecId,
        got: AlpenSpecId,
    },

    /// A transaction's sender could not be recovered from its signature.
    #[error("could not recover a transaction sender")]
    SenderRecovery,

    /// Block 0 has no parent, so it cannot be executed.
    #[error("block 0 has no parent")]
    NoParent,

    /// The witness lacks the ancestor header at this block number.
    #[error("witness has no ancestor at block {0}")]
    MissingAncestor(u64),

    /// The witnessed parent is not the block's parent.
    #[error("witnessed parent {witnessed} is not the block's parent {expected}")]
    AncestorMismatch { expected: B256, witnessed: B256 },

    /// The EVM failed to execute the block.
    #[error("block execution failed: {0}")]
    Execution(#[source] Box<BlockExecutionError>),

    /// A bridge-out log could not be read as a withdrawal intent.
    #[error("withdrawal intent: {0}")]
    WithdrawalIntent(#[from] WithdrawalIntentExtractionError),

    /// The block's deposit withdrawals and the input deposits differ in count.
    #[error("block has {withdrawals} deposit withdrawals, inputs have {deposits} deposits")]
    DepositCountMismatch { deposits: usize, withdrawals: usize },

    /// A deposit's destination is not an EVM address.
    #[error("deposit destination {0:?} is not an EVM address")]
    InvalidDepositAddress(SubjectId),

    /// A deposit's amount does not fit in gwei.
    #[error("deposit {index} amount overflows gwei")]
    DepositAmountOverflow { index: usize },

    /// A deposit differs from the block's withdrawal at the same position.
    #[error("deposit {index} differs from the block's withdrawal")]
    DepositMismatch { index: usize },

    /// The block's claimed outputs differ from what executing it produced.
    #[error("block output mismatch: claimed {claimed:?}, executed {executed:?}")]
    OutputMismatch {
        claimed: Box<ExecOutputs>,
        executed: Box<ExecOutputs>,
    },
}

impl From<ConsensusError> for EvmEnvError {
    fn from(err: ConsensusError) -> Self {
        Self::ConsensusRule(Box::new(err))
    }
}

impl From<BlockExecutionError> for EvmEnvError {
    fn from(err: BlockExecutionError) -> Self {
        Self::Execution(Box::new(err))
    }
}
