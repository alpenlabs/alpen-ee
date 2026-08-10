//! MDBX environment configuration.

/// One gibibyte, in bytes.
pub const GIB: usize = 1024 * 1024 * 1024;

/// One tebibyte, in bytes.
pub const TIB: usize = 1024 * GIB;

/// The durability mode MDBX uses when committing a write transaction.
///
/// See the MDBX crash-safety review for the guarantees each mode provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdbxSyncMode {
    /// fsync data and meta on every commit. Loses nothing on crash; slowest.
    /// The no-regret default.
    Durable,
    /// Defer fsync; a crash may lose the last N commits but **never** corrupts
    /// (MDBX `SAFE_NOSYNC`, not LMDB's unsafe `NOSYNC`). Safe here because
    /// Tier-2 data is rebuildable. Requires periodic [`MdbxEnv::sync`] flushes.
    ///
    /// [`MdbxEnv::sync`]: crate::MdbxEnv::sync
    SafeNoSync,
}

/// Configuration for opening an [`MdbxEnv`](crate::MdbxEnv).
///
/// The defaults target a production sequencer: `DURABLE` sync, non-`WRITEMAP`
/// (clean `ENOSPC` instead of `SIGBUS`, no stray-pointer corruption vector), and
/// a generous sparse geometry.
///
/// The long-reader → freelist-stall → `MAP_FULL` footgun is addressed
/// structurally rather than by a timeout: every read transaction is scoped to a
/// single synchronous [`MdbxEnv::view`](crate::MdbxEnv::view) call and is never
/// held across an `await` or slow work, so readers are always short-lived.
/// `signet-libmdbx` does not expose a duration-based reader timeout; its native
/// escape valve is the Handle-Slow-Readers callback, which can be wired in later
/// if a pathological reader ever needs to be evicted under freelist pressure.
#[derive(Debug, Clone)]
pub struct MdbxConfig {
    /// Maximum number of named sub-databases (tables) in the environment.
    pub max_dbs: usize,
    /// Maximum number of concurrent reader slots.
    pub max_readers: u64,
    /// Upper bound of the map, in bytes. Sparse — reserves address space, not
    /// disk. Hitting it is a hard `MAP_FULL`, so set it generously.
    pub max_size: usize,
    /// Geometry growth step, in bytes.
    pub growth_step: isize,
    /// Explicit page size in bytes, or [`None`] to let MDBX pick the system
    /// default (typically 4 KiB).
    pub page_size: Option<usize>,
    /// Commit durability mode.
    pub sync_mode: MdbxSyncMode,
}

impl Default for MdbxConfig {
    fn default() -> Self {
        Self {
            max_dbs: 64,
            max_readers: 1024,
            max_size: 2 * TIB,
            growth_step: (4 * GIB) as isize,
            page_size: None,
            sync_mode: MdbxSyncMode::Durable,
        }
    }
}

impl MdbxConfig {
    /// A small-geometry configuration for tests and tooling: a 1 GiB map with a
    /// 16 MiB growth step. Still `DURABLE`.
    pub fn small() -> Self {
        Self {
            max_size: GIB,
            growth_step: (16 * 1024 * 1024) as isize,
            ..Default::default()
        }
    }
}
