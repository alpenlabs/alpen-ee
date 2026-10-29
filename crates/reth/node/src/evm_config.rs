//! Version-aware EVM config: one immutable per-version config, resolved per
//! block.
//!
//! Reth assumes one chain spec for the lifetime of the EVM component, but the
//! Alpen spec version governing a block is decided per block, from the version
//! carried in the header's `extra_data` (see [`alpen_params::HeaderExtra`]).
//! This config keeps `NodeTypes::ChainSpec` and the surrounding generics
//! untouched: it holds the whole per-version table — one config for every
//! known [`AlpenSpecId`], all built from the same params — and dispatches each
//! [`ConfigureEvm`] call by the block's stamp. Nothing ever switches — every
//! version's rules stay live, which is what lets one node execute both sides
//! of an upgrade during sync, reorgs, and historical re-execution.
//!
//! Each entry of the table is an [`AlpenEvmConfig`], which executes one
//! version. It does the per-block work itself: it decodes the block's stamp
//! into the execution context, charges the stamp's DA rate, and writes the
//! stamp into every header it assembles. This layer only picks the entry.
//!
//! The dispatch has to reach *inside* reth's execution abstraction:
//! [`ConfigureEvm`] exposes its executor factory and block assembler through
//! context-free getters, so those dispatch too. They pick the entry by the
//! version in the stamp the execution context carries
//! ([`AlpenBlockExecutionCtx::header_extra`]). Since the assembler writes that
//! same stamp into the header, the version that selected the build rules is
//! the version import resolves from.

use std::{io, iter};

use alloy_eips::Decodable2718;
use alloy_primitives::Bytes;
use alloy_rpc_types::engine::payload::ExecutionData;
use alpen_params::{
    header_spec_version, peek_spec_version, AlpenParams, AlpenSpecId, EvmSpec, HeaderExtra,
    HeaderExtraError,
};
use alpen_reth_evm::{
    config::{
        AlpenBlockAssembler, AlpenBlockExecutionCtx, AlpenBlockExecutorFactory, AlpenEvmConfig,
        StampError,
    },
    evm::AlpenEvmFactory,
};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{
    block::{BlockExecutorFactory, StateDB},
    execute::{BlockAssembler, BlockAssemblerInput, BlockExecutionError},
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, EvmFactory, ExecutableTxIterator,
    NextBlockEnvAttributes,
};
use reth_primitives_traits::{Header, Recovered, SealedBlock, SealedHeader, SignedTransaction};
use revm::Inspector;

/// Version-aware [`ConfigureEvm`] over one [`AlpenEvmConfig`] per spec version.
#[derive(Debug, Clone)]
pub struct MultiSpecEvmConfig {
    /// The embedded EVM chain spec the table was derived from.
    evm_spec: EvmSpec,
    /// EVM config of each known [`AlpenSpecId`], indexed by discriminant.
    configs: Vec<AlpenEvmConfig>,
    executor_factory: MultiSpecBlockExecutorFactory,
    assembler: MultiSpecBlockAssembler,
}

impl MultiSpecEvmConfig {
    /// Creates the config over the rules of every known spec version in
    /// `params`.
    pub fn new(params: &AlpenParams) -> Self {
        let configs: Vec<AlpenEvmConfig> =
            iter::successors(Some(AlpenSpecId::V0), |version| version.successor().ok())
                .map(|version| AlpenEvmConfig::new(params, version))
                .collect();
        let executor_factory = MultiSpecBlockExecutorFactory {
            inners: configs
                .iter()
                .map(|config| config.block_executor_factory().clone())
                .collect(),
        };
        let assembler = MultiSpecBlockAssembler {
            inners: configs
                .iter()
                .map(|config| config.block_assembler().clone())
                .collect(),
        };

        Self {
            evm_spec: params.evm_spec().clone(),
            configs,
            executor_factory,
            assembler,
        }
    }

    /// Returns the embedded EVM chain spec the table was derived from.
    pub fn evm_spec(&self) -> &EvmSpec {
        &self.evm_spec
    }

    /// Returns the EVM config governing `spec_version`.
    ///
    /// Total: the table covers every known version, so only decoding a raw
    /// version out of chain data can fail, never the lookup.
    pub fn config_for(&self, spec_version: AlpenSpecId) -> &AlpenEvmConfig {
        version_indexed(&self.configs, spec_version)
    }

