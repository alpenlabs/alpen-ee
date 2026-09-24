//! Drives incremental EE DA extraction and persistence.

use alpen_da_l1_extraction::{DaExtractor, FetchBlockError, FetchRangeError, RecoveredDaBlob};
use alpen_database::RecoveredDaDbError;
use futures::{pin_mut, StreamExt};
use strata_identifiers::L1Height;
use thiserror::Error;
use tracing::debug;

use crate::context::DaVerifierContext;

/// Failure to recover and persist EE DA over an L1 range.
#[derive(Debug, Error)]
pub(crate) enum DaRecoveryError {
    /// The requested recovery range is invalid.
    #[error(transparent)]
    FetchRange(#[from] FetchRangeError),

    /// Fetching an L1 block failed.
    #[error(transparent)]
    FetchBlock(#[from] FetchBlockError),

    /// Persisting recovered EE DA failed.
    #[error(transparent)]
    Database(#[from] RecoveredDaDbError),

    /// The fetched block stream skipped or repeated an L1 height.
    #[error("non-contiguous L1 block stream (expected height {expected}, got {actual})")]
    NonContiguousBlocks {
        expected: L1Height,
        actual: L1Height,
    },

    /// The fetched block stream ended before the requested inclusive range did.
    #[error(
        "L1 block stream ended before requested range (next height {next_height}, end height {end_height})"
    )]
    IncompleteBlockRange {
        next_height: L1Height,
        end_height: L1Height,
    },

    /// The in-memory recovery cursor cannot represent the next L1 height.
    #[error("EE DA recovery cannot advance past terminal L1 height {height}")]
    TerminalHeight { height: L1Height },
}

impl DaRecoveryError {
    /// Returns whether a later verification cycle may succeed without intervention.
    pub(crate) fn is_recoverable(&self) -> bool {
        match self {
            Self::FetchBlock(FetchBlockError::RetriesExhausted { .. }) => true,
            Self::Database(error) => error.is_recoverable(),
            Self::FetchRange(_)
            | Self::FetchBlock(FetchBlockError::Client { .. })
            | Self::NonContiguousBlocks { .. }
            | Self::IncompleteBlockRange { .. }
            | Self::TerminalHeight { .. } => false,
        }
    }
}

/// Recovered blobs awaiting durable persistence for one processed L1 block.
#[derive(Debug)]
struct PendingRecoveredDa {
    block_height: L1Height,
    recovered_blobs: Vec<RecoveredDaBlob>,
}

impl PendingRecoveredDa {
    fn new(block_height: L1Height, recovered_blobs: Vec<RecoveredDaBlob>) -> Self {
        Self {
            block_height,
            recovered_blobs,
        }
    }
}

/// Owns the L1 scan position, parser state, and any write awaiting persistence,
/// which advance together.
#[derive(Debug)]
pub(crate) struct DaRecoveryDriver {
    extractor: DaExtractor,

    /// Blobs recovered from one L1 block, held until they are persisted.
    ///
    /// A restart drops them, so the rescan must cover the blocks they were
    /// extracted from; otherwise the sequence gap never closes.
    pending_recovered_da: Option<PendingRecoveredDa>,

    next_l1_height: L1Height,
}

impl DaRecoveryDriver {
    /// Creates a recovery driver positioned at `next_l1_height`.
    pub(crate) fn new(extractor: DaExtractor, next_l1_height: L1Height) -> Self {
        Self {
            extractor,
            pending_recovered_da: None,
            next_l1_height,
        }
    }

    /// Returns the next L1 height to process.
    pub(crate) fn next_l1_height(&self) -> L1Height {
        self.next_l1_height
    }

    /// Persists pending recovered DA and advances the scan cursor after success.
    pub(crate) async fn flush_pending(
        &mut self,
        context: &impl DaVerifierContext,
    ) -> Result<(), DaRecoveryError> {
        let Some(pending) = self.pending_recovered_da.as_ref() else {
            return Ok(());
        };

        let block_height = pending.block_height;
        self.next_l1_height_after(block_height)?;
        // The store consumes its input, so retain a copy for a failed write retry.
        let recovered_blobs = pending.recovered_blobs.clone();
        context.put_recovered_da(recovered_blobs).await?;
        self.advance_cursor_past(block_height)?;
        self.pending_recovered_da = None;
        Ok(())
    }

    /// Recovers and persists EE DA through an inclusive L1 height.
    ///
    /// Advances the cursor after each processed block. A block containing
    /// recovered DA advances only after its blobs are persisted.
    ///
    /// # Panics
    ///
    /// Panics if recovered DA from an earlier block is still awaiting persistence.
    pub(crate) async fn recover_through(
        &mut self,
        context: &impl DaVerifierContext,
        end_height: L1Height,
    ) -> Result<(), DaRecoveryError> {
        // The cycle flushes pending data before deciding whether a range needs recovery.
        assert!(
            self.pending_recovered_da.is_none(),
            "pending recovered DA must be persisted before processing more blocks"
        );

        let blocks = context.fetch_l1_block_range(self.next_l1_height, end_height)?;
        pin_mut!(blocks);

        while let Some(block) = blocks.next().await {
            let block = block?;
            let height = block.height();
            self.next_l1_height_after(height)?;
            let recovered_blobs = self.extractor.process_block(&block);
            let recovered_blob_count = recovered_blobs.len();
            if recovered_blobs.is_empty() {
                self.advance_cursor_past(height)?;
            } else {
                self.pending_recovered_da = Some(PendingRecoveredDa::new(height, recovered_blobs));
                self.flush_pending(context).await?;
            }
            debug!(
                height,
                recovered_blob_count, "processed L1 block for EE DA recovery"
            );
        }

        if self.next_l1_height <= end_height {
            return Err(DaRecoveryError::IncompleteBlockRange {
                next_height: self.next_l1_height,
                end_height,
            });
        }

        Ok(())
    }

    fn advance_cursor_past(&mut self, block_height: L1Height) -> Result<(), DaRecoveryError> {
        self.next_l1_height = self.next_l1_height_after(block_height)?;
        Ok(())
    }

    fn next_l1_height_after(&self, block_height: L1Height) -> Result<L1Height, DaRecoveryError> {
        if block_height != self.next_l1_height {
            return Err(DaRecoveryError::NonContiguousBlocks {
                expected: self.next_l1_height,
                actual: block_height,
            });
        }

        block_height
            .checked_add(1)
            .ok_or(DaRecoveryError::TerminalHeight {
                height: block_height,
            })
    }
}

#[cfg(test)]
mod tests {
    use bitcoind_async_client::error::ClientError;

    use super::*;

    #[test]
    fn test_exhausted_block_fetch_is_recoverable() {
        let error = DaRecoveryError::FetchBlock(FetchBlockError::RetriesExhausted {
            height: 42,
            max_retries: 3,
            source: ClientError::Timeout,
        });

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_non_retryable_block_fetch_is_fatal() {
        let error = DaRecoveryError::FetchBlock(FetchBlockError::Client {
            height: 42,
            source: ClientError::MissingUserPassword,
        });

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_recoverable_database_failure_is_forwarded() {
        let error = DaRecoveryError::Database(RecoveredDaDbError::WorkerCancelled);

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_fatal_database_failure_is_forwarded() {
        let error =
            DaRecoveryError::Database(RecoveredDaDbError::WorkerPanicked("panic".to_owned()));

        assert!(!error.is_recoverable());
    }
}
