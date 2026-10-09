//! DA codec types and format constants shared between producer and verifier.

use alpen_params::AlpenSpecId;
use alpen_reth_statediff::BatchStateDiff;
use strata_codec::{BufDecoder, Codec, CodecError, Decoder, Encoder};

/// Magic bytes in the EE DA commit transaction marker output.
///
/// TODO(STR-1907): derive this from authenticated EE proof context instead of
/// baking the network value into runtime/proof code.
pub const EE_DA_MAGIC_BYTES: [u8; 4] = *b"ALPN";

/// Returns the DA blob version for a batch governed by `spec_version`.
///
/// The commit transaction carries this version next to the EE DA magic bytes
/// in OP_RETURN, so L1 scanners can tell which layout a decoded blob uses.
/// The version is the spec version itself, so the two version spaces
/// coincide, and V0 blobs keep the `0` they always carried.
pub fn da_blob_version(spec_version: AlpenSpecId) -> u32 {
    u16::from(spec_version).into()
}

/// DA blob containing batch metadata and state diff.
///
/// This is the top-level structure that gets encoded and posted to L1. It
/// wraps the batch state diff with sequencing metadata needed for L1 sync and
/// chain reconstruction.
///
/// The layout depends on the spec version governing the batch, through
/// [`EvmHeaderSummary`]. The version is not part of the encoded bytes. The
/// commit marker carries it instead (see [`da_blob_version`]).
#[derive(Debug, Clone)]
pub struct DaBlob {
    /// Spec version governing the batch.
    pub spec_version: AlpenSpecId,
    /// Monotonic EE account update sequence number for this blob.
    pub update_seq_no: u64,
    /// EVM header context of the last block in this batch.
    pub evm_header: EvmHeaderSummary,
    /// Aggregated state diff for the batch (can be empty for batches with no
    /// state changes).
    pub state_diff: BatchStateDiff,
}

impl DaBlob {
    /// Encodes the blob under the layout its spec version defines.
    pub fn encode(&self, enc: &mut impl Encoder) -> Result<(), CodecError> {
        self.update_seq_no.encode(enc)?;
        self.evm_header.encode(self.spec_version, enc)?;
        self.state_diff.encode(enc)
    }

    /// Encodes the blob into a newly allocated vec.
    pub fn encode_to_vec(&self) -> Result<Vec<u8>, CodecError> {
        let mut buf = Vec::new();
        self.encode(&mut buf)?;
        Ok(buf)
    }

    /// Decodes a blob encoded under the layout `spec_version` defines.
    pub fn decode(spec_version: AlpenSpecId, dec: &mut impl Decoder) -> Result<Self, CodecError> {
        let update_seq_no = u64::decode(dec)?;
        let evm_header = EvmHeaderSummary::decode(spec_version, dec)?;
        let state_diff = BatchStateDiff::decode(dec)?;
        Ok(Self {
            spec_version,
            update_seq_no,
            evm_header,
            state_diff,
        })
    }
}

/// Compact summary of the last EVM block header in a batch.
///
/// A sequencer rebuilding from L1 DA has the [`BatchStateDiff`] for state
/// changes but not the block headers, so these non-derivable fields let it
/// build the next block: `base_fee`/`gas_used`/`gas_limit` drive the EIP-1559
/// base-fee and gas-limit update, `timestamp` enforces monotonicity,
/// `da_rate` bounds the next block's DA rate (from V1), and `block_num` marks
/// where the chain continues.
///
/// The layout depends on the spec version governing the block. V0 carries
/// every field but `da_rate`, and V1 appends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvmHeaderSummary {
    /// Block number of the last EVM block in this batch.
    pub block_num: u64,
    /// Unix timestamp (seconds) of the last EVM block.
    pub timestamp: u64,
    /// Base fee per gas (EIP-1559) of the last EVM block.
    pub base_fee: u64,
    /// Total gas consumed by the last EVM block.
    pub gas_used: u64,
    /// Gas limit of the last EVM block.
    pub gas_limit: u64,
    /// DA rate (wei per byte) committed in the last EVM block's header.
    ///
    /// `None` under V0, whose headers have no rate. Under V1 a zero is a
    /// real rate, so the missing V0 rate is not written as `0`.
    pub da_rate: Option<u64>,
}