    /// Returns the EVM config governing `header`.
    fn config_for_header(&self, header: &Header) -> Result<&AlpenEvmConfig, HeaderExtraError> {
        Ok(self.config_for(header_spec_version(header)?))
    }

    /// Builds the execution context for a new block on top of `parent`,
    /// stamped with `header_extra`.
    ///
    /// Block production cannot use [`ConfigureEvm::context_for_next_block`]:
    /// that path continues the parent's version — all a header-only resolver
    /// can do — while the version to build under comes from the Alpen layer
    /// via the payload attributes, and the two differ at an upgrade
    /// boundary. The stamp's version picks the config, and the assembler
    /// writes the stamp into the built header.
    pub fn context_for_next_block_with(
        &self,
        parent: &SealedHeader,
        attributes: NextBlockEnvAttributes,
        header_extra: HeaderExtra,
    ) -> Result<AlpenBlockExecutionCtx<'_>, StampError> {
        self.config_for(header_extra.spec_version())
            .context_for_next_block_with(parent, attributes, header_extra)
    }
}

/// Indexes a per-version table by discriminant.
pub(crate) fn version_indexed<T>(table: &[T], spec_version: AlpenSpecId) -> &T {
    table
        .get(usize::from(u16::from(spec_version)))
        .expect("EvmSpec invariant: the table covers every known version")
}

/// Version-dispatching [`BlockExecutorFactory`]: `create_executor` picks the
/// inner factory by the version in the context's stamp.
#[derive(Debug, Clone)]
pub struct MultiSpecBlockExecutorFactory {
    inners: Vec<AlpenBlockExecutorFactory>,
}

impl BlockExecutorFactory for MultiSpecBlockExecutorFactory {
    type EvmFactory = AlpenEvmFactory;
    type ExecutionCtx<'a> = AlpenBlockExecutionCtx<'a>;
    type Transaction = <AlpenBlockExecutorFactory as BlockExecutorFactory>::Transaction;
    type Receipt = <AlpenBlockExecutorFactory as BlockExecutorFactory>::Receipt;
    type TxExecutionResult = <AlpenBlockExecutorFactory as BlockExecutorFactory>::TxExecutionResult;
    type Executor<'a, DB: StateDB, I: Inspector<<Self::EvmFactory as EvmFactory>::Context<DB>>> =
        <AlpenBlockExecutorFactory as BlockExecutorFactory>::Executor<'a, DB, I>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        // Every version shares the node's EVM factory; any entry serves.
        self.inners
            .first()
            .expect("the version space has at least the genesis version")
            .evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as EvmFactory>::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<<Self::EvmFactory as EvmFactory>::Context<DB>>,
    {
        version_indexed(&self.inners, ctx.header_extra().spec_version()).create_executor(evm, ctx)
    }
}

/// Version-dispatching [`BlockAssembler`]: assembles under the version in the
/// context's stamp. The inner assembler writes that stamp into the header.
#[derive(Debug, Clone)]
pub struct MultiSpecBlockAssembler {
    inners: Vec<AlpenBlockAssembler>,
}

impl BlockAssembler<MultiSpecBlockExecutorFactory> for MultiSpecBlockAssembler {
    type Block = <AlpenBlockAssembler as BlockAssembler<AlpenBlockExecutorFactory>>::Block;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, MultiSpecBlockExecutorFactory, Header>,
    ) -> Result<Self::Block, BlockExecutionError> {
        let spec_version = input.execution_ctx.header_extra().spec_version();
        version_indexed(&self.inners, spec_version).assemble_block(BlockAssemblerInput::<
            AlpenBlockExecutorFactory,
        >::new(
            input.evm_env,
            input.execution_ctx,
            input.parent,
            input.transactions,
            input.output,
            input.bundle_state,
            input.state_provider,
            input.state_root,
        ))
    }
}

impl ConfigureEvm for MultiSpecEvmConfig {
    type Primitives = EthPrimitives;
    type Error = StampError;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = MultiSpecBlockExecutorFactory;
    type BlockAssembler = MultiSpecBlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.config_for_header(header)?.evm_env(header)
    }

    /// Resolves next-block environments under the parent's version.
    ///
    /// Block *production* never takes this path — the payload builder
    /// resolves the version from its attributes and drives the inner config
    /// directly. This serves speculative next-block consumers (RPC pending
    /// block), for which continuing the tip's version is the right guess.
    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.config_for_header(parent)?
            .next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<reth_ethereum_primitives::Block>,
    ) -> Result<AlpenBlockExecutionCtx<'a>, Self::Error> {
        self.config_for_header(block.header())?
            .context_for_block(block)
    }

    /// See [`Self::next_evm_env`] on why the parent's version governs.
    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<AlpenBlockExecutionCtx<'_>, Self::Error> {
        self.config_for_header(parent.header())?
            .context_for_next_block(parent, attributes)
    }
}

