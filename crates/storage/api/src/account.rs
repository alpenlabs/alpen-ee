use alpen_acct_types::EeAccountState;
use alpen_common::EeAccountStateAtEpoch;
use async_trait::async_trait;
use strata_identifiers::{EpochCommitment, OLBlockId};

use super::StorageError;

/// Identifies an OL block either by block ID or slot number.
#[derive(Debug)]
pub enum OLBlockOrEpoch {
    /// Identifies by block ID.
    TerminalBlock(OLBlockId),
    /// Identifies by slot number.
    Epoch(u32),
}

impl From<OLBlockId> for OLBlockOrEpoch {
    fn from(value: OLBlockId) -> Self {
        Self::TerminalBlock(value)
    }
}

impl From<&OLBlockId> for OLBlockOrEpoch {
    fn from(value: &OLBlockId) -> Self {
        Self::TerminalBlock(*value)
    }
}

impl From<u32> for OLBlockOrEpoch {
    fn from(value: u32) -> Self {
        OLBlockOrEpoch::Epoch(value)
    }
}

#[cfg_attr(feature = "test-utils", mockall::automock)]
#[async_trait]
/// Persistence for EE Nodes
pub trait Storage: Send + Sync {
    /// Get EE account internal state corresponding to a given OL slot.
    async fn ee_account_state(
        &self,
        block_or_epoch: OLBlockOrEpoch,
    ) -> Result<Option<EeAccountStateAtEpoch>, StorageError>;

    /// Get EE account internal state for the highest epoch available.
    async fn best_ee_account_state(&self) -> Result<Option<EeAccountStateAtEpoch>, StorageError>;

    /// Store EE account internal state for next slot.
    async fn store_ee_account_state(
        &self,
        ol_epoch: &EpochCommitment,
        ee_account_state: &EeAccountState,
    ) -> Result<(), StorageError>;

    /// Remove stored EE internal account state for epochs > `to_epoch`.
    async fn rollback_ee_account_state(&self, to_epoch: u32) -> Result<(), StorageError>;
}
