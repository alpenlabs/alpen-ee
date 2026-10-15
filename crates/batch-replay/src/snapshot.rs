//! Batch replay snapshot types.

use alpen_reth_statediff::EthereumStateExt;
use rsp_mpt::EthereumState;
use strata_identifiers::Buf32;
use strata_snark_acct_types::Seqno;

use crate::BatchReplayError;

/// In-memory replay anchor used for partial replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReplaySnapshot {
    state_root: Buf32,
    next_update_seq_no: Seqno,
    last_applied_block_num: u64,
    ethereum_state: EthereumState,
}

impl BatchReplaySnapshot {
    /// Creates a snapshot after verifying the state against its recorded root.
    pub fn try_new(
        next_update_seq_no: Seqno,
        last_applied_block_num: u64,
        state_root: Buf32,
        ethereum_state: EthereumState,
    ) -> Result<Self, BatchReplayError> {
        let actual = ethereum_state.state_root_buf32();
        if actual != state_root {
            return Err(BatchReplayError::SnapshotRootMismatch {
                expected: state_root,
                actual,
            });
        }

        Ok(Self {
            state_root,
            next_update_seq_no,
            last_applied_block_num,
            ethereum_state,
        })
    }

    /// Returns the Ethereum state root recorded by this snapshot.
    pub fn state_root(&self) -> Buf32 {
        self.state_root
    }

    /// Returns the update sequence number expected for the next replay batch.
    pub fn next_update_seq_no(&self) -> Seqno {
        self.next_update_seq_no
    }

    /// Returns the last EVM block number applied before this snapshot.
    pub fn last_applied_block_num(&self) -> u64 {
        self.last_applied_block_num
    }

    /// Returns the Ethereum state carried by this snapshot.
    pub fn ethereum_state(&self) -> &EthereumState {
        &self.ethereum_state
    }

    pub(crate) fn into_parts(self) -> (Seqno, u64, EthereumState) {
        (
            self.next_update_seq_no,
            self.last_applied_block_num,
            self.ethereum_state,
        )
    }
}