// Required by the engine launch path: `BasicEngineValidator` executes
// incoming `newPayload` payloads through these before they are ever sealed
// blocks. Resolution reads the same stamped bytes as the block path, from the
// payload's `extra_data`.
impl ConfigureEngineEvm<ExecutionData> for MultiSpecEvmConfig {
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.config_for(payload_spec_version(payload)?)
            .evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<AlpenBlockExecutionCtx<'a>, Self::Error> {
        self.config_for(payload_spec_version(payload)?)
            .context_for_payload(payload)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        // Version-invariant, mirroring the inner config's implementation:
        // decoding and signer recovery predate any fork the table can vary.
        let txs = payload.payload.transactions().clone();
        let convert = |tx: Bytes| {
            let tx = reth_ethereum_primitives::TransactionSigned::decode_2718_exact(tx.as_ref())
                .map_err(io::Error::other)?;
            let signer = tx.try_recover().map_err(io::Error::other)?;
            Ok::<_, io::Error>(Recovered::new_unchecked(tx, signer))
        };
        Ok((txs, convert))
    }
}

/// Resolves the spec version claimed by a `newPayload` payload.
///
/// No genesis exemption here: the genesis block is initialized locally and
/// never arrives as a payload, so a payload whose `extra_data` does not
/// decode is correctly rejected.
pub fn payload_spec_version(payload: &ExecutionData) -> Result<AlpenSpecId, HeaderExtraError> {
    peek_spec_version(&payload.payload.as_v1().extra_data)
}

