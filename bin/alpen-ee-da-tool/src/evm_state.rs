//! Reconstructs EVM state from recovered EE DA blobs.

use alpen_ee_da_l1_extraction::RecoveredDaBlob;
use alpen_ee_l1_reconstruction::{
    reconstruct_from_genesis, L1ReconstructionError, L1ReconstructionOutcome,
};
use alpen_ee_params::AlpenParams;

/// Reconstructs EVM state from genesis using recovered EE DA blobs.
pub(crate) fn reconstruct_evm_state(
    params: &AlpenParams,
    recovered_blobs: Vec<RecoveredDaBlob>,
) -> Result<Option<L1ReconstructionOutcome>, L1ReconstructionError> {
    let genesis_accounts = params.evm_spec().genesis().alloc.clone();
    reconstruct_from_genesis(genesis_accounts, recovered_blobs)
}
