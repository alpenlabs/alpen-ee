//! Test support for the storage traits in `alpen-common`.
//!
//! Holds [`InMemoryStorage`], the acceptance suites every store implementing
//! [`BatchStorage`](alpen_common::BatchStorage) or
//! [`ChunkStorage`](alpen_common::ChunkStorage) must pass, and fixtures for
//! tests that need plausible exec block records.
//!
//! Run a suite against a store with [`batch_storage_tests!`] or
//! [`chunk_storage_tests!`]; the calling crate needs `tokio` for the generated
//! `#[tokio::test]` functions.

#[doc(hidden)]
pub mod batch;
#[doc(hidden)]
pub mod chunk;
mod fixtures;
mod in_memory;

pub use fixtures::{create_exec_block, create_exec_block_with_messages, create_message_entry};
pub use in_memory::InMemoryStorage;

#[cfg(test)]
mod tests {
    use crate::{batch_storage_tests, chunk_storage_tests, InMemoryStorage};

    batch_storage_tests!(InMemoryStorage::new_empty());
    chunk_storage_tests!(InMemoryStorage::new_empty());
}
