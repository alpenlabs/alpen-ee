use async_trait::async_trait;

use crate::{ForkActivationRecord, StorageError};

/// Storage for runtime-derived fork activations.
///
/// Activations are derived at the VK-update boundary and must be durable
/// before any block at or past the activation is durable, so a restarted
/// node rehydrates its live chainspec from this table before executing or
/// building anything.
#[cfg_attr(feature = "test-utils", mockall::automock)]
#[async_trait]
pub trait ForkScheduleStorage: Send + Sync {
    /// Persists a derived fork activation. Overwriting an entry with the
    /// same content is a no-op; the caller guarantees it never rewrites an
    /// activation with different content (derivations are deterministic).
    async fn save_fork_activation(&self, record: ForkActivationRecord) -> Result<(), StorageError>;

    /// Returns all persisted fork activations.
    async fn get_fork_activations(&self) -> Result<Vec<ForkActivationRecord>, StorageError>;
}
