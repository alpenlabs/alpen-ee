//! The chunk input layout the V2 guests read.
//!
//! These are this source's own types, which the V2 guests decode directly.
//! Until the V2 release ships, they may change. After it ships, a layout
//! change breaks the deployed V2 guest: copy the released types here in place
//! of the re-exports, as [`v1`] does.
//!
//! [`v1`]: super::v1

pub use alpen_chunk_runtime::{PrivateInput, RawChunkData};