impl EvmHeaderSummary {
    /// Encodes the summary under the layout `spec_version` defines.
    ///
    /// Errs if `da_rate` doesn't fit the layout: V0 has no rate field, and V1
    /// requires one.
    pub fn encode(
        &self,
        spec_version: AlpenSpecId,
        enc: &mut impl Encoder,
    ) -> Result<(), CodecError> {
        self.block_num.encode(enc)?;
        self.timestamp.encode(enc)?;
        self.base_fee.encode(enc)?;
        self.gas_used.encode(enc)?;
        self.gas_limit.encode(enc)?;
        match (spec_version, self.da_rate) {
            (AlpenSpecId::V0, None) => Ok(()),
            (AlpenSpecId::V1, Some(da_rate)) => da_rate.encode(enc),
            (AlpenSpecId::V0, Some(_)) | (AlpenSpecId::V1, None) => {
                Err(CodecError::MalformedField("EvmHeaderSummary.da_rate"))
            }
        }
    }

    /// Encodes the summary into a newly allocated vec.
    ///
    /// Errs as [`Self::encode`] does.
    pub fn encode_to_vec(&self, spec_version: AlpenSpecId) -> Result<Vec<u8>, CodecError> {
        let mut buf = Vec::new();
        self.encode(spec_version, &mut buf)?;
        Ok(buf)
    }

    /// Decodes a summary encoded under the layout `spec_version` defines.
    pub fn decode(spec_version: AlpenSpecId, dec: &mut impl Decoder) -> Result<Self, CodecError> {
        let block_num = u64::decode(dec)?;
        let timestamp = u64::decode(dec)?;
        let base_fee = u64::decode(dec)?;
        let gas_used = u64::decode(dec)?;
        let gas_limit = u64::decode(dec)?;
        let da_rate = match spec_version {
            AlpenSpecId::V0 => None,
            AlpenSpecId::V1 => Some(u64::decode(dec)?),
        };
        Ok(Self {
            block_num,
            timestamp,
            base_fee,
            gas_used,
            gas_limit,
            da_rate,
        })
    }

    /// Decodes a summary from `buf`, rejecting any trailing bytes.
    pub fn decode_exact(spec_version: AlpenSpecId, buf: &[u8]) -> Result<Self, CodecError> {
        let mut dec = BufDecoder::new(buf);
        let summary = Self::decode(spec_version, &mut dec)?;
        if dec.remaining() > 0 {
            return Err(CodecError::ExtraInput);
        }
        Ok(summary)
    }
}

/// Decodes a [`DaBlob`] from contiguous payload bytes.
///
/// Trailing bytes after a complete `DaBlob` are rejected.
/// `spec_version` names the layout the blob was encoded under.
pub fn decode_da_blob(payload: &[u8], spec_version: AlpenSpecId) -> Result<DaBlob, CodecError> {
    let mut dec = BufDecoder::new(payload);
    let blob = DaBlob::decode(spec_version, &mut dec)?;
    if dec.remaining() > 0 {
        return Err(CodecError::ExtraInput);
    }
    Ok(blob)
}

/// Decodes a [`DaBlob`] across ordered payload chunks.
///
/// `chunks` must be in commit-output order. The blob is decoded directly across
/// the chunk slices (no intermediate contiguous copy), and any trailing bytes
/// after a complete `DaBlob` are rejected, matching contiguous decoding.
/// `spec_version` names the layout the blob was encoded under.
pub fn decode_da_blob_from_chunks(
    chunks: &[Vec<u8>],
    spec_version: AlpenSpecId,
) -> Result<DaBlob, CodecError> {
    if chunks.is_empty() {
        return Err(CodecError::MalformedField("no DA chunks provided"));
    }

    let mut dec = MultiSliceDecoder::new(chunks);
    let blob = DaBlob::decode(spec_version, &mut dec)?;
    if dec.remaining() > 0 {
        return Err(CodecError::ExtraInput);
    }
    Ok(blob)
}

