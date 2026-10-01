use alpen_params::AlpenSpecId;
use bitcoin::Txid;
use serde::{Deserialize, Serialize};
use strata_identifiers::L1BlockCommitment;

/// Identifies one recovered EE DA blob.
///
/// SPS-EE-DA permits multiple sequencer-signed candidates for one sequence
/// number, for example after an L2 reorg. The caller selects the candidate
/// whose replayed state matches OL's published inner state root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecoveredDaKey {
    update_seq_no: u64,
    commit_txid: Txid,
}

impl RecoveredDaKey {
    pub(crate) fn new(update_seq_no: u64, commit_txid: Txid) -> Self {
        Self {
            update_seq_no,
            commit_txid,
        }
    }

    pub(crate) fn update_seq_no(&self) -> u64 {
        self.update_seq_no
    }

    pub(crate) fn commit_txid(&self) -> Txid {
        self.commit_txid
    }
}

/// A recovered EE DA payload and its L1 completion block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DBRecoveredDaBlob {
    completion_block: L1BlockCommitment,
    /// [`AlpenSpecId`] discriminant governing the encoded payload layout.
    spec_version: u16,
    payload_bytes: Vec<u8>,
}

impl DBRecoveredDaBlob {
    pub(crate) fn new(
        completion_block: L1BlockCommitment,
        spec_version: AlpenSpecId,
        payload_bytes: Vec<u8>,
    ) -> Self {
        Self {
            completion_block,
            spec_version: spec_version.into(),
            payload_bytes,
        }
    }

    pub(crate) fn into_parts(self) -> (L1BlockCommitment, u16, Vec<u8>) {
        (self.completion_block, self.spec_version, self.payload_bytes)
    }

    pub(crate) fn completion_block(&self) -> L1BlockCommitment {
        self.completion_block
    }
}
