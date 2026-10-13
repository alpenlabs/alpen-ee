//! EVM write batch implementation.

use alloy_consensus::Header;
use reth_trie::{HashedPostState, KeccakKeyHasher};
use revm::database::BundleState;
use strata_codec::{Codec, CodecError};

use crate::codec_shims::{
    decode_hashed_post_state, decode_rlp_with_length, encode_hashed_post_state,
    encode_rlp_with_length,
};

/// Write batch of one executed EVM block.
///
/// This holds Reth's HashedPostState, the changes to account states and
/// storage slots the block made, and the block's header. Merging it applies
/// the state changes and records the block, so later blocks in the chunk can
/// build on it and read its hash with `BLOCKHASH`.
#[derive(Clone, Debug)]
pub struct EvmWriteBatch {
    hashed_post_state: HashedPostState,
    header: Header,
}

impl EvmWriteBatch {
    /// Creates a write batch from a block's state changes and its header.
    pub fn new(hashed_post_state: HashedPostState, header: Header) -> Self {
        Self {
            hashed_post_state,
            header,
        }
    }

    /// Creates the write batch of a block's execution from the state changes
    /// it made.
    pub fn from_bundle_state(state: &BundleState, header: Header) -> Self {
        Self::new(
            HashedPostState::from_bundle_state::<KeccakKeyHasher>(&state.state),
            header,
        )
    }

    /// Gets the header of the block that made these changes.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Gets a reference to the underlying HashedPostState.
    pub fn hashed_post_state(&self) -> &HashedPostState {
        &self.hashed_post_state
    }

    /// Consumes self and returns the underlying HashedPostState.
    pub fn into_hashed_post_state(self) -> HashedPostState {
        self.hashed_post_state
    }
}

impl Codec for EvmWriteBatch {
    fn encode(&self, enc: &mut impl strata_codec::Encoder) -> Result<(), CodecError> {
        encode_hashed_post_state(&self.hashed_post_state, enc)?;
        encode_rlp_with_length(&self.header, enc)
    }

    fn decode(dec: &mut impl strata_codec::Decoder) -> Result<Self, CodecError> {
        let hashed_post_state = decode_hashed_post_state(dec)?;
        let header = decode_rlp_with_length(dec)?;
        Ok(Self::new(hashed_post_state, header))
    }
}
