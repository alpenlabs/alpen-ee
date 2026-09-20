//! Textual key handling for console lookups.
//!
//! A console user types keys as text (`get("ExecBlockSchema", "0x1a2b…")`), so
//! every reflected table needs a way from that text to its real
//! [`Schema::Key`](alpen_store_mdbx::Schema::Key) and back. [`ConsoleKey`]
//! is that conversion, implemented once per key type rather than once per
//! table: the EE schema draws its keys from a small closed set, so a table
//! whose key is already covered gets its `get` for free.

use alloy_primitives::B256;
use alpen_ee_common::{BatchId, ChunkId};
use strata_acct_types::Hash;
use strata_db_types::fee_bump::TxNodeId;
use strata_identifiers::{Buf32, OLBlockId};

use super::value::{hex, parse_hex};
use crate::serialization_types::{DBBatchId, DBChunkId, DBOLBlockId};

/// A key type the console can parse from user text and render back.
///
/// Rendering is what `scan` reports per record and what a user can paste
/// back into `get`, so the two directions must round-trip.
pub trait ConsoleKey: Sized {
    /// Parses the key from the text a user typed.
    fn parse(input: &str) -> eyre::Result<Self>;

    /// Renders the key as the text `parse` accepts.
    fn render(&self) -> String;

    /// Parses a leading part of a key, for a prefix scan.
    ///
    /// Only a key written as hex has a meaningful prefix — the first bytes of
    /// a byte string, a hash, or a hash pair. A decimal key has none, and says
    /// so.
    fn prefix(input: &str) -> eyre::Result<Vec<u8>> {
        let _ = input;
        eyre::bail!("this key type is not written as hex, so it has no prefix form")
    }
}

/// Parses a hex prefix: any whole number of bytes, and at least one.
fn parse_prefix(input: &str, limit: usize) -> eyre::Result<Vec<u8>> {
    let bytes = parse_hex(input).map_err(|e| eyre::eyre!("bad prefix: {e}"))?;
    if bytes.is_empty() {
        eyre::bail!("a prefix needs at least one byte");
    }
    if bytes.len() > limit {
        eyre::bail!(
            "prefix is {} bytes but a key is at most {limit}: {input:?}",
            bytes.len()
        );
    }
    Ok(bytes)
}

impl ConsoleKey for Vec<u8> {
    fn parse(input: &str) -> eyre::Result<Self> {
        parse_hex(input).map_err(|e| eyre::eyre!("bad key: {e}"))
    }

    fn render(&self) -> String {
        hex(self)
    }

    fn prefix(input: &str) -> eyre::Result<Vec<u8>> {
        parse_prefix(input, usize::MAX)
    }
}

/// Parses a fixed 32-byte key, rejecting any other length up front so a
/// truncated paste reports its own length rather than a decode failure.
fn parse_hash32(input: &str) -> eyre::Result<[u8; 32]> {
    let bytes = parse_hex(input).map_err(|e| eyre::eyre!("bad key: {e}"))?;
    let len = bytes.len();
    <[u8; 32]>::try_from(bytes)
        .map_err(|_| eyre::eyre!("key must be 32 bytes, got {len}: {input:?}"))
}

/// Covers both spellings of the 32-byte identifier: `Hash` is an alias for
/// [`Buf32`], so this single impl serves every hash-keyed table.
impl ConsoleKey for Buf32 {
    fn parse(input: &str) -> eyre::Result<Self> {
        parse_hash32(input).map(Self::from)
    }

    fn render(&self) -> String {
        hex(self.as_ref())
    }

    fn prefix(input: &str) -> eyre::Result<Vec<u8>> {
        parse_prefix(input, 32)
    }
}

/// The alloy 32-byte hash, keying the witness environment's tables.
///
/// Its tables encode the key through bincode, which writes a byte string as
/// an 8-byte big-endian length and then the bytes, so every stored key starts
/// with the same header and a prefix has to start with it too. A prefix
/// without the header would sort before every key and match nothing.
impl ConsoleKey for B256 {
    fn parse(input: &str) -> eyre::Result<Self> {
        parse_hash32(input).map(Self::from)
    }