/// Returns placeholder params around `evm_spec`, for tests that build the
/// config from a genesis document of their own.
#[cfg(test)]
pub(crate) fn test_params(evm_spec: EvmSpec) -> AlpenParams {
    let defaults = AlpenParams::default();
    AlpenParams::new(
        defaults.strata_exec_account_id(),
        *defaults.bridge_params(),
        defaults.blob_spec(),
        defaults.spec_schedule().clone(),
        evm_spec,
        defaults.fee_spec().clone(),
    )
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use alpen_params::{AlpenSpecId, EvmSpec, HeaderExtra, HeaderExtraError};
    use reth_evm::{
        execute::{BlockAssembler, BlockAssemblerInput, BlockBuilder},
        ConfigureEvm, EvmEnv, NextBlockEnvAttributes,
    };
    use reth_primitives_traits::{Header, SealedHeader};
    use reth_revm::database::StateProviderDatabase;
    use reth_storage_api::noop::NoopProvider;
    use revm::{database::State, primitives::hardfork::SpecId};

    use super::{test_params, MultiSpecEvmConfig};

    /// A non-zero DA rate, so the tests catch a stamp that silently drops it.
    const DA_RATE: u64 = 1_500_000_000;

    /// The real two-version table: v0 up to Prague from the genesis document,
    /// v1 = v0 with Osaka on top (the code-owned delta).
    fn test_config() -> MultiSpecEvmConfig {
        let evm_spec: EvmSpec = serde_json::from_str(
            r#"{"config":{"chainId":2892,"shanghaiTime":0,"cancunTime":0,"pragueTime":0}}"#,
        )
        .expect("genesis document parses");
        MultiSpecEvmConfig::new(&test_params(evm_spec))
    }

    fn next_block_attributes() -> NextBlockEnvAttributes {
        NextBlockEnvAttributes {
            timestamp: 1,
            suggested_fee_recipient: Default::default(),
            prev_randao: Default::default(),
            gas_limit: 30_000_000,
            parent_beacon_block_root: None,
            withdrawals: Some(Default::default()),
            extra_data: Default::default(),
            slot_number: None,
        }
    }

    fn stamped_header(spec_version: AlpenSpecId) -> Header {
        Header {
            number: 1,
            extra_data: HeaderExtra::new(spec_version, 0).encode().into(),
            ..Default::default()
        }
    }

    #[test]
    fn evm_env_dispatches_by_header_stamp() {
        let config = test_config();

        let v0_env = config
            .evm_env(&stamped_header(AlpenSpecId::V0))
            .expect("v0 stamp resolves");
        assert_eq!(v0_env.cfg_env.spec, SpecId::PRAGUE);

        let v1_env = config
            .evm_env(&stamped_header(AlpenSpecId::V1))
            .expect("v1 stamp resolves");
        assert_eq!(v1_env.cfg_env.spec, SpecId::OSAKA);
    }

    /// Strict resolution: a truncated or future stamp fails instead of
    /// silently executing under some version's rules.
    #[test]
    fn malformed_or_future_stamps_are_refused() {
        let config = test_config();

        let truncated = Header {
            number: 1,
            extra_data: Bytes::from_static(&[0x00]),
            ..Default::default()
        };
        assert_eq!(
            config.evm_env(&truncated),
            Err(HeaderExtraError::TooShort { len: 1 }.into())
        );

        let future = Header {
            number: 1,
            extra_data: Bytes::from_static(&[0x00, 0x07]),
            ..Default::default()
        };
        assert_eq!(
            config.evm_env(&future),
            Err(HeaderExtraError::UnknownVersion(7).into())
        );
    }

    /// The genesis header's operator-authored `extra_data` is never decoded;
    /// block 0 is v0 by definition.
    #[test]
    fn genesis_header_resolves_to_v0() {
        let config = test_config();
        let genesis = Header {
            number: 0,
            extra_data: Bytes::from_static(b"SC"),
            ..Default::default()
        };

        let env = config.evm_env(&genesis).expect("genesis is exempt");
        assert_eq!(env.cfg_env.spec, SpecId::PRAGUE);
    }

    /// The production path pins what the functional fullnode-sync flow
    /// exercises end to end: a block built under an explicitly resolved
    /// stamp comes out carrying it, so strict import resolves the same
    /// version and rate the block was built with.
    #[test]
    fn production_builder_stamps_the_resolved_version() {
        // Shanghai-only so the empty-state build needs no post-Cancun system
        // calls; the per-version derivation on top is the real one.
        let evm_spec: EvmSpec =
            serde_json::from_str(r#"{"config":{"chainId":2892,"shanghaiTime":0}}"#)
                .expect("genesis document parses");
        let config = MultiSpecEvmConfig::new(&test_params(evm_spec));
        let parent = SealedHeader::seal_slow(Header {
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(7),
            ..Default::default()
        });
        let provider = NoopProvider::default();

        for version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            let mut db = State::builder()
                .with_database(StateProviderDatabase::new(&provider))
                .with_bundle_update()
                .build();
            let attributes = next_block_attributes();
            let evm_env = config
                .config_for(version)
                .next_evm_env(&parent, &attributes)
                .expect("next env resolves");
            let evm = config.evm_with_env(&mut db, evm_env);
            let ctx = config
                .context_for_next_block_with(
                    &parent,
                    attributes,
                    HeaderExtra::new(version, DA_RATE),
                )
                .expect("the stamp picks its own version's config");
            let mut builder = config.create_block_builder(evm, &parent, ctx);
            builder
                .apply_pre_execution_changes()
                .expect("empty pre-execution succeeds");
            let outcome = builder
                .finish(&provider, None)
                .expect("empty block assembles");

            assert_eq!(
                outcome.block.header().extra_data,
                Bytes::from(HeaderExtra::new(version, DA_RATE).encode()),
                "{version:?}"
            );
        }
    }

    /// The assembler writes the context's stamp into the built header — the
    /// same stamp whose version selected the build rules, closing the
    /// production/import loop. V0's stamp has no rate, so its header stays
    /// empty.
    #[test]
    fn assembled_blocks_carry_the_contexts_stamp() {
        let config = test_config();
        let parent = SealedHeader::seal_slow(Header::default());
        let output = Default::default();
        let bundle_state = Default::default();
        let provider = NoopProvider::default();

        for version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            let ctx = config
                .context_for_next_block_with(
                    &parent,
                    next_block_attributes(),
                    HeaderExtra::new(version, DA_RATE),
                )
                .expect("the stamp picks its own version's config");
            let block = config
                .assembler
                .assemble_block(BlockAssemblerInput::new(
                    EvmEnv::default(),
                    ctx,
                    &parent,
                    Vec::new(),
                    &output,
                    &bundle_state,
                    &provider,
                    Default::default(),
                ))
                .expect("empty block assembles");

            assert_eq!(
                block.header.extra_data,
                Bytes::from(HeaderExtra::new(version, DA_RATE).encode()),
                "{version:?}"
            );
        }
    }
}
