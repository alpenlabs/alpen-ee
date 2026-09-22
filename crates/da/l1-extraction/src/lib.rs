//! Fetches bounded L1 block ranges and incrementally extracts EE DA payloads.
//!
//! Fetch preserves L1 height order. Scanning authenticates recovered
//! payloads against the configured EE sequencer key.

mod decode;
mod fetch;
mod scan;

pub use decode::{decode_observed_da_payload, DaDecodeError, RecoveredDaBlob};
pub use fetch::{
    fetch_l1_block_range, FetchBlockError, FetchPolicy, FetchRangeError, FetchRetryPolicy,
    L1BlockData, L1BlockFetcher,
};
pub use scan::{DaL1Observation, DaL1Ref, DaScanner, DaScannerConfig};