    fn render(&self) -> String {
        hex(self.as_slice())
    }

    fn prefix(input: &str) -> eyre::Result<Vec<u8>> {
        let bytes = parse_prefix(input, 32)?;
        let mut framed = 32u64.to_be_bytes().to_vec();
        framed.extend(bytes);
        Ok(framed)
    }
}

/// The L1 broadcast replacement-chain id: a 32-byte hash under another name.
impl ConsoleKey for TxNodeId {
    fn parse(input: &str) -> eyre::Result<Self> {
        parse_hash32(input).map(|raw| Self(Buf32::from(raw)))
    }

    fn render(&self) -> String {
        hex(self.0.as_ref())
    }

    fn prefix(input: &str) -> eyre::Result<Vec<u8>> {
        parse_prefix(input, 32)
    }
}

/// Parses a decimal integer key, tolerating surrounding whitespace.
macro_rules! impl_console_key_int {
    ($ty:ty) => {
        impl ConsoleKey for $ty {
            fn parse(input: &str) -> eyre::Result<Self> {
                input
                    .trim()
                    .parse::<$ty>()
                    .map_err(|e| eyre::eyre!("bad key {input:?}: {e}"))
            }

            fn render(&self) -> String {
                self.to_string()
            }
        }
    };
}

impl_console_key_int!(u32);
impl_console_key_int!(u64);

/// Splits a `prev:last` pair key into its two halves.
fn parse_pair(input: &str, what: &str) -> eyre::Result<([u8; 32], [u8; 32])> {
    let (prev, last) = input
        .split_once(':')
        .ok_or_else(|| eyre::eyre!("{what} key must be `prev_block:last_block`, got {input:?}"))?;
    Ok((parse_hash32(prev)?, parse_hash32(last)?))
}

/// Renders a `prev:last` pair key.
fn render_pair(prev: &[u8], last: &[u8]) -> String {
    format!("{}:{}", hex(prev), hex(last))
}

/// A pair key's prefix is a prefix of its first hash, so it is written as
/// plain hex with no `:`.
fn parse_pair_prefix(input: &str) -> eyre::Result<Vec<u8>> {
    if input.contains(':') {
        eyre::bail!("a pair prefix is the start of the first hash, written without `:`");
    }
    parse_prefix(input, 32)
}

impl ConsoleKey for DBBatchId {
    fn parse(input: &str) -> eyre::Result<Self> {
        let (prev, last) = parse_pair(input, "batch")?;
        Ok(BatchId::from_parts(Hash::from(prev), Hash::from(last)).into())
    }

    fn render(&self) -> String {
        let id = BatchId::from(self.clone());
        render_pair(id.prev_block().as_ref(), id.last_block().as_ref())
    }

    fn prefix(input: &str) -> eyre::Result<Vec<u8>> {
        parse_pair_prefix(input)
    }
}

impl ConsoleKey for DBChunkId {
    fn parse(input: &str) -> eyre::Result<Self> {
        let (prev, last) = parse_pair(input, "chunk")?;
        Ok(ChunkId::from_parts(Hash::from(prev), Hash::from(last)).into())
    }

    fn render(&self) -> String {
        let id = ChunkId::from(self.clone());
        render_pair(id.prev_block().as_ref(), id.last_block().as_ref())
    }

    fn prefix(input: &str) -> eyre::Result<Vec<u8>> {
        parse_pair_prefix(input)
    }
}

impl ConsoleKey for DBOLBlockId {
    fn parse(input: &str) -> eyre::Result<Self> {
        let raw = parse_hash32(input)?;
        Ok(OLBlockId::from(Buf32::from(raw)).into())
    }

    fn render(&self) -> String {
        let id = OLBlockId::from(self.clone());
        hex(Buf32::from(id).as_ref())
    }

