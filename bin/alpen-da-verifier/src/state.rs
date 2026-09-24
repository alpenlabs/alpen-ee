//! Mutable state and transitions for the EE DA verifier service.

use std::{num::NonZeroU32, sync::Arc};

use alpen_da_l1_extraction::DaExtractor;
use strata_identifiers::L1Height;
use thiserror::Error;
use tracing::{debug, info};

use crate::{
    bitcoin::FetchBitcoinTipError,
    context::DaVerifierContext,
    da_extraction::{DaRecoveryDriver, DaRecoveryError},
};

/// Failure to complete one EE DA verification cycle.
#[derive(Debug, Error)]
pub(crate) enum DaVerifierError {
    /// Fetching the current Bitcoin tip failed.
    #[error(transparent)]
    FetchBitcoinTip(#[from] FetchBitcoinTipError),

    /// Recovering or persisting EE DA failed.
    #[error(transparent)]
    DaRecovery(#[from] DaRecoveryError),
}

impl DaVerifierError {
    /// Returns whether a later verification cycle may succeed without intervention.
    pub(crate) fn is_recoverable(&self) -> bool {
        match self {
            Self::FetchBitcoinTip(error) => error.is_recoverable(),
            Self::DaRecovery(error) => error.is_recoverable(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaRecoveryAction {
    WaitForReorgSafeTip,
    CaughtUp { reorg_safe_tip: L1Height },
    RecoverThrough { reorg_safe_tip: L1Height },
}

impl DaRecoveryAction {
    fn reorg_safe_tip(self) -> Option<L1Height> {
        match self {
            Self::WaitForReorgSafeTip => None,
            Self::CaughtUp { reorg_safe_tip } | Self::RecoverThrough { reorg_safe_tip } => {
                Some(reorg_safe_tip)
            }
        }
    }
}

/// Policy and mutable progress for recovering and persisting EE DA.
pub(crate) struct DaRecoveryState {
    l1_reorg_safe_depth: u32,
    max_l1_scan_window_size: NonZeroU32,
    driver: DaRecoveryDriver,

    /// Published for status only; recomputed from the Bitcoin tip each cycle.
    reorg_safe_tip: Option<L1Height>,
}

impl DaRecoveryState {
    /// Creates recovery state anchored at the configured genesis L1 height.
    pub(crate) fn new(
        l1_reorg_safe_depth: u32,
        max_l1_scan_window_size: NonZeroU32,
        extractor: DaExtractor,
        genesis_l1_height: L1Height,
    ) -> Self {
        Self {
            l1_reorg_safe_depth,
            max_l1_scan_window_size,
            driver: DaRecoveryDriver::new(extractor, genesis_l1_height),
            reorg_safe_tip: None,
        }
    }

    /// Recovers EE DA through the current reorg-safe L1 tip.
    async fn recover_da_to_safe_tip(
        &mut self,
        context: &impl DaVerifierContext,
    ) -> Result<(), DaVerifierError> {
        self.driver.flush_pending(context).await?;

        let tip_height = context.fetch_bitcoin_tip_height().await?;
        let action = decide_da_recovery_action(
            tip_height,
            self.l1_reorg_safe_depth,
            self.driver.next_l1_height(),
        );
        self.reorg_safe_tip = action.reorg_safe_tip();

        match action {
            DaRecoveryAction::WaitForReorgSafeTip => {
                debug!(
                    tip_height,
                    reorg_safe_depth = self.l1_reorg_safe_depth,
                    "Bitcoin chain has not reached the EE DA recovery depth"
                );
                Ok(())
            }
            DaRecoveryAction::CaughtUp { reorg_safe_tip } => {
                debug!(
                    next_l1_height = self.driver.next_l1_height(),
                    reorg_safe_tip, "no reorg-safe L1 blocks require EE DA recovery"
                );
                Ok(())
            }
            DaRecoveryAction::RecoverThrough { reorg_safe_tip } => {
                self.recover_windows_through(context, reorg_safe_tip).await
            }
        }
    }

    async fn recover_windows_through(
        &mut self,
        context: &impl DaVerifierContext,
        reorg_safe_tip: L1Height,
    ) -> Result<(), DaVerifierError> {
        while self.driver.next_l1_height() <= reorg_safe_tip {
            let start_height = self.driver.next_l1_height();
            let end_height =
                l1_scan_window_end(start_height, self.max_l1_scan_window_size, reorg_safe_tip);
            self.driver.recover_through(context, end_height).await?;

            info!(
                start_height,
                end_height,
                reorg_safe_tip,
                next_l1_height = self.driver.next_l1_height(),
                "completed L1 scan window for EE DA recovery"
            );
        }
        Ok(())
    }
}

/// Owns the verifier context and per-phase mutable state.
pub(crate) struct DaVerifierServiceState<C> {
    context: Arc<C>,
    recovery_state: DaRecoveryState,
}

impl<C: DaVerifierContext> DaVerifierServiceState<C> {
    /// Creates verifier state from its DA recovery state.
    pub(crate) fn new(context: Arc<C>, recovery_state: DaRecoveryState) -> Self {
        Self {
            context,
            recovery_state,
        }
    }

    /// Advances recovery for one service tick.
    pub(crate) async fn handle_tick(&mut self) -> Result<(), DaVerifierError> {
        self.recovery_state
            .recover_da_to_safe_tip(self.context.as_ref())
            .await
    }

    /// Returns the next L1 height to process.
    pub(crate) fn next_l1_height(&self) -> L1Height {
        self.recovery_state.driver.next_l1_height()
    }

    /// Returns the latest reorg-safe L1 tip observed by recovery.
    pub(crate) fn reorg_safe_tip(&self) -> Option<L1Height> {
        self.recovery_state.reorg_safe_tip
    }
}

fn decide_da_recovery_action(
    tip_height: L1Height,
    reorg_safe_depth: u32,
    next_l1_height: L1Height,
) -> DaRecoveryAction {
    let Some(reorg_safe_tip) = tip_height.checked_sub(reorg_safe_depth) else {
        return DaRecoveryAction::WaitForReorgSafeTip;
    };

    if next_l1_height > reorg_safe_tip {
        DaRecoveryAction::CaughtUp { reorg_safe_tip }
    } else {
        DaRecoveryAction::RecoverThrough { reorg_safe_tip }
    }
}

fn l1_scan_window_end(
    start_height: L1Height,
    max_window_size: NonZeroU32,
    target_height: L1Height,
) -> L1Height {
    start_height
        .checked_add(max_window_size.get() - 1)
        .unwrap_or(L1Height::MAX)
        .min(target_height)
}

#[cfg(test)]
mod tests {
    use bitcoind_async_client::error::ClientError;

    use super::*;

    const SAFE_DEPTH: u32 = 6;
    const TIP: L1Height = 100;
    const SAFE_TIP: L1Height = TIP - SAFE_DEPTH;
    const TEST_SCAN_WINDOW_SIZE: NonZeroU32 = NonZeroU32::new(3).expect("3 is nonzero");

    #[test]
    fn test_recovery_waits_when_chain_has_no_reorg_safe_tip() {
        let action = decide_da_recovery_action(5, SAFE_DEPTH, 0);

        assert_eq!(action, DaRecoveryAction::WaitForReorgSafeTip);
        assert_eq!(action.reorg_safe_tip(), None);
    }

    #[test]
    fn test_recovery_is_caught_up_after_reorg_safe_tip() {
        let action = decide_da_recovery_action(TIP, SAFE_DEPTH, SAFE_TIP + 1);

        assert_eq!(
            action,
            DaRecoveryAction::CaughtUp {
                reorg_safe_tip: SAFE_TIP,
            }
        );
        assert_eq!(action.reorg_safe_tip(), Some(SAFE_TIP));
    }

    #[test]
    fn test_recovery_includes_next_height_at_reorg_safe_tip() {
        let action = decide_da_recovery_action(TIP, SAFE_DEPTH, SAFE_TIP);

        assert_eq!(
            action,
            DaRecoveryAction::RecoverThrough {
                reorg_safe_tip: SAFE_TIP,
            }
        );
    }

    #[test]
    fn test_l1_scan_window_uses_configured_size() {
        let end_height = l1_scan_window_end(10, TEST_SCAN_WINDOW_SIZE, 20);

        assert_eq!(end_height, 12);
    }

    #[test]
    fn test_l1_scan_window_is_truncated_at_target() {
        let end_height = l1_scan_window_end(19, TEST_SCAN_WINDOW_SIZE, 20);

        assert_eq!(end_height, 20);
    }

    #[test]
    fn test_l1_scan_window_handles_terminal_height() {
        let end_height =
            l1_scan_window_end(L1Height::MAX - 1, TEST_SCAN_WINDOW_SIZE, L1Height::MAX);

        assert_eq!(end_height, L1Height::MAX);
    }

    #[test]
    fn test_transient_verifier_error_is_recoverable() {
        let error =
            DaVerifierError::FetchBitcoinTip(FetchBitcoinTipError::Rpc(ClientError::Timeout));

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_deterministic_verifier_error_is_fatal() {
        let error = DaVerifierError::DaRecovery(DaRecoveryError::NonContiguousBlocks {
            expected: 42,
            actual: 43,
        });

        assert!(!error.is_recoverable());
    }
}
