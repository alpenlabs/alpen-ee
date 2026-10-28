//! The EE's view of the OL chain: the client traits and the data they return.

#[cfg(test)]
use mockall as _;

mod chain_status;
mod client;
mod epoch_summary;

pub use chain_status::OLChainStatus;
pub use client::{
    chain_status_checked, get_inbox_messages_checked, OLAccountStateView, OLBlockData, OLBlockLink,
    OLClient, OLClientError, SequencerOLClient,
};
#[cfg(feature = "test-utils")]
pub use client::{MockOLClient, MockSequencerOLClient};
pub use epoch_summary::{SnarkAccountEpochSummary, SnarkAccountUpdateInfo};
