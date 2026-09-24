//! Recovers authenticated EE DA blobs from Bitcoin blocks.

use alpen_ee_da_l1_extraction::{
    fetch_l1_block_range, EeDaExtractor, EeDaScannerConfig, FetchPolicy, FetchRetryPolicy,
    RecoveredDaBlob,
};
use alpen_ee_params::AlpenParams;
use bitcoind_async_client::{traits::Reader, Client};
use eyre::Context;
use futures::{pin_mut, StreamExt};
use strata_common::retry::policies::ExponentialBackoff;
use strata_identifiers::L1Height;
use thiserror::Error;

use crate::config::EeDaToolConfig;

#[derive(Debug, Error)]
enum ReorgSafetyError {
    #[error("Bitcoin tip {tip_height} is below configured reorg-safe depth {reorg_safe_depth}")]
    TipBelowSafeDepth {
        tip_height: L1Height,
        reorg_safe_depth: u32,
    },

    #[error("scan end height {end_height} exceeds reorg-safe Bitcoin tip {reorg_safe_tip_height}")]
    EndHeightAboveSafeTip {
        end_height: L1Height,
        reorg_safe_tip_height: L1Height,
    },
}

/// Recovers decoded EE DA blobs from Bitcoin.
pub(crate) async fn recover_ee_da(
    bitcoin_client: &Client,
    params: &AlpenParams,
    config: &EeDaToolConfig,
    start_height: L1Height,
    end_height: L1Height,
) -> eyre::Result<Vec<RecoveredDaBlob>> {
    let tip_height = bitcoin_client
        .get_block_count()
        .await
        .context("failed to fetch Bitcoin tip")?;
    let tip_height = L1Height::try_from(tip_height)
        .context("Bitcoin tip exceeds the supported L1 height range")?;
    ensure_end_height_is_reorg_safe(tip_height, end_height, config.l1_reorg_safe_depth())?;

    let retry_policy = FetchRetryPolicy::new(
        config.fetch_max_retries(),
        ExponentialBackoff::new_with_default_multiplier(config.fetch_retry_delay_ms()),
    );
    let fetch_policy = FetchPolicy::new(retry_policy, config.block_fetch_concurrency());
    let blocks = fetch_l1_block_range(bitcoin_client, start_height, end_height, &fetch_policy)?;
    pin_mut!(blocks);

    let scanner_config =
        EeDaScannerConfig::new(params.blob_spec().magic_bytes(), config.sequencer_pubkey());
    let mut extractor = EeDaExtractor::new(scanner_config);
    let mut recovered_blobs = Vec::new();

    while let Some(block) = blocks.next().await {
        let block = block?;
        recovered_blobs.extend(extractor.process_block(&block)?);
    }

    Ok(recovered_blobs)
}

fn ensure_end_height_is_reorg_safe(
    tip_height: L1Height,
    end_height: L1Height,
    reorg_safe_depth: u32,
) -> Result<(), ReorgSafetyError> {
    let reorg_safe_tip_height =
        tip_height
            .checked_sub(reorg_safe_depth)
            .ok_or(ReorgSafetyError::TipBelowSafeDepth {
                tip_height,
                reorg_safe_depth,
            })?;
    if end_height > reorg_safe_tip_height {
        return Err(ReorgSafetyError::EndHeightAboveSafeTip {
            end_height,
            reorg_safe_tip_height,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_end_height_at_safe_tip_accepted() {
        ensure_end_height_is_reorg_safe(100, 94, 6).expect("scan end at reorg-safe tip accepted");
    }

    #[test]
    fn test_end_height_above_safe_tip_rejected() {
        let err = ensure_end_height_is_reorg_safe(100, 95, 6)
            .expect_err("scan end above reorg-safe tip rejected");

        assert!(matches!(
            err,
            ReorgSafetyError::EndHeightAboveSafeTip {
                end_height: 95,
                reorg_safe_tip_height: 94,
            }
        ));
    }

    #[test]
    fn test_reorg_safe_depth_above_tip_rejected() {
        let err = ensure_end_height_is_reorg_safe(5, 0, 6)
            .expect_err("reorg-safe depth above tip rejected");

        assert!(matches!(
            err,
            ReorgSafetyError::TipBelowSafeDepth {
                tip_height: 5,
                reorg_safe_depth: 6,
            }
        ));
    }
}
