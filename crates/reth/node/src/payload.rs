use alloy_eips::eip7685::Requests;
use alloy_rpc_types::{
    engine::{
        ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4, ExecutionPayloadEnvelopeV5,
        ExecutionPayloadEnvelopeV6, ExecutionPayloadV1, ExecutionPayloadV2,
        PayloadAttributes as EthPayloadAttributes, PayloadId,
    },
    Withdrawal,
};
use alpen_ee_params::{AlpenSpecId, HeaderExtraError};
use alpen_reth_primitives::WithdrawalIntent;
use reth_ethereum_engine_primitives::BuiltPayloadConversionError;
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_node_api::{BuiltPayload, PayloadAttributes};
use reth_payload_builder::EthBuiltPayload;
use reth_primitives_traits::SealedBlock;
use revm_primitives::alloy_primitives::{B256, U256};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AlpenPayloadAttributes {
    /// Original Ethereum payload attributes
    #[serde(flatten)]
    pub inner: EthPayloadAttributes,
    /// Alpen spec version governing the block, as the raw discriminant.
    /// Version resolution happens at the Alpen layer; this wire type only
    /// carries the choice (the enum's serde form is a variant name, wrong
    /// for engine-API JSON). Typed — and an unknown discriminant refused —
    /// when the payload is validated or built (see
    /// [`Self::alpen_spec_version`]). Defaults to 0 (the genesis version)
    /// for attribute sources that predate versioning.
    #[serde(default)]
    pub spec_version: u16,
}

impl AlpenPayloadAttributes {
    /// Wraps Ethereum payload attributes, stamping the Alpen spec version that
    /// governs the block.
    pub fn new_from_eth(
        payload_attributes: EthPayloadAttributes,
        spec_version: AlpenSpecId,
    ) -> Self {
        Self {
            inner: payload_attributes,
            spec_version: u16::from(spec_version),
        }
    }

    /// Returns the typed Alpen spec version governing the block.
    ///
    /// Errs when the attributes name a spec version this binary has no
    /// variant for: they were resolved by newer code, and failing beats
    /// building under rules older than the ones asked for.
    pub fn alpen_spec_version(&self) -> Result<AlpenSpecId, HeaderExtraError> {
        AlpenSpecId::try_from(self.spec_version).map_err(HeaderExtraError::UnknownVersion)
    }
}

impl PayloadAttributes for AlpenPayloadAttributes {
    /// Derived from the wrapped Ethereum attributes only; the spec version
    /// does not contribute to the id.
    fn payload_id(&self, parent_hash: &B256) -> PayloadId {
        self.inner.payload_id(parent_hash)
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.inner.withdrawals()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root()
    }

    fn slot_number(&self) -> Option<u64> {
        self.inner.slot_number()
    }
}

#[derive(Debug, Clone)]
pub struct AlpenBuiltPayload {
    /// Payload to build ethereum block.
    pub(crate) inner: EthBuiltPayload,
    /// Identifier of the payload job that built this payload, kept for
    /// persistence alongside the [`EthBuiltPayload`] (which carries no id).
    pub(crate) payload_id: PayloadId,
    // additional fields for strata
    /// Requested withdrawals
    pub(crate) withdrawal_intents: Vec<WithdrawalIntent>,
    /// Encoded depth-0 per-block proof witness, captured inline during payload
    /// build (see `try_build_payload`). Carried in-memory back to the sequencer
    /// for persistence; `None` for payloads reconstructed from the wire (e.g.
    /// `newPayload`), which never need it.
    pub(crate) block_witness: Option<Vec<u8>>,
}

impl AlpenBuiltPayload {
    pub fn new(inner: EthBuiltPayload, withdrawal_intents: Vec<WithdrawalIntent>) -> Self {
        Self {
            inner,
            payload_id: PayloadId::default(),
            withdrawal_intents,
            block_witness: None,
        }
    }

    /// Attaches the identifier of the payload job that built this payload.
    pub fn with_payload_id(mut self, payload_id: PayloadId) -> Self {
        self.payload_id = payload_id;
        self
    }

    /// Returns the identifier of the payload job that built this payload.
    pub fn payload_id(&self) -> PayloadId {
        self.payload_id
    }

    /// Attaches the encoded per-block proof witness captured during build.
    pub fn with_block_witness(mut self, block_witness: Vec<u8>) -> Self {
        self.block_witness = Some(block_witness);
        self
    }

    pub fn withdrawal_intents(&self) -> &[WithdrawalIntent] {
        &self.withdrawal_intents
    }

    /// Takes the encoded per-block proof witness captured during build, if any.
    pub fn take_block_witness(&mut self) -> Option<Vec<u8>> {
        self.block_witness.take()
    }