    fn prefix(input: &str) -> eyre::Result<Vec<u8>> {
        parse_prefix(input, 32)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use alpen_reth_db::mdbx::BlockStateChangesSchema;
    use alpen_store_mdbx::KeyCodec;

    use super::*;

    /// Whatever `render` prints must parse back to the same key, or a key
    /// copied out of a scan cannot be pasted into a `get`.
    fn round_trip<K: ConsoleKey + PartialEq + Debug>(key: K) {
        let rendered = key.render();
        let parsed = K::parse(&rendered).expect("rendered key parses");
        assert_eq!(parsed, key, "round-trip changed the key (via {rendered:?})");
    }

    #[test]
    fn byte_and_hash_keys_round_trip() {
        round_trip(vec![0xde, 0xad, 0xbe, 0xef]);
        round_trip(Hash::from([7u8; 32]));
    }

    #[test]
    fn integer_keys_round_trip() {
        round_trip(0u32);
        round_trip(4_294_967_295u32);
        round_trip(18_446_744_073_709_551_615u64);
    }

    #[test]
    fn pair_keys_round_trip() {
        round_trip(DBBatchId::from(BatchId::from_parts(
            Hash::from([1u8; 32]),
            Hash::from([2u8; 32]),
        )));
        round_trip(DBChunkId::from(ChunkId::from_parts(
            Hash::from([3u8; 32]),
            Hash::from([4u8; 32]),
        )));
    }

    #[test]
    fn a_hash_key_of_the_wrong_length_reports_its_length() {
        let err = Hash::parse("0xdeadbeef").unwrap_err().to_string();
        assert!(err.contains("32 bytes"), "unexpected error: {err}");
        assert!(err.contains("got 4"), "unexpected error: {err}");
    }

    #[test]
    fn a_pair_key_without_a_separator_says_so() {
        let err = DBBatchId::parse(&hex(&[0u8; 32])).unwrap_err().to_string();
        assert!(err.contains("prev_block:last_block"), "unexpected: {err}");
    }

    #[test]
    fn a_hex_key_tolerates_the_0x_prefix() {
        assert_eq!(
            Hash::parse(&format!("0x{}", hex(&[5u8; 32]))).unwrap(),
            Hash::from([5u8; 32])
        );
    }

    #[test]
    fn alloy_hash_and_tx_node_id_keys_round_trip() {
        let text = hex(&[7u8; 32]);
        round_trip(B256::from([7u8; 32]));
        round_trip(TxNodeId(Buf32::from([7u8; 32])));
        assert_eq!(B256::parse(&text).unwrap(), B256::from([7u8; 32]));
        assert_eq!(
            TxNodeId::parse(&text).unwrap(),
            TxNodeId(Buf32::from([7u8; 32]))
        );
        assert!(B256::parse("0102").is_err(), "not 32 bytes");
        assert_eq!(B256::prefix("07").unwrap()[8..], [7]);
    }

    /// The framed prefix is exactly how the table's codec begins a key, so a
    /// prefix scan on a witness table lands on the keys it names.
    #[test]
    fn an_alloy_hash_prefix_starts_the_way_its_codec_encodes_a_key() {
        let mut padded = [0u8; 32];
        padded[0] = 0x06;
        padded[1] = 0x07;
        let encoded =
            <B256 as KeyCodec<BlockStateChangesSchema>>::encode_key(&B256::from(padded)).unwrap();
        let prefix = B256::prefix("0607").unwrap();
        assert!(encoded.starts_with(&prefix), "{encoded:?} vs {prefix:?}");
        assert_eq!(prefix.len(), 8 + 2);
    }

    #[test]
    fn prefixes_are_whole_bytes_of_a_hex_key() {
        assert_eq!(<Vec<u8>>::prefix("0x0001").unwrap(), vec![0, 1]);
        assert_eq!(Buf32::prefix("ab").unwrap(), vec![0xab]);
        assert!(Buf32::prefix("").is_err(), "empty prefix");
        assert!(Buf32::prefix("abc").is_err(), "half a byte");
        assert!(
            Buf32::prefix(&"ff".repeat(33)).is_err(),
            "longer than the key"
        );
        assert!(u64::prefix("12").is_err(), "decimal keys have no prefix");

        assert_eq!(DBBatchId::prefix("3324").unwrap(), vec![0x33, 0x24]);
        assert!(
            DBBatchId::prefix("3324:04").is_err(),
            "a pair prefix is hex only"
        );
    }
}
