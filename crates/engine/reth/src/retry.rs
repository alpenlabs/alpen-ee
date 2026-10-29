//! Retry settings for calls into reth.

use strata_retry::ExponentialBackoff;

/// Retries after the first attempt for a call into reth.
pub(crate) const ENGINE_CALL_MAX_RETRIES: u16 = 4;

/// Returns the backoff for calls into reth: 1.5 s at first, then 1.5x per retry.
pub(crate) fn engine_call_backoff() -> ExponentialBackoff {
    ExponentialBackoff::new(1_500, 15, 10, None)
}
