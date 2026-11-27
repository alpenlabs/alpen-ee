//! Codec helper functions for encoding/decoding with length prefixes.
//!
//! This module provides utility functions for encoding and decoding data with length prefixes,
//! useful for variable-length fields in the EVM execution environment types.
//!
//! It also provides custom encodings for external RSP and Reth types that do not implement
//! [`Codec`]. MPT nodes use bincode because RSP does not expose the node internals required for a
//! custom lossless encoding.

use std::{collections::BTreeMap, io};

use alloy_consensus::constants::EMPTY_ROOT_HASH;
use reth_trie::{HashedPostState, TrieAccount};
use revm_primitives::{B256, HashMap as RevmHashMap};
use rsp_mpt::{EthereumState, MptNode};
use strata_codec::{Codec, CodecError, Varint};

/// Adapts a [`strata_codec::Encoder`] to [`io::Write`] while preserving encoder failures.
///
/// Retains the first [`CodecError`] so the caller can recover the typed encoder failure after
/// [`io::Write`] reports it as an [`io::Error`].
struct EncoderWriter<'a, E> {
    encoder: &'a mut E,
    encoder_error: Option<CodecError>,
}

impl<'a, E> EncoderWriter<'a, E> {
    fn new(encoder: &'a mut E) -> Self {
        Self {
            encoder,
            encoder_error: None,
        }
    }

