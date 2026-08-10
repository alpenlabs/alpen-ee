//! Replay snapshot types.

use rsp_mpt::EthereumState;

/// In-memory replay anchor used for partial replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayStateSnapshot {
    next_update_seq_no: u64,
    last_applied_block_num: Option<u64>,
    ethereum_state: EthereumState,
}

impl ReplayStateSnapshot {
    /// Creates a snapshot bound to the supplied state's current root.
    pub fn new(
        next_update_seq_no: u64,
        last_applied_block_num: Option<u64>,
        ethereum_state: EthereumState,
    ) -> Self {
        Self {
            next_update_seq_no,
            last_applied_block_num,
            ethereum_state,
        }
    }

    /// Returns the update sequence number expected for the next replay batch.
    pub fn next_update_seq_no(&self) -> u64 {
        self.next_update_seq_no
    }

    /// Returns the last EVM block number applied before this snapshot.
    pub fn last_applied_block_num(&self) -> Option<u64> {
        self.last_applied_block_num
    }

    /// Returns the Ethereum state carried by this snapshot.
    pub fn ethereum_state(&self) -> &EthereumState {
        &self.ethereum_state
    }

    pub(crate) fn into_parts(self) -> (u64, Option<u64>, EthereumState) {
        (
            self.next_update_seq_no,
            self.last_applied_block_num,
            self.ethereum_state,
        )
    }
}
