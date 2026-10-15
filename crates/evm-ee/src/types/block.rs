//! EVM block implementation.

use alloy_consensus::BlockBody;
use alpen_acct_types::ExecBlock;
use ref_cast::RefCast;
use reth_ethereum_primitives::{Block, TransactionSigned};
use reth_primitives_traits::SealedBlock;
use strata_codec::{Codec, CodecError, Decoder, Encoder};

use super::EvmHeader;
use crate::codec_shims::{decode_rlp_with_length, encode_rlp_with_length};

/// Full EVM block containing header and body.
///
/// Represents a complete Ethereum block with header metadata and transaction body.
/// This is the top-level block type used in the ExecutionEnvironment.
///
/// It holds the block sealed, as Reth's rules and executor take it, so the
/// execution environment can pass it to them without rebuilding it.
#[derive(Clone, Debug)]
pub struct EvmBlock {
    block: SealedBlock<Block>,
}

impl EvmBlock {
    /// Creates a new EvmBlock from a Reth block.
    pub fn new(block: Block) -> Self {
        Self {
            block: SealedBlock::new_unhashed(block),
        }
    }

    /// Gets a reference to the block's body.
    pub fn body(&self) -> &BlockBody<TransactionSigned> {
        self.block.body()
    }

    /// Gets a reference to the sealed block.
    pub fn sealed(&self) -> &SealedBlock<Block> {
        &self.block
    }

    /// Consumes self and returns the sealed block.
    pub fn into_sealed(self) -> SealedBlock<Block> {
        self.block
    }
}

impl ExecBlock for EvmBlock {
    type Header = EvmHeader;

    fn get_header(&self) -> &Self::Header {
        EvmHeader::ref_cast(self.block.sealed_header())
    }
}

impl Codec for EvmBlock {
    fn encode(&self, enc: &mut impl Encoder) -> Result<(), CodecError> {
        self.get_header().encode(enc)?;
        encode_body(self.body(), enc)
    }

    fn decode(dec: &mut impl Decoder) -> Result<Self, CodecError> {
        let header = EvmHeader::decode(dec)?;
        let body = decode_body(dec)?;
        Ok(Self {
            block: SealedBlock::from_sealed_parts(header.into_sealed(), body),
        })
    }
}

fn encode_body(
    body: &BlockBody<TransactionSigned>,
    enc: &mut impl Encoder,
) -> Result<(), CodecError> {
    // Encode transactions count
    let tx_count = body.transactions.len() as u32;
    tx_count.encode(enc)?;

    // Encode each transaction using RLP helper
    for tx in &body.transactions {
        encode_rlp_with_length(tx, enc)?;
    }

    // Encode withdrawals (optional)
    let has_withdrawals = body.withdrawals.is_some();
    has_withdrawals.encode(enc)?;

    if let Some(ref withdrawals) = body.withdrawals {
        let withdrawals_count = withdrawals.len() as u32;
        withdrawals_count.encode(enc)?;

        // Encode each withdrawal using RLP helper
        for withdrawal in withdrawals.iter() {
            encode_rlp_with_length(withdrawal, enc)?;
        }
    }

    Ok(())
}

fn decode_body(dec: &mut impl Decoder) -> Result<BlockBody<TransactionSigned>, CodecError> {
    // Decode transactions count
    let tx_count = u32::decode(dec)? as usize;

    // Decode each transaction using RLP helper
    let mut transactions = Vec::with_capacity(tx_count);
    for _ in 0..tx_count {
        transactions.push(decode_rlp_with_length(dec)?);
    }

    // Decode withdrawals (optional)
    let has_withdrawals = bool::decode(dec)?;
    let withdrawals = if has_withdrawals {
        let withdrawals_count = u32::decode(dec)? as usize;
        let mut withdrawals_vec = Vec::with_capacity(withdrawals_count);

        // Decode each withdrawal using RLP helper
        for _ in 0..withdrawals_count {
            withdrawals_vec.push(decode_rlp_with_length(dec)?);
        }

        Some(withdrawals_vec)
    } else {
        None
    };

    Ok(BlockBody {
        transactions,
        ommers: vec![], // Ommers are deprecated post-merge, always empty
        withdrawals: withdrawals.map(Into::into),
    })
}