    /// Consumes the adapter and returns the first error produced by the wrapped encoder.
    ///
    /// Returns [`Ok`] if all forwarded writes succeeded. Serializer failures are reported
    /// separately by the serializer.
    fn into_encoder_result(self) -> Result<(), CodecError> {
        match self.encoder_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl<E: strata_codec::Encoder> io::Write for EncoderWriter<'_, E> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.encoder.write_buf(buf) {
            Ok(()) => Ok(buf.len()),
            Err(error) => {
                // Preserve the encoder failure instead of exposing bincode's synthetic I/O error.
                if self.encoder_error.is_none() {
                    self.encoder_error = Some(error);
                }
                Err(io::Error::other("encoder rejected the written buffer"))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Encodes an RLP-encodable item with a varint length prefix.
///
/// This encodes the item using RLP, then writes a varint length prefix followed by the RLP bytes.
/// Varints are more space-efficient for small lengths.
pub(crate) fn encode_rlp_with_length<T: alloy_rlp::Encodable>(
    item: &T,
    enc: &mut impl strata_codec::Encoder,
) -> Result<(), CodecError> {
    let rlp_encoded = alloy_rlp::encode(item);
    let len = Varint::new_usize(rlp_encoded.len()).ok_or(CodecError::OverflowContainer)?;
    len.encode(enc)?;
    enc.write_buf(&rlp_encoded)?;
    Ok(())
}

/// Decodes an RLP-decodable item with a varint length prefix.
///
/// This reads a varint length prefix, then reads that many bytes and decodes them using RLP.
pub(crate) fn decode_rlp_with_length<T: alloy_rlp::Decodable>(
    dec: &mut impl strata_codec::Decoder,
) -> Result<T, CodecError> {
    let len_varint = Varint::decode(dec)?;
    let len = len_varint.inner() as usize;
    let mut buf = vec![0u8; len];
    dec.read_buf(&mut buf)?;

    alloy_rlp::Decodable::decode(&mut &buf[..])
        .map_err(|_| CodecError::MalformedField("RLP decode failed"))
}

/// Encodes raw bytes with a varint length prefix.
///
/// This writes a varint length prefix followed by the raw bytes.
/// Varints are more space-efficient for small lengths.
pub(crate) fn encode_bytes_with_length(
    bytes: &[u8],
    enc: &mut impl strata_codec::Encoder,
) -> Result<(), CodecError> {
    let len = Varint::new_usize(bytes.len()).ok_or(CodecError::OverflowContainer)?;
    len.encode(enc)?;
    enc.write_buf(bytes)?;
    Ok(())
}

// Use the same default bincode configuration for sizing and serialization to preserve framing.
fn encode_mpt_node_with_length(
    node: &MptNode,
    enc: &mut impl strata_codec::Encoder,
) -> Result<(), CodecError> {
    let size =
        bincode::serialized_size(node).map_err(|_| CodecError::MalformedField("MPT node"))?;
    let encoded_size = usize::try_from(size).map_err(|_| CodecError::OverflowContainer)?;
    let len = Varint::new_usize(encoded_size).ok_or(CodecError::OverflowContainer)?;
    len.encode(enc)?;

    let mut writer = EncoderWriter::new(enc);
    let serialization_result = bincode::serialize_into(&mut writer, node);
    writer.into_encoder_result()?;
    serialization_result.map_err(|_| CodecError::MalformedField("MPT node"))
}

/// Decodes raw bytes with a varint length prefix.
///
/// This reads a varint length prefix, then reads that many bytes and returns them as a Vec<u8>.
pub(crate) fn decode_bytes_with_length(
    dec: &mut impl strata_codec::Decoder,
) -> Result<Vec<u8>, CodecError> {
    let len_varint = Varint::decode(dec)?;
    let len = len_varint.inner() as usize;
    let mut bytes = vec![0u8; len];
    dec.read_buf(&mut bytes)?;
    Ok(bytes)
}

// ============================================================================
// Custom deterministic encoding for external types (RSP, Reth)
// ============================================================================

/// Encodes EthereumState deterministically (sorts HashMap entries).
///
/// Uses bincode (serde) for MptNode serialization to preserve all resolved trie
/// nodes. The previous RLP-based encoding was lossy: `alloy_rlp::Encodable` for
/// MptNode collapses child nodes >= 32 bytes into hash digests via
/// `reference_encode`, making the decoded state unusable for block execution.
///
/// Bincode is used here as an implementation detail because `MptNode` and
/// `MptNodeData` are not publicly accessible from `rsp_mpt` (private module),
/// preventing a custom recursive strata-codec encoding.
pub fn encode_ethereum_state(
    state: &EthereumState,
    enc: &mut impl strata_codec::Encoder,
) -> Result<(), CodecError> {
    encode_mpt_node_with_length(&state.state_trie, enc)?;

    // Sort storage_tries by key for deterministic encoding
    let sorted_storage: BTreeMap<_, _> = state.storage_tries.iter().collect();
    (sorted_storage.len() as u32).encode(enc)?;

    for (address_hash, storage_trie) in sorted_storage {
        enc.write_buf(address_hash.as_slice())?;
        encode_mpt_node_with_length(storage_trie, enc)?;
    }

    Ok(())
}

/// Decodes an [`EthereumState`] using a [`strata_codec::Decoder`].
///
/// The caller owns outer framing and any exact-input check.
pub fn decode_ethereum_state(
    dec: &mut impl strata_codec::Decoder,
) -> Result<EthereumState, CodecError> {
    let state_trie_bytes = decode_bytes_with_length(dec)?;
    let state_trie = bincode::deserialize(&state_trie_bytes)
        .map_err(|_| CodecError::MalformedField("state_trie bincode decode failed"))?;

    // Decode storage_tries
    let storage_count = u32::decode(dec)? as usize;
    let mut storage_tries =
        RevmHashMap::with_capacity_and_hasher(storage_count, Default::default());

    for _ in 0..storage_count {
        let mut address_hash_bytes = [0u8; 32];
        dec.read_buf(&mut address_hash_bytes)?;
        let address_hash = B256::from(address_hash_bytes);

        let storage_trie_bytes = decode_bytes_with_length(dec)?;
        let storage_trie = bincode::deserialize(&storage_trie_bytes)
            .map_err(|_| CodecError::MalformedField("storage_trie bincode decode failed"))?;
        storage_tries.insert(address_hash, storage_trie);
    }

    let state = EthereumState {
        state_trie,
        storage_tries,
    };
    validate_storage_tries(&state)?;

    Ok(state)
}

/// Checks each storage trie against the storage root its account commits to.
///
/// The state root commits to the account leaves, and each leaf commits to a
/// storage root -- but nothing ties the storage tries carried alongside to
/// those roots. Without this check a prover could hand the guest a storage
/// trie holding values the account never had, and `SLOAD` would read them.
fn validate_storage_tries(state: &EthereumState) -> Result<(), CodecError> {
    for (hashed_address, storage_trie) in state.storage_tries.iter() {
        let account = state
            .state_trie
            .get_rlp::<TrieAccount>(hashed_address.as_slice())
            .map_err(|_| CodecError::MalformedField("account not resolvable in state trie"))?;

        // An account absent from the state trie has no storage, so the only
        // trie it can carry is an empty one.
        let expected_root = account.map_or(EMPTY_ROOT_HASH, |account| account.storage_root);
        if expected_root != storage_trie.hash() {
            return Err(CodecError::MalformedField("storage root mismatch"));
        }
    }

    Ok(())
}

/// Encodes HashedPostState deterministically (sorts HashMap entries).
///
/// HashedPostState contains two HashMaps (accounts and storages) which we sort
/// by key before encoding to ensure deterministic iteration order.
pub(crate) fn encode_hashed_post_state(
    state: &HashedPostState,
    enc: &mut impl strata_codec::Encoder,
) -> Result<(), CodecError> {
    // Sort accounts by key for deterministic encoding
    let sorted_accounts: BTreeMap<_, _> = state.accounts.iter().collect();
    (sorted_accounts.len() as u32).encode(enc)?;

    for (address_hash, account_opt) in sorted_accounts {
        enc.write_buf(address_hash.as_slice())?;

        // Encode Option<Account>
        match account_opt {
            Some(account) => {
                true.encode(enc)?;
                // Encode Account fields
                account.nonce.encode(enc)?;
                enc.write_buf(account.balance.as_le_slice())?;
                match &account.bytecode_hash {
                    Some(hash) => {
                        true.encode(enc)?;
                        enc.write_buf(hash.as_slice())?;
                    }
                    None => {
                        false.encode(enc)?;
                    }
                }
            }
            None => {
                false.encode(enc)?;
            }
        }
    }

    // Sort storages by key for deterministic encoding
    let sorted_storages: BTreeMap<_, _> = state.storages.iter().collect();
    (sorted_storages.len() as u32).encode(enc)?;

    for (address_hash, hashed_storage) in sorted_storages {
        enc.write_buf(address_hash.as_slice())?;

        // Encode HashedStorage
        hashed_storage.wiped.encode(enc)?;

        // Sort storage slots by key
        let sorted_storage_slots: BTreeMap<_, _> = hashed_storage.storage.iter().collect();
        (sorted_storage_slots.len() as u32).encode(enc)?;

        for (slot_hash, value) in sorted_storage_slots {
            enc.write_buf(slot_hash.as_slice())?;
            enc.write_buf(value.as_le_slice())?;
        }
    }

    Ok(())
}

/// Decodes HashedPostState.
pub(crate) fn decode_hashed_post_state(
    dec: &mut impl strata_codec::Decoder,
) -> Result<HashedPostState, CodecError> {
    use reth_primitives_traits::Account;
    use reth_trie::HashedStorage;
    use revm_primitives::U256;

    // Start with empty HashedPostState
    let mut state = HashedPostState::default();

    // Decode accounts
    let accounts_count = u32::decode(dec)? as usize;

    for _ in 0..accounts_count {
        let mut address_hash_bytes = [0u8; 32];
        dec.read_buf(&mut address_hash_bytes)?;
        let address_hash = B256::from(address_hash_bytes);

        let has_account = bool::decode(dec)?;
        let account_opt = if has_account {
            let nonce = u64::decode(dec)?;

            let mut balance_bytes = [0u8; 32];
            dec.read_buf(&mut balance_bytes)?;
            let balance = U256::from_le_slice(&balance_bytes);

            let has_bytecode = bool::decode(dec)?;
            let bytecode_hash = if has_bytecode {
                let mut hash_bytes = [0u8; 32];
                dec.read_buf(&mut hash_bytes)?;
                Some(B256::from(hash_bytes))
            } else {
                None
            };

            Some(Account {
                nonce,
                balance,
                bytecode_hash,
            })
        } else {
            None
        };

        state.accounts.insert(address_hash, account_opt);
    }

    // Decode storages
    let storages_count = u32::decode(dec)? as usize;

    for _ in 0..storages_count {
        let mut address_hash_bytes = [0u8; 32];
        dec.read_buf(&mut address_hash_bytes)?;
        let address_hash = B256::from(address_hash_bytes);

        let wiped = bool::decode(dec)?;

        let storage_slots_count = u32::decode(dec)? as usize;
        let mut hashed_storage = HashedStorage::default();

        for _ in 0..storage_slots_count {
            let mut slot_hash_bytes = [0u8; 32];
            dec.read_buf(&mut slot_hash_bytes)?;
            let slot_hash = B256::from(slot_hash_bytes);

            let mut value_bytes = [0u8; 32];
            dec.read_buf(&mut value_bytes)?;
            let value = U256::from_le_slice(&value_bytes);

            hashed_storage.storage.insert(slot_hash, value);
        }

        hashed_storage.wiped = wiped;
        state.storages.insert(address_hash, hashed_storage);
    }

    Ok(state)
}

#[cfg(test)]
mod tests {
    use alloy_rpc_types_debug::ExecutionWitness;
    use reth_primitives_traits::Account;
    use reth_trie::HashedStorage;
    use revm_primitives::{Address, Bytes, U256, keccak256};

    use super::*;

    struct FailAfterLengthPrefix {
        writes: usize,
    }

    impl strata_codec::Encoder for FailAfterLengthPrefix {
        fn write_buf(&mut self, _buf: &[u8]) -> Result<(), CodecError> {
            self.writes += 1;
            if self.writes == 1 {
                Ok(())
            } else {
                Err(CodecError::OverrunInput)
            }
        }
    }

    // Keep the previous algorithm here to pin the encoding while changing its allocation shape.
    fn encode_ethereum_state_buffered(
        state: &EthereumState,
        enc: &mut impl strata_codec::Encoder,
    ) -> Result<(), CodecError> {
        let trie_bytes = bincode::serialize(&state.state_trie)
            .map_err(|_| CodecError::MalformedField("state_trie bincode encode failed"))?;
        encode_bytes_with_length(&trie_bytes, enc)?;

        let sorted_storage: BTreeMap<_, _> = state.storage_tries.iter().collect();
        (sorted_storage.len() as u32).encode(enc)?;

        for (address_hash, storage_trie) in sorted_storage {
            enc.write_buf(address_hash.as_slice())?;
            let storage_bytes = bincode::serialize(storage_trie)
                .map_err(|_| CodecError::MalformedField("storage_trie bincode encode failed"))?;
            encode_bytes_with_length(&storage_bytes, enc)?;
        }

        Ok(())
    }

    // Build valid account leaves that commit to several storage tries inserted out of order.
    fn build_populated_state() -> EthereumState {
        let witness = ExecutionWitness {
            state: vec![Bytes::from_static(&[0x80])],
            ..Default::default()
        };
        let mut state = EthereumState::from_execution_witness(&witness, EMPTY_ROOT_HASH);
        let mut post_state = HashedPostState::default();

        for byte in [0x22, 0x01, 0x13] {
            let hashed_address = keccak256(Address::from([byte; 20]));
            post_state.accounts.insert(
                hashed_address,
                Some(Account {
                    nonce: u64::from(byte),
                    balance: U256::from(byte),
                    bytecode_hash: None,
                }),
            );

            let mut storage = HashedStorage::new(false);
            storage
                .storage
                .insert(keccak256([byte; 32]), U256::from(byte));
            post_state.storages.insert(hashed_address, storage);
        }

        state.update(&post_state);
        state
    }

    #[test]
    fn test_streamed_encoding_matches_buffered_encoding() {
        let state = build_populated_state();
        assert_eq!(
            state.storage_tries.len(),
            3,
            "state exercises several tries"
        );

        let mut streamed = Vec::new();
        encode_ethereum_state(&state, &mut streamed).expect("streaming encode succeeds");
        let mut buffered = Vec::new();
        encode_ethereum_state_buffered(&state, &mut buffered).expect("buffered encode succeeds");

        assert_eq!(streamed, buffered);
    }

    #[test]
    fn test_streamed_encoding_preserves_encoder_error() {
        let state = build_populated_state();
        let mut encoder = FailAfterLengthPrefix { writes: 0 };

        let error = encode_ethereum_state(&state, &mut encoder)
            .expect_err("injected encoder failure must propagate");

        assert!(matches!(error, CodecError::OverrunInput));
    }
}
