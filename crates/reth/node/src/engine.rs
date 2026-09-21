use alloy_rpc_types::engine::{
    payload::ExecutionData, ExecutionPayload, ExecutionPayloadEnvelopeV3,
    ExecutionPayloadEnvelopeV5, ExecutionPayloadEnvelopeV6, ExecutionPayloadV1,
};
use alpen_ee_params::{AlpenSpecId, EvmSpec, HeaderExtraError};
use reth_chainspec::ChainSpec;
use reth_ethereum_payload_builder::EthereumExecutionPayloadValidator;
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_node_api::{
    payload::PayloadTypes, validate_execution_requests, validate_version_specific_fields,
    AddOnsContext, BuiltPayload, EngineApiMessageVersion, EngineApiValidator,
    EngineObjectValidationError, EngineTypes, FullNodeComponents, NewPayloadError, NodeTypes,
    PayloadOrAttributes, PayloadValidator,
};
use reth_node_builder::rpc::PayloadValidatorBuilder;
use reth_primitives_traits::{NodePrimitives, SealedBlock, SignedTransaction};
use serde::{Deserialize, Serialize};

use crate::{
    evm_config::{payload_spec_version, version_indexed, AlpenEvmConfig},
    payload::{AlpenBuiltPayload, AlpenExecutionPayloadEnvelopeV4},
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
    type ExecutionPayloadEnvelopeV6 = ExecutionPayloadEnvelopeV6;
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
        attributes.alpen_spec_version()
    }
}

impl PayloadValidator<AlpenEngineTypes> for AlpenEngineValidator {
    type Block = Block;

    fn convert_payload_to_block(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        let spec_version = payload_spec_version(&payload).map_err(NewPayloadError::other)?;
        let inner = version_indexed(&self.inners, spec_version);
        let block = inner
            .ensure_well_formed_payload(payload)
            .map_err(NewPayloadError::from)?;
        validate_transaction_signatures(&block)?;
        Ok(block)
    }
}

/// Recovers every payload transaction signer before Reth starts block execution.
///
/// Reth v2.2.0 otherwise discovers malformed signatures inside its payload-processing worker and
/// reports the aborted worker as an internal Engine API error. Rejecting the malformed payload at
/// this validation boundary returns the required `INVALID` payload status instead.
fn validate_transaction_signatures(block: &SealedBlock<Block>) -> Result<(), NewPayloadError> {
    block
        .body()
        .transactions()
        .try_for_each(|transaction| transaction.try_recover().map(|_| ()))
        .map_err(NewPayloadError::other)
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

#[cfg(test)]
mod tests {
    use alloy_consensus::{crypto::SECP256K1N_HALF, BlockBody, SignableTransaction, TxLegacy};
    use alloy_primitives::{Signature, U256};
    use reth_ethereum_primitives::{Block, TransactionSigned};
    use reth_node_api::NewPayloadError;
    use reth_primitives_traits::SealedBlock;

    use super::validate_transaction_signatures;

    fn sealed_block_with_signature(signature: Signature) -> SealedBlock<Block> {
        let transaction: TransactionSigned = TxLegacy::default().into_signed(signature).into();
        let body = BlockBody {
            transactions: vec![transaction],
            ..Default::default()
        };
        SealedBlock::seal_slow(Block {
            header: Default::default(),
            body,
        })
    }

    #[test]
    fn valid_payload_transaction_signature_is_accepted() {
        let block = sealed_block_with_signature(Signature::test_signature());

        validate_transaction_signatures(&block).expect("valid signature must be accepted");
    }

    #[test]
    fn malformed_payload_transaction_signature_is_invalid_payload_input() {
        let high_s_signature = Signature::new(U256::ONE, SECP256K1N_HALF + U256::ONE, false);
        let block = sealed_block_with_signature(high_s_signature);

        let error = validate_transaction_signatures(&block)
            .expect_err("high-s signature must be rejected before execution");

        assert!(matches!(error, NewPayloadError::Other(_)));
        assert_eq!(error.to_string(), "Failed to recover the signer");
    }
}
