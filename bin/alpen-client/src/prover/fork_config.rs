//! Fork-aware guest chain config.
//!
//! The proof guests derive the EVM rules from the chain config in their
//! input. The base config is captured at startup, before any runtime-derived
//! fork activation exists — a real upgrade ships a new ELF with the fork
//! baked in as active, and the runtime-derived activation table is this
//! deployment's equivalent. Input assembly therefore patches the derived
//! activations into the config on every fetch, so post-boundary blocks are
//! proven under the rules they were built with. Activations are persisted
//! before any post-boundary block exists, so every proof of such a block
//! observes them; pre-boundary blocks predate the activation timestamps and
//! are unaffected by the patch.

use alpen_ee_common::{ForkActivation, ForkActivationRecord};
use rsp_primitives::genesis::Genesis;
use tracing::warn;

/// Returns `genesis` with the derived fork activations applied.
pub(crate) fn apply_fork_activations(
    genesis: &Genesis,
    records: &[ForkActivationRecord],
) -> Genesis {
    let mut patched = genesis.clone();
    let Genesis::Custom(config) = &mut patched else {
        if !records.is_empty() {
            warn!("derived fork activations cannot be applied to a named chain config");
        }
        return patched;
    };

    for record in records {
        let ForkActivation::Timestamp(ts) = record.activation() else {
            warn!(
                fork = record.fork(),
                "skipping non-timestamp fork activation"
            );
            continue;
        };
        let slot = match record.fork() {
            "Shanghai" => &mut config.shanghai_time,
            "Cancun" => &mut config.cancun_time,
            "Prague" => &mut config.prague_time,
            "Osaka" => &mut config.osaka_time,
            "Bpo1" => &mut config.bpo1_time,
            "Bpo2" => &mut config.bpo2_time,
            "Bpo3" => &mut config.bpo3_time,
            "Bpo4" => &mut config.bpo4_time,
            "Bpo5" => &mut config.bpo5_time,
            other => {
                warn!(fork = other, "unknown fork in derived activation table");
                continue;
            }
        };
        *slot = Some(ts);
    }

    patched
}
