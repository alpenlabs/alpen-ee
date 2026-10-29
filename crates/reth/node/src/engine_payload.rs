use alloy_eips::eip7685::Requests;
use alloy_primitives::{B256, U256};
use alloy_rlp::Decodable;
use alloy_rpc_types::engine::PayloadId;
use alpen_engine_api::EnginePayload;
use alpen_primitives::WithdrawalIntent;
use bincode::{deserialize, serialize};
use reth_ethereum_engine_primitives::{BlobSidecars, EthBuiltPayload};
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_node_builder::{BuiltPayload, NodePrimitives};
use reth_primitives_traits::{Block as BlockTrait, SealedBlock};
use serde::{Deserialize, Serialize};
use strata_acct_types::Hash;
use thiserror::Error;
use tracing::error;

use crate::AlpenBuiltPayload;

/// Errors that can occur when working with Alpen engine payloads.
#[derive(Debug, Error)]
pub enum AlpenEnginePayloadError {
    #[error("expected blob sidecars to be empty; blockhash: {0}")]
    BlobSidecarsNotEmpty(B256),
    #[error(transparent)]
    Serialization(#[from] bincode::Error),
    #[error("RLP decoding failed: {0}")]
    RlpDecode(#[from] alloy_rlp::Error),
}

/// The node stores each block's payload in this encoding and replays the stored
/// payloads into reth on restart, so the encoding must not change. The
/// `engine_payload.bin` fixture pins it.
impl EnginePayload for AlpenBuiltPayload {
    type Error = AlpenEnginePayloadError;

    fn blocknum(&self) -> u64 {
        self.block().number
    }

    fn blockhash(&self) -> Hash {
        self.block().hash().0.into()
    }

    fn state_root(&self) -> Hash {
        self.block().header().state_root.0.into()
    }

    fn withdrawal_intents(&self) -> &[WithdrawalIntent] {
        self.withdrawal_intents()
    }

    fn to_bytes(&self) -> Result<Vec<u8>, Self::Error> {
        let serializable = SerializablePayload::try_from(self.clone())?;
        Ok(serialize(&serializable)?)
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Error> {
        let serializable = deserialize::<SerializablePayload>(bytes)?;
        serializable.try_into()
    }
}

/// Internal representation of a payload for bincode serialization.
///
/// The block is stored as RLP-encoded bytes because `SealedBlock`'s serde
/// implementation uses iterators that bincode cannot serialize when the
/// block contains transactions.
#[derive(Debug, Serialize, Deserialize)]
struct SerializablePayload {
    payload_id: PayloadId,
    /// RLP-encoded block bytes.
    block_rlp: Vec<u8>,
    /// Block hash (used to re-seal the block on deserialization).
    block_hash: B256,
    fees: U256,
    requests: Option<Requests>,
    withdrawal_intents: Vec<WithdrawalIntent>,
}

impl TryFrom<AlpenBuiltPayload> for SerializablePayload {
    type Error = AlpenEnginePayloadError;

    fn try_from(value: AlpenBuiltPayload) -> Result<Self, Self::Error> {
        let payload_id = value.payload_id();
        let (eth_built_payload, withdrawal_intents) = value.into_parts();

        if !matches!(eth_built_payload.sidecars(), BlobSidecars::Empty) {
            let blockhash = eth_built_payload.block().hash();
            error!(%blockhash, "expected payload sidecars to be empty");
            return Err(AlpenEnginePayloadError::BlobSidecarsNotEmpty(blockhash));
        }

        // RLP encode the block for bincode serialization
        let sealed_block = eth_built_payload.block();
        let block_hash = sealed_block.hash();
        let block_rlp = alloy_rlp::encode(sealed_block.clone_block());

        Ok(SerializablePayload {
            payload_id,
            block_rlp,
            block_hash,
            fees: eth_built_payload.fees(),
            requests: eth_built_payload.requests().clone(),
            withdrawal_intents,
        })
    }
}

impl TryFrom<SerializablePayload> for AlpenBuiltPayload {
    type Error = AlpenEnginePayloadError;

    fn try_from(value: SerializablePayload) -> Result<Self, Self::Error> {
        let SerializablePayload {
            payload_id,
            block_rlp,
            block_hash,
            fees,
            requests,
            withdrawal_intents,
        } = value;

        // Decode the RLP-encoded block and seal it with the stored hash
        let block = Block::decode(&mut block_rlp.as_slice())?;
        let sealed_block: SealedBlock<<EthPrimitives as NodePrimitives>::Block> =
            block.seal_unchecked(block_hash);

        let eth_built_payload = EthBuiltPayload::new(sealed_block.into(), fees, requests, None);

        Ok(
            AlpenBuiltPayload::new(eth_built_payload, withdrawal_intents)
                .with_payload_id(payload_id),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
    use alloy_eips::eip4895::{Withdrawal, Withdrawals};
    use alloy_primitives::{Address, Bytes, Signature, TxKind};
    use reth_ethereum_primitives::TransactionSigned;
    use strata_ol_bridge_types::OperatorSelection;
    use strata_primitives::bitcoin_bosd::Descriptor;

    use super::*;

    /// Bytes the codec wrote for [`fixture_payload`] before it moved out of
    /// `alpen-common`. Nodes have stored payloads in this form.
    const ENGINE_PAYLOAD_FIXTURE: &[u8] = include_bytes!("../testdata/engine_payload.bin");

    /// A payload that sets every stored field: one transaction, one deposit,
    /// one request, one withdrawal intent and a payload id.
    fn fixture_payload() -> AlpenBuiltPayload {
        let tx = TxLegacy {
            chain_id: Some(2892),
            nonce: 1,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::repeat_byte(0x33)),
            value: U256::from(5),
            input: Bytes::from_static(&[0xde, 0xad]),
        };
        let tx: TransactionSigned = tx
            .into_signed(Signature::new(U256::from(1), U256::from(2), false))
            .into();

        let header = Header {
            parent_hash: B256::repeat_byte(0x11),
            number: 7,
            state_root: B256::repeat_byte(0x22),
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            withdrawals_root: Some(B256::repeat_byte(0x44)),
            ..Default::default()
        };
        let body = BlockBody {
            transactions: vec![tx],
            ommers: vec![],
            withdrawals: Some(Withdrawals::new(vec![Withdrawal {
                index: 3,
                validator_index: 0,
                address: Address::repeat_byte(0x55),
                amount: 1_000,
            }])),
        };
        let block = Block::new(header, body).seal_slow();
        let requests = Requests::new(vec![Bytes::from_static(&[0x01, 0xaa])]);
        let eth_payload =
            EthBuiltPayload::new(Arc::new(block), U256::from(21_000), Some(requests), None);

        let mut destination = vec![0x03];
        destination.extend_from_slice(&[0x22; 20]);
        let intent = WithdrawalIntent {
            amt: 100_000_000,
            selected_operator: OperatorSelection::any(),
            destination: Descriptor::from_bytes(&destination).expect("valid p2wpkh descriptor"),
        };

        AlpenBuiltPayload::new(eth_payload, vec![intent]).with_payload_id(PayloadId::new([0x66; 8]))
    }

    #[test]
    fn test_engine_payload_encoding_matches_fixture() {
        let encoded = fixture_payload().to_bytes().unwrap();

        assert_eq!(encoded.as_slice(), ENGINE_PAYLOAD_FIXTURE);
    }

    #[test]
    fn test_engine_payload_decodes_fixture() {
        let expected = fixture_payload();

        let decoded = AlpenBuiltPayload::from_bytes(ENGINE_PAYLOAD_FIXTURE).unwrap();

        assert_eq!(decoded.blocknum(), expected.blocknum());
        assert_eq!(decoded.blockhash(), expected.blockhash());
        assert_eq!(decoded.state_root(), expected.state_root());
        assert_eq!(decoded.withdrawal_intents(), expected.withdrawal_intents());
        assert_eq!(decoded.payload_id(), expected.payload_id());
        assert_eq!(decoded.block(), expected.block());
        assert_eq!(decoded.fees(), expected.fees());
        assert_eq!(decoded.requests(), expected.requests());
    }
}
