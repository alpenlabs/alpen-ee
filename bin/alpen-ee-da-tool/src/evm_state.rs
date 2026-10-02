//! Reconstructs EVM state from recovered EE DA blobs.

use alpen_ee_batch_replay::BatchReplaySnapshot;
use alpen_ee_da_l1_extraction::RecoveredDaBlob;
use alpen_ee_l1_reconstruction::{
    reconstruct_from_genesis, reconstruct_from_snapshot, L1ReconstructionError,
    L1ReconstructionOutcome,
};
use alpen_ee_params::AlpenParams;

/// Reconstructs EVM state from a replay snapshot or genesis using recovered EE DA blobs.
pub(crate) fn reconstruct_evm_state(
    params: &AlpenParams,
    replay_snapshot: Option<BatchReplaySnapshot>,
    recovered_blobs: Vec<RecoveredDaBlob>,
) -> Result<Option<L1ReconstructionOutcome>, L1ReconstructionError> {
    match replay_snapshot {
        Some(replay_snapshot) => reconstruct_from_snapshot(replay_snapshot, recovered_blobs),
        None => {
            let genesis_accounts = params.evm_spec().genesis().alloc.clone();
            reconstruct_from_genesis(genesis_accounts, recovered_blobs)
        }
    }
}