    pub fn into_parts(self) -> (EthBuiltPayload, Vec<WithdrawalIntent>) {
        (self.inner, self.withdrawal_intents)
    }
}

impl BuiltPayload for AlpenBuiltPayload {
    type Primitives = EthPrimitives;

    fn block(&self) -> &SealedBlock<Block> {
        self.inner.block()
    }

    fn fees(&self) -> U256 {
        self.inner.fees()
    }

    fn requests(&self) -> Option<Requests> {
        self.inner.requests()
    }
}

impl From<AlpenBuiltPayload> for ExecutionPayloadV1 {
    fn from(value: AlpenBuiltPayload) -> Self {
        value.inner.into()
    }
}

/// Custom Execution payload v2

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPayloadEnvelopeV2 {
    /// Execution payload, which could be either V1 or V2
    ///
    /// V1 (_NO_ withdrawals) MUST be returned if the payload timestamp is lower than the Shanghai
    /// timestamp
    ///
    /// V2 (_WITH_ withdrawals) MUST be returned if the payload timestamp is greater or equal to
    /// the Shanghai timestamp
    pub execution_payload: ExecutionPayloadFieldV2,
    /// The expected value to be received by the feeRecipient in wei
    pub block_value: U256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExecutionPayloadFieldV2 {
    /// V2 payload
    V2(ExecutionPayloadV2),
    /// V1 payload
    V1(ExecutionPayloadV1),
}

impl ExecutionPayloadFieldV2 {
    /// Returns the inner [ExecutionPayloadV1]
    pub fn into_v1_payload(self) -> ExecutionPayloadV1 {
        match self {
            Self::V2(payload) => payload.payload_inner,
            Self::V1(payload) => payload,
        }
    }
}

impl From<EthBuiltPayload> for ExecutionPayloadEnvelopeV2 {
    fn from(value: EthBuiltPayload) -> Self {
        let block = value.block().clone();
        let fees = value.fees();

        Self {
            block_value: fees,
            execution_payload: ExecutionPayloadFieldV2::V2(
                ExecutionPayloadV2::from_block_unchecked(block.hash(), &block.into_block()),
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlpenExecutionPayloadEnvelopeV4 {
    #[serde(flatten)]
    pub inner: ExecutionPayloadEnvelopeV4,
    pub withdrawal_intents: Vec<WithdrawalIntent>,
}

impl AlpenExecutionPayloadEnvelopeV4 {
    pub fn inner(&self) -> &ExecutionPayloadEnvelopeV4 {
        &self.inner
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlpenExecutionPayloadEnvelopeV2 {
    #[serde(flatten)]
    pub inner: ExecutionPayloadEnvelopeV2,
    pub withdrawal_intents: Vec<WithdrawalIntent>,
}

impl AlpenExecutionPayloadEnvelopeV2 {
    pub fn inner(&self) -> &ExecutionPayloadEnvelopeV2 {
        &self.inner
    }
}

impl From<AlpenBuiltPayload> for AlpenExecutionPayloadEnvelopeV2 {
    fn from(value: AlpenBuiltPayload) -> Self {
        Self {
            inner: value.inner.into(),
            withdrawal_intents: value.withdrawal_intents,
        }
    }
}

impl TryFrom<AlpenBuiltPayload> for ExecutionPayloadEnvelopeV3 {
    type Error = BuiltPayloadConversionError;

    fn try_from(value: AlpenBuiltPayload) -> Result<Self, Self::Error> {
        value.inner.try_into_v3()
    }
}

impl TryFrom<AlpenBuiltPayload> for ExecutionPayloadEnvelopeV4 {
    type Error = BuiltPayloadConversionError;

    fn try_from(value: AlpenBuiltPayload) -> Result<Self, Self::Error> {
        value.inner.try_into_v4()
    }
}

impl TryFrom<AlpenBuiltPayload> for AlpenExecutionPayloadEnvelopeV4 {
    type Error = BuiltPayloadConversionError;

    fn try_from(value: AlpenBuiltPayload) -> Result<Self, Self::Error> {
        Ok(Self {
            inner: value.inner.try_into_v4()?,
            withdrawal_intents: value.withdrawal_intents,
        })
    }
}

impl TryFrom<AlpenBuiltPayload> for ExecutionPayloadEnvelopeV5 {
    type Error = BuiltPayloadConversionError;

    fn try_from(value: AlpenBuiltPayload) -> Result<Self, Self::Error> {
        value.inner.try_into_v5()
    }
}

impl TryFrom<AlpenBuiltPayload> for ExecutionPayloadEnvelopeV6 {
    type Error = BuiltPayloadConversionError;

    fn try_from(value: AlpenBuiltPayload) -> Result<Self, Self::Error> {
        value.inner.try_into_v6()
    }
}
