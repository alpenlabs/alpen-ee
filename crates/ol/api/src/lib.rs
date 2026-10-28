//! The EE's view of the OL chain: the client traits and the data they return.

mod chain_status;
mod client;
mod epoch_summary;

pub use chain_status::OLChainStatus;
pub use client::{
    account_block_summaries_checked, chain_status_checked, AccountBlockSummary, AccountState,
    OLBlockClient, OLBlockHeader, OLClientError, OLEpochClient, OLSubmitClient,
};
#[cfg(feature = "test-utils")]
pub use client::{MockOLBlockClient, MockOLEpochClient};
pub use epoch_summary::{AccountEpochSummary, AccountUpdate};
