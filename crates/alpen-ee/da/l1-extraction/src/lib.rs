//! Fetches bounded L1 block ranges and incrementally extracts EE DA payloads.
//!
//! Fetch preserves L1 height order. Scanning authenticates recovered
//! payloads against the configured EE sequencer key.

mod decode;
mod extract;
mod fetch;
mod scan;
#[cfg(test)]
mod test_utils;

pub use decode::{decode_observed_da_payload, DaDecodeError, RecoveredDaBlob};
pub use extract::EeDaExtractor;
pub use fetch::{
    fetch_l1_block_range, FetchBlockError, FetchPolicy, FetchRangeError, FetchRetryPolicy,
    L1BlockData, L1BlockFetcher,
};
pub use scan::{EeDaL1Observation, EeDaL1Ref, EeDaScanner, EeDaScannerConfig};
