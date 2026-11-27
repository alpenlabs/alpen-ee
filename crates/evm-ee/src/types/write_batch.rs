//! EVM write batch implementation.

use alloy_consensus::Header;
use alloy_primitives::{keccak256, map::B256Set};
use evm2::block::BundleState;
use reth_primitives_traits::SealedHeader;
use reth_trie_common::{HashedPostState, KeccakKeyHasher};
use strata_codec::{Codec, CodecError};

use crate::codec_shims::{
    decode_hashed_post_state, decode_rlp_with_length, encode_hashed_post_state,
    encode_rlp_with_length,
};

/// Write batch of one executed EVM block.
///
/// This holds Reth's HashedPostState, the changes to account states and
/// storage slots the block made, and the block's sealed header. Merging it
/// applies the state changes and records the block, so later blocks in the
/// chunk can build on it and read its hash with `BLOCKHASH`.
#[derive(Clone, Debug)]
pub struct EvmWriteBatch {
    hashed_post_state: HashedPostState,
    wiped_storage: B256Set,
    header: SealedHeader<Header>,
}

impl EvmWriteBatch {
    /// Creates a write batch from a block's state changes and its header.
    pub fn new(hashed_post_state: HashedPostState, header: SealedHeader<Header>) -> Self {
        Self {
            hashed_post_state,
            wiped_storage: B256Set::default(),
            header,
        }
    }

    /// Creates the write batch of a block's execution from the state changes
    /// it made.
    pub fn from_bundle_state(state: &BundleState, header: SealedHeader<Header>) -> Self {
        let mut batch = Self::new(
            HashedPostState::from_bundle_state::<KeccakKeyHasher>(&state.state),
            header,
        );
        // Reth 2.11 tracks the wipe in BundleState, not in HashedStorage.
        // Keep the old wire flag for surviving accounts and retain empty wipe entries.
        for (address, account) in &state.state {
            if account.was_destroyed() && account.info.is_some() {
                let hashed = keccak256(address);
                batch.wiped_storage.insert(hashed);
                batch.hashed_post_state.storages.entry(hashed).or_default();
            }
        }
        batch
    }

    /// Gets the header of the block that made these changes.
    pub fn header(&self) -> &SealedHeader<Header> {
        &self.header
    }

    /// Gets a reference to the underlying HashedPostState.
    pub fn hashed_post_state(&self) -> &HashedPostState {
        &self.hashed_post_state
    }

    pub(crate) const fn wiped_storage(&self) -> &B256Set {
        &self.wiped_storage
    }

    /// Consumes self and returns the underlying HashedPostState.
    pub fn into_hashed_post_state(self) -> HashedPostState {
        self.hashed_post_state
    }
}

impl Codec for EvmWriteBatch {
    fn encode(&self, enc: &mut impl strata_codec::Encoder) -> Result<(), CodecError> {
        encode_hashed_post_state(&self.hashed_post_state, &self.wiped_storage, enc)?;
        encode_rlp_with_length(self.header.header(), enc)
    }

    fn decode(dec: &mut impl strata_codec::Decoder) -> Result<Self, CodecError> {
        let (hashed_post_state, wiped_storage) = decode_hashed_post_state(dec)?;
        let header = decode_rlp_with_length(dec)?;
        Ok(Self {
            hashed_post_state,
            wiped_storage,
            header: SealedHeader::new_unhashed(header),
        })
    }
}
