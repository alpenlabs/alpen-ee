//! EVM block header implementation.

use alloy_consensus::Header;
use alpen_acct_types::ExecHeader;
use alpen_chain_types::ExecHeaderSummary;
use alpen_da_types::EvmHeaderSummary;
use alpen_params::HeaderExtra;
use strata_codec::{Codec, CodecError};

use super::Hash;
use crate::codec_shims::{decode_rlp_with_length, encode_rlp_with_length};

/// Block header for EVM execution.
///
/// Wraps Alloy's consensus Header type and implements the ExecHeader trait
/// to provide block metadata for the execution environment.
#[derive(Clone, Debug)]
pub struct EvmHeader {
    header: Header,
}

impl EvmHeader {
    /// Creates a new EvmHeader from an Alloy Header.
    pub fn new(header: Header) -> Self {
        Self { header }
    }

    /// Gets a reference to the underlying Header.
    pub fn header(&self) -> &Header {
        &self.header
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
        // stamp decodes. The guest summarizes only the chunk tip, and only
        // after `verify_header` has decoded and checked its stamp.
        let header_extra = HeaderExtra::of_header(&self.header)
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
        self.header.hash_slow().0.into()
    }
}

impl Codec for EvmHeader {
    fn encode(&self, enc: &mut impl strata_codec::Encoder) -> Result<(), CodecError> {
        // Use Alloy's RLP encoding (standard Ethereum format) with length prefix
        encode_rlp_with_length(&self.header, enc)
    }

    fn decode(dec: &mut impl strata_codec::Decoder) -> Result<Self, CodecError> {
        // Decode using Alloy's RLP decoder with length prefix
        let header = decode_rlp_with_length(dec)?;
        Ok(Self { header })
    }
}