/// A [`Decoder`] that reads across a sequence of byte chunks without first
/// concatenating them into one contiguous buffer.
///
/// Lets [`decode_da_blob_from_chunks`] decode a [`DaBlob`] straight from its
/// commit/reveal chunk payloads, avoiding an O(blob) allocation + copy on the
/// proof-verification path.
struct MultiSliceDecoder<'a> {
    chunks: &'a [Vec<u8>],
    /// Index of the chunk currently being read.
    chunk: usize,
    /// Read offset within `chunks[chunk]`.
    offset: usize,
}

impl<'a> MultiSliceDecoder<'a> {
    fn new(chunks: &'a [Vec<u8>]) -> Self {
        Self {
            chunks,
            chunk: 0,
            offset: 0,
        }
    }

    /// Total number of unread bytes across the current and later chunks.
    fn remaining(&self) -> usize {
        if self.chunk >= self.chunks.len() {
            return 0;
        }
        let current = self.chunks[self.chunk].len().saturating_sub(self.offset);
        let later: usize = self.chunks[self.chunk + 1..].iter().map(Vec::len).sum();
        current + later
    }
}

impl Decoder for MultiSliceDecoder<'_> {
    fn read_buf(&mut self, into: &mut [u8]) -> Result<(), CodecError> {
        if into.len() > self.remaining() {
            return Err(CodecError::OverrunInput);
        }

        let mut filled = 0;
        while filled < into.len() {
            let chunk = &self.chunks[self.chunk];
            if self.offset >= chunk.len() {
                // Current chunk exhausted; `remaining()` guarantees a later one holds the rest.
                self.chunk += 1;
                self.offset = 0;
                continue;
            }
            let available = &chunk[self.offset..];
            let take = available.len().min(into.len() - filled);
            into[filled..filled + take].copy_from_slice(&available[..take]);
            self.offset += take;
            filled += take;
        }

        Ok(())
    }

    fn read_arr<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let mut buf = [0u8; N];
        self.read_buf(&mut buf)?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use alpen_params::HeaderExtra;

    use super::*;

    const VERSIONS: [AlpenSpecId; 2] = [AlpenSpecId::V0, AlpenSpecId::V1];

    /// A summary of a header governed by `spec_version`, so it carries the
    /// rate only where that version's header has one.
    fn sample_summary(spec_version: AlpenSpecId) -> EvmHeaderSummary {
        EvmHeaderSummary {
            block_num: 10,
            timestamp: 1_700_000_000,
            base_fee: 100,
            gas_used: 21_000,
            gas_limit: 36_000_000,
            da_rate: HeaderExtra::new(spec_version, 2_500_000_000).da_rate(),
        }
    }

    fn sample_blob(spec_version: AlpenSpecId) -> DaBlob {
        DaBlob {
            spec_version,
            update_seq_no: 7,
            evm_header: sample_summary(spec_version),
            state_diff: BatchStateDiff::new(),
        }
    }

    #[test]
    fn v0_summary_layout_omits_da_rate() {
        let summary = sample_summary(AlpenSpecId::V0);
        let encoded = summary.encode_to_vec(AlpenSpecId::V0).unwrap();

        let expected: Vec<u8> = [10u64, 1_700_000_000, 100, 21_000, 36_000_000]
            .iter()
            .flat_map(|field| field.to_be_bytes())
            .collect();
        assert_eq!(encoded, expected);

        let decoded = EvmHeaderSummary::decode_exact(AlpenSpecId::V0, &encoded).unwrap();
        assert_eq!(decoded.da_rate, None);
        assert_eq!(decoded, summary);
    }

    #[test]
    fn v1_summary_layout_appends_da_rate() {
        let summary = sample_summary(AlpenSpecId::V1);
        let v0 = sample_summary(AlpenSpecId::V0)
            .encode_to_vec(AlpenSpecId::V0)
            .unwrap();
        let v1 = summary.encode_to_vec(AlpenSpecId::V1).unwrap();

        assert_eq!(v1[..v0.len()], v0[..]);
        assert_eq!(v1[v0.len()..], 2_500_000_000u64.to_be_bytes());
        assert_eq!(
            EvmHeaderSummary::decode_exact(AlpenSpecId::V1, &v1).unwrap(),
            summary
        );
    }

    /// A rate that doesn't fit the layout is refused rather than dropped or
    /// made up.
    #[test]
    fn summary_rate_must_fit_the_layout() {
        for (summary_version, layout) in [
            (AlpenSpecId::V1, AlpenSpecId::V0),
            (AlpenSpecId::V0, AlpenSpecId::V1),
        ] {
            assert!(
                matches!(
                    sample_summary(summary_version).encode_to_vec(layout),
                    Err(CodecError::MalformedField(_))
                ),
                "{summary_version:?} summary under the {layout:?} layout"
            );
        }
    }

    #[test]
    fn summary_does_not_decode_under_another_layout() {
        let v0 = sample_summary(AlpenSpecId::V0)
            .encode_to_vec(AlpenSpecId::V0)
            .unwrap();
        let v1 = sample_summary(AlpenSpecId::V1)
            .encode_to_vec(AlpenSpecId::V1)
            .unwrap();

        assert!(matches!(
            EvmHeaderSummary::decode_exact(AlpenSpecId::V0, &v1),
            Err(CodecError::ExtraInput)
        ));
        assert!(EvmHeaderSummary::decode_exact(AlpenSpecId::V1, &v0).is_err());
    }

    #[test]
    fn da_blob_version_is_the_spec_version() {
        assert_eq!(da_blob_version(AlpenSpecId::V0), 0);
        assert_eq!(da_blob_version(AlpenSpecId::V1), 1);
    }

    #[test]
    fn decodes_across_arbitrary_chunk_boundaries() {
        for spec_version in VERSIONS {
            let encoded = sample_blob(spec_version).encode_to_vec().unwrap();

            // Splitting the same bytes at every boundary must decode identically to
            // the single-buffer path (compared via re-encoding, as DaBlob is not Eq).
            for chunk_size in 1..=encoded.len() {
                let chunks: Vec<Vec<u8>> = encoded.chunks(chunk_size).map(|c| c.to_vec()).collect();
                let got = decode_da_blob_from_chunks(&chunks, spec_version)
                    .expect("decode across chunks");
                assert_eq!(
                    got.encode_to_vec().unwrap(),
                    encoded,
                    "{spec_version:?} chunk_size={chunk_size}"
                );
            }
        }
    }

    #[test]
    fn empty_chunks_is_error() {
        assert!(decode_da_blob_from_chunks(&[], AlpenSpecId::V1).is_err());
    }

    #[test]
    fn decodes_contiguous_payload() {
        for spec_version in VERSIONS {
            let encoded = sample_blob(spec_version).encode_to_vec().unwrap();
            let got = decode_da_blob(&encoded, spec_version).expect("decode contiguous payload");
            assert_eq!(got.encode_to_vec().unwrap(), encoded);
        }
    }

    #[test]
    fn contiguous_payload_does_not_decode_under_another_layout() {
        let v0 = sample_blob(AlpenSpecId::V0).encode_to_vec().unwrap();
        let v1 = sample_blob(AlpenSpecId::V1).encode_to_vec().unwrap();

        assert!(decode_da_blob(&v0, AlpenSpecId::V1).is_err());
        assert!(decode_da_blob(&v1, AlpenSpecId::V0).is_err());
    }

    #[test]
    fn contiguous_trailing_bytes_are_rejected() {
        for spec_version in VERSIONS {
            let mut encoded = sample_blob(spec_version).encode_to_vec().unwrap();
            encoded.push(0xFF);
            assert!(matches!(
                decode_da_blob(&encoded, spec_version),
                Err(CodecError::ExtraInput)
            ));
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        for spec_version in VERSIONS {
            let mut encoded = sample_blob(spec_version).encode_to_vec().unwrap();
            encoded.push(0xFF);
            assert!(matches!(
                decode_da_blob_from_chunks(&[encoded], spec_version),
                Err(CodecError::ExtraInput)
            ));
        }
    }
}
