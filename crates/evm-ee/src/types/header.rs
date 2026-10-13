//! EVM block header implementation.

use alloy_consensus::Header;
use alpen_acct_types::ExecHeader;
use alpen_chain_types::ExecHeaderSummary;
use alpen_da_types::EvmHeaderSummary;
use alpen_params::HeaderExtra;
use ref_cast::RefCast;
use reth_primitives_traits::SealedHeader;
use strata_codec::{Codec, CodecError};

use super::Hash;
use crate::codec_shims::{decode_rlp_with_length, encode_rlp_with_length};

/// Block header for EVM execution.
///
/// Wraps a sealed Alloy consensus header and implements the ExecHeader trait
/// to provide block metadata for the execution environment.
///
/// The seal computes the block hash on first use and caches it, so the
/// header is hashed once however often its ID is read.
///
/// It is transparent over the sealed header so an [`EvmBlock`](super::EvmBlock)
/// can lend out its header as an `EvmHeader` without copying it.
#[derive(Clone, Debug, RefCast)]
#[repr(transparent)]
pub struct EvmHeader {
    header: SealedHeader<Header>,
}

impl EvmHeader {
    /// Creates a new EvmHeader from an Alloy Header.
    pub fn new(header: Header) -> Self {
        Self {
            header: SealedHeader::new_unhashed(header),
        }
    }

    /// Gets a reference to the underlying Header.
    pub fn header(&self) -> &Header {
        self.header.header()
    }

    /// Gets a reference to the sealed header, as Reth's rules take it.
    pub fn sealed(&self) -> &SealedHeader<Header> {
        &self.header
    }

    /// Consumes self and returns the sealed header.
    pub fn into_sealed(self) -> SealedHeader<Header> {
        self.header
    }

    /// Returns the block number.
    pub fn block_number(&self) -> u64 {
        self.header.number
    }
}

impl ExecHeader for EvmHeader {
    fn get_parent_id(&self) -> Hash {
        self.header.parent_hash.0.into()
    }

    fn get_state_root(&self) -> Hash {
        self.header.state_root.0.into()
    }

    fn get_exec_header_summary(&self) -> ExecHeaderSummary {
        // Hosts only summarize headers they have already validated, so the
        // stamp decodes. The guest summarizes the chunk tip before it runs
        // the blocks, but there a panic rejects the chunk just like an error.
        let header_extra = HeaderExtra::of_header(self.header())
            .expect("Alpen EVM headers carry a valid extra_data stamp");
        let payload = EvmHeaderSummary {
            block_num: self.header.number,
            timestamp: self.header.timestamp,
            base_fee: self
                .header
                .base_fee_per_gas
                .expect("Alpen EVM headers must include base_fee_per_gas from genesis"),
            gas_used: self.header.gas_used,
            gas_limit: self.header.gas_limit,
            da_rate: header_extra.da_rate(),
        };
        let encoded = payload
            .encode_to_vec(header_extra.spec_version())
            .expect("encode EVM header summary");
        ExecHeaderSummary::from_vec(encoded).expect("exec header summary fits the SSZ bound")
    }

    fn compute_block_id(&self) -> Hash {
        self.header.hash().0.into()
    }
}

impl Codec for EvmHeader {
    fn encode(&self, enc: &mut impl strata_codec::Encoder) -> Result<(), CodecError> {
        // Use Alloy's RLP encoding (standard Ethereum format) with length prefix
        encode_rlp_with_length(self.header(), enc)
    }

    fn decode(dec: &mut impl strata_codec::Decoder) -> Result<Self, CodecError> {
        // Decode using Alloy's RLP decoder with length prefix
        let header = decode_rlp_with_length(dec)?;
        Ok(Self::new(header))
    }
}
