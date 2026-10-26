//! Test support for the storage traits, behind the `test-utils` feature.
//!
//! Holds [`InMemoryStorage`], the acceptance suites every store implementing
//! [`BatchStorage`](crate::BatchStorage) or [`ChunkStorage`](crate::ChunkStorage)
//! must pass, and fixtures for tests that need plausible exec block records.
//!
//! Run a suite against a store with [`batch_storage_tests!`](crate::batch_storage_tests)
//! or [`chunk_storage_tests!`](crate::chunk_storage_tests); the calling crate needs
//! `tokio` for the generated `#[tokio::test]` functions.

#[doc(hidden)]
pub mod batch;
#[doc(hidden)]
pub mod chunk;
// The fixtures build node records, not storage. They belong with the node
// types' test support once those types leave alpen-common.
mod fixtures;
mod in_memory;

pub use fixtures::{create_exec_block, create_exec_block_with_messages, create_message_entry};
pub use in_memory::InMemoryStorage;

#[cfg(test)]
mod tests {
    use super::InMemoryStorage;
    use crate::{batch_storage_tests, chunk_storage_tests};

    batch_storage_tests!(InMemoryStorage::new_empty());
    chunk_storage_tests!(InMemoryStorage::new_empty());
}
