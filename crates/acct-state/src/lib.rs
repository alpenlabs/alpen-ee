//! Reconstructs EE account state from accepted account-update data.

mod error;
mod manifest;
mod reconstruction;

pub use error::EeAccountReconstructionError;
pub use manifest::{EeAccountUpdateData, EeAccountUpdateManifest, EeAccountUpdateManifestError};
pub use reconstruction::{
    apply_ee_account_update_manifest, compute_ee_account_inner_root, reconstruct_ee_account_update,
};
