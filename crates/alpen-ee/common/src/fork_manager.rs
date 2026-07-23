//! Runtime fork-schedule management.
//!
//! The upgraded node ships with its pending forks disabled; the activation
//! coordinate is derived when the VK-update message is observed in the EE
//! inbox ordering. This module owns that derivation and its durability:
//!
//! - the sequencer applies the boundary right after the boundary block is
//!   saved and before the next block is built, so the next block is the
//!   first one under the new rules;
//! - derived activations are persisted, and a restarted node rehydrates the
//!   live chainspec from the table before executing or building anything;
//! - a crash between saving the boundary block and persisting the derived
//!   activation is healed at boot by re-checking the tip block, which is the
//!   only block a record can be missing for (later blocks are never built
//!   before the record is durable).

use std::{str::FromStr, sync::Arc};

use alpen_chainspec::AlpenChainSpec;
use eyre::{bail, Context};
use reth_chainspec::{EthereumHardfork, ForkCondition};
use tracing::info;

use crate::{
    find_vk_update, ExecBlockRecord, ForkActivation, ForkActivationRecord, ForkScheduleStorage,
};

/// Owns the runtime-derived fork schedule.
#[derive(Clone)]
pub struct ForkScheduleManager {
    chain_spec: Arc<AlpenChainSpec>,
    pending_evm_forks: Vec<EthereumHardfork>,
    storage: Arc<dyn ForkScheduleStorage>,
}

impl std::fmt::Debug for ForkScheduleManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkScheduleManager")
            .field("pending_evm_forks", &self.pending_evm_forks)
            .finish()
    }
}

impl ForkScheduleManager {
    /// Creates a new manager.
    pub fn new(
        chain_spec: Arc<AlpenChainSpec>,
        pending_evm_forks: Vec<EthereumHardfork>,
        storage: Arc<dyn ForkScheduleStorage>,
    ) -> Self {
        Self {
            chain_spec,
            pending_evm_forks,
            storage,
        }
    }

    /// Applies every persisted fork activation to the live chainspec.
    ///
    /// Must run at boot before any block is executed or built. Returns the
    /// number of activations applied.
    pub async fn rehydrate(&self) -> eyre::Result<usize> {
        let records = self
            .storage
            .get_fork_activations()
            .await
            .context("reading persisted fork activations")?;
        for record in &records {
            self.apply_record(record)?;
        }
        Ok(records.len())
    }

    /// Heals the crash window between saving a boundary block and persisting
    /// its derived activation.
    ///
    /// A record can only be missing for the tip block: later blocks are never
    /// built before the boundary's record is durable. Call at boot with the
    /// local tip after [`Self::rehydrate`].
    pub async fn ensure_boundary_applied(&self, tip: &ExecBlockRecord) -> eyre::Result<()> {
        if find_vk_update(tip.messages()).is_some() {
            self.apply_boundary(tip).await?;
        }
        Ok(())
    }

    /// Derives, persists, and applies the pending forks' activations from the
    /// boundary block (the block that consumed the VK-update message).
    ///
    /// Stock EVM forks are keyed by timestamp: the activation is the boundary
    /// block's EVM timestamp plus one, which selects exactly the blocks after
    /// the boundary since EVM timestamps are strictly increasing. Idempotent:
    /// forks with a persisted activation are re-applied as persisted, never
    /// re-derived, so replaying the boundary (restart, resync) cannot move an
    /// activation.
    pub async fn apply_boundary(&self, boundary: &ExecBlockRecord) -> eyre::Result<()> {
        if self.pending_evm_forks.is_empty() {
            info!(
                boundary_blocknum = boundary.blocknum(),
                "VK-update boundary crossed with no pending forks"
            );
            return Ok(());
        }

        let persisted = self
            .storage
            .get_fork_activations()
            .await
            .context("reading persisted fork activations")?;

        let boundary_blocknum = boundary.blocknum();
        let activation_ts = (boundary.timestamp_ms() / 1000) + 1;

        for fork in &self.pending_evm_forks {
            if let Some(existing) = persisted.iter().find(|r| r.fork() == fork.name()) {
                // Already derived (possibly from a prior crash or replay);
                // the persisted value is authoritative.
                self.apply_record(existing)?;
                continue;
            }

            let record = ForkActivationRecord::new(
                fork.name().to_string(),
                ForkActivation::Timestamp(activation_ts),
                boundary_blocknum,
            );
            // Persist before applying: the in-memory swap is rebuilt from the
            // table at boot, never the other way around.
            self.storage
                .save_fork_activation(record.clone())
                .await
                .context("persisting derived fork activation")?;
            self.apply_record(&record)?;
            info!(
                fork = fork.name(),
                activation_ts,
                boundary_blocknum,
                "activated fork at VK-update boundary"
            );
        }

        Ok(())
    }

    fn apply_record(&self, record: &ForkActivationRecord) -> eyre::Result<()> {
        let Ok(fork) = EthereumHardfork::from_str(record.fork()) else {
            bail!(
                "persisted fork activation names unknown fork `{}`; refusing to continue \
                 with a schedule this binary cannot honor",
                record.fork()
            );
        };
        let cond = match record.activation() {
            ForkActivation::Block(height) => ForkCondition::Block(height),
            ForkActivation::Timestamp(ts) => ForkCondition::Timestamp(ts),
        };
        self.chain_spec
            .set_fork_activation(fork, cond)
            .with_context(|| format!("applying activation for fork {}", record.fork()))?;
        Ok(())
    }
}
