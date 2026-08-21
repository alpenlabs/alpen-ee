use alloy_rpc_types::engine::{
    payload::ExecutionData, ExecutionPayload, ExecutionPayloadEnvelopeV3,
    ExecutionPayloadEnvelopeV5, ExecutionPayloadV1,
};
use alpen_ee_params::{AlpenSpecId, EvmSpec, HeaderExtraError};
use reth_chainspec::ChainSpec;
use reth_ethereum_payload_builder::EthereumExecutionPayloadValidator;
use reth_node_api::{
    payload::PayloadTypes, validate_execution_requests, validate_version_specific_fields,
    AddOnsContext, BuiltPayload, EngineApiMessageVersion, EngineApiValidator,
    EngineObjectValidationError, EngineTypes, FullNodeComponents, NewPayloadError, NodeTypes,
    PayloadOrAttributes, PayloadValidator,
};
use reth_node_builder::rpc::PayloadValidatorBuilder;
use reth_primitives::{Block, EthPrimitives, NodePrimitives, RecoveredBlock, SealedBlock};
use serde::{Deserialize, Serialize};

use crate::{
    evm_config::{payload_spec_version, version_indexed, AlpenEvmConfig},
    payload::{AlpenBuiltPayload, AlpenExecutionPayloadEnvelopeV4, AlpenPayloadBuilderAttributes},
    AlpenExecutionPayloadEnvelopeV2, AlpenPayloadAttributes,
};

/// Custom engine types for strata to use custom payload attributes and payload
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[non_exhaustive]
pub struct AlpenEngineTypes {}

impl PayloadTypes for AlpenEngineTypes {
    type BuiltPayload = AlpenBuiltPayload;
    type ExecutionData = ExecutionData;
    type PayloadAttributes = AlpenPayloadAttributes;
    type PayloadBuilderAttributes = AlpenPayloadBuilderAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        let (payload, sidecar) =
            ExecutionPayload::from_block_unchecked(block.hash(), &block.into_block());
        ExecutionData { payload, sidecar }
    }
}

impl EngineTypes for AlpenEngineTypes {
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    type ExecutionPayloadEnvelopeV2 = AlpenExecutionPayloadEnvelopeV2;
    type ExecutionPayloadEnvelopeV3 = ExecutionPayloadEnvelopeV3;
    type ExecutionPayloadEnvelopeV4 = AlpenExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV5;
}

/// Strata engine validator, dispatching by the spec version each payload
/// claims: payloads by the version stamped in their `extra_data`, attributes
/// by the version the Alpen layer resolved onto them.
#[derive(Debug, Clone)]
pub struct AlpenEngineValidator {
    /// Payload validator of each known [`AlpenSpecId`], indexed by
    /// discriminant.
    inners: Vec<EthereumExecutionPayloadValidator<ChainSpec>>,
}

impl AlpenEngineValidator {
    /// Instantiates a new validator over `evm_spec`'s per-version chain spec
    /// table.
    pub fn new(evm_spec: &EvmSpec) -> Self {
        Self {
            inners: evm_spec
                .chain_specs()
                .iter()
                .cloned()
                .map(EthereumExecutionPayloadValidator::new)
                .collect(),
        }
    }

    /// Returns the chain spec governing `spec_version`.
    #[inline]
    fn chain_spec_for(&self, spec_version: AlpenSpecId) -> &ChainSpec {
        version_indexed(&self.inners, spec_version).chain_spec()
    }

    /// Resolves the spec version governing `attributes`, refusing a version
    /// this binary has no variant for.
    fn attributes_spec_version(
        attributes: &AlpenPayloadAttributes,
    ) -> Result<AlpenSpecId, HeaderExtraError> {
        AlpenSpecId::try_from(attributes.spec_version).map_err(HeaderExtraError::UnknownVersion)
    }
}

impl PayloadValidator<AlpenEngineTypes> for AlpenEngineValidator {
    type Block = Block;

    fn ensure_well_formed_payload(
        &self,
        payload: ExecutionData,
    ) -> Result<RecoveredBlock<Self::Block>, NewPayloadError> {
        let spec_version = payload_spec_version(&payload).map_err(NewPayloadError::other)?;
        let inner = version_indexed(&self.inners, spec_version);
        let sealed_block = inner.ensure_well_formed_payload(payload)?;
        sealed_block
            .try_recover()
            .map_err(|e| NewPayloadError::Other(e.into()))
    }
}

impl EngineApiValidator<AlpenEngineTypes> for AlpenEngineValidator {
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<'_, ExecutionData, AlpenPayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        payload_or_attrs
            .execution_requests()
            .map(|requests| validate_execution_requests(requests))
            .transpose()?;

        let spec_version = match &payload_or_attrs {
            PayloadOrAttributes::ExecutionPayload(payload) => payload_spec_version(payload),
            PayloadOrAttributes::PayloadAttributes(attributes) => {
                Self::attributes_spec_version(attributes)
            }
        }
        .map_err(|err| EngineObjectValidationError::InvalidParams(err.into()))?;
        validate_version_specific_fields(
            self.chain_spec_for(spec_version),
            version,
            payload_or_attrs,
        )
    }

    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &AlpenPayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        let spec_version = Self::attributes_spec_version(attributes)
            .map_err(|err| EngineObjectValidationError::InvalidParams(err.into()))?;
        validate_version_specific_fields(
            self.chain_spec_for(spec_version),
            version,
            PayloadOrAttributes::<ExecutionData, AlpenPayloadAttributes>::PayloadAttributes(
                attributes,
            ),
        )?;

        Ok(())
    }
}

/// Custom engine validator builder
///
/// Deliberately stateless: reth's `BasicEngineApiBuilder` and
/// `BasicEngineValidatorBuilder` each hold their own default-constructed
/// copy of this builder, so any configuration carried on an instance would
/// silently miss the engine paths. Deriving the table from the node's EVM
/// component instead makes every copy equivalent — validation and execution
/// share one per-version source.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct AlpenEngineValidatorBuilder;

impl<N> PayloadValidatorBuilder<N> for AlpenEngineValidatorBuilder
where
    N: FullNodeComponents<
        Types: NodeTypes<
            Payload = AlpenEngineTypes,
            ChainSpec = ChainSpec,
            Primitives = EthPrimitives,
        >,
        Evm = AlpenEvmConfig,
    >,
{
    type Validator = AlpenEngineValidator;

    async fn build(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        Ok(AlpenEngineValidator::new(ctx.node.evm_config().evm_spec()))
    }
}
