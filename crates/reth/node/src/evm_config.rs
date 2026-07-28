//! Version-aware EVM config: one immutable per-version config, resolved per
//! block.
//!
//! Reth assumes one chain spec for the lifetime of the EVM component, but the
//! Alpen spec version governing a block is decided per block, from the version
//! carried in the header's `extra_data` (see [`alpen_ee_params::HeaderExtra`]).
//! This config keeps `NodeTypes::ChainSpec` and the surrounding generics
//! untouched: it holds the whole per-version table — total over the closed
//! [`AlpenSpecId`] enum by [`EvmSpec`]'s construction — and dispatches each
//! [`ConfigureEvm`] call by the block's stamp. Nothing ever switches — every
//! version's rules stay live, which is what lets one node execute both sides
//! of an upgrade during sync, reorgs, and historical re-execution.
//!
//! The dispatch has to reach *inside* reth's execution abstraction:
//! [`ConfigureEvm`] exposes its executor factory and block assembler through
//! context-free getters, so those are wrappers too, carrying the resolved
//! version in the execution context ([`AlpenBlockExecutionCtx`]) from the
//! `context_for_*` methods — the last point where the block is in hand — to
//! the executor and assembler. The assembler closes the production loop: it
//! stamps the context's version into every block it assembles, so the version
//! that selected the build rules is the version import resolves from.

use std::{convert::Infallible, io, sync::Arc};

use alloy_eips::Decodable2718;
use alloy_rpc_types::engine::payload::ExecutionData;
use alpen_ee_params::{
    header_spec_version, peek_spec_version, AlpenSpecId, EvmSpec, HeaderExtra, HeaderExtraError,
};
use alpen_reth_evm::evm::AlpenEvmFactory;
use reth_chainspec::ChainSpec;
use reth_evm::{
    block::{BlockExecutorFactory, BlockExecutorFor},
    eth::{EthBlockExecutionCtx, EthBlockExecutorFactory},
    execute::{BlockAssembler, BlockAssemblerInput, BlockBuilder, BlockExecutionError},
    ConfigureEngineEvm, ConfigureEvm, Database, EvmEnvFor, EvmFactory, ExecutableTxIterator,
    NextBlockEnvAttributes,
};
use reth_evm_ethereum::{EthBlockAssembler, EthEvmConfig, RethReceiptBuilder};
use reth_primitives::{
    transaction::SignedTransaction, EthPrimitives, Header, Recovered, SealedBlock, SealedHeader,
};
use revm::{database::State, Inspector};

/// The per-version inner EVM config the table is made of.
pub type VersionedEvmConfig = EthEvmConfig<ChainSpec, AlpenEvmFactory>;

type VersionedExecutorFactory =
    EthBlockExecutorFactory<RethReceiptBuilder, Arc<ChainSpec>, AlpenEvmFactory>;

fn infallible<T>(result: Result<T, Infallible>) -> T {
    result.unwrap_or_else(|never| match never {})
}

/// Version-aware [`ConfigureEvm`] over the per-version chain spec table.
#[derive(Debug, Clone)]
pub struct AlpenEvmConfig {
    /// EVM config of each known [`AlpenSpecId`], indexed by discriminant.
    configs: Vec<VersionedEvmConfig>,
    executor_factory: AlpenBlockExecutorFactory,
    assembler: AlpenBlockAssembler,
}

impl AlpenEvmConfig {
    /// Creates the config over `evm_spec`'s per-version chain spec table.
    pub fn new(evm_spec: &EvmSpec, evm_factory: AlpenEvmFactory) -> Self {
        let configs: Vec<VersionedEvmConfig> = evm_spec
            .chain_specs()
            .iter()
            .map(|spec| EthEvmConfig::new_with_evm_factory(spec.clone(), evm_factory.clone()))
            .collect();

        let executor_factory = AlpenBlockExecutorFactory {
            inners: configs
                .iter()
                .map(|config| config.executor_factory.clone())
                .collect(),
        };
        let assembler = AlpenBlockAssembler {
            inners: configs
                .iter()
                .map(|config| config.block_assembler.clone())
                .collect(),
        };

        Self {
            configs,
            executor_factory,
            assembler,
        }
    }

    /// Returns the inner EVM config governing `spec_version`.
    ///
    /// Total: the table covers every known version, so only decoding a raw
    /// version out of chain data can fail, never the lookup.
    pub fn config_for(&self, spec_version: AlpenSpecId) -> &VersionedEvmConfig {
        version_indexed(&self.configs, spec_version)
    }

    /// Returns the inner EVM config governing `header`.
    pub fn config_for_header(
        &self,
        header: &Header,
    ) -> Result<&VersionedEvmConfig, HeaderExtraError> {
        Ok(self.config_for(header_spec_version(header)?))
    }

    /// Creates a block builder for the next block under an explicitly
    /// resolved governing version.
    ///
    /// Block production cannot use [`ConfigureEvm::builder_for_next_block`]:
    /// that path continues the parent's version — all a header-only resolver
    /// can do — while the version to build under comes from the Alpen layer
    /// via the payload attributes, and the two differ at an upgrade
    /// boundary. Routing through this config's own executor factory and
    /// assembler (rather than driving the version's inner config directly)
    /// is also what stamps the version into the built header.
    pub fn builder_for_next_block_with_version<'a, DB: Database>(
        &'a self,
        db: &'a mut State<DB>,
        parent: &'a SealedHeader,
        attributes: NextBlockEnvAttributes,
        spec_version: AlpenSpecId,
    ) -> impl BlockBuilder<
        Primitives = EthPrimitives,
        Executor: BlockExecutorFor<'a, AlpenBlockExecutorFactory, DB>,
    > {
        let config = self.config_for(spec_version);
        let evm_env = infallible(config.next_evm_env(parent, &attributes));
        let evm = self.evm_with_env(db, evm_env);
        let ctx = AlpenBlockExecutionCtx {
            inner: infallible(config.context_for_next_block(parent, attributes)),
            spec_version,
        };
        self.create_block_builder(evm, parent, ctx)
    }
}

/// Indexes a per-version table by discriminant.
pub(crate) fn version_indexed<T>(table: &[T], spec_version: AlpenSpecId) -> &T {
    table
        .get(usize::from(u16::from(spec_version)))
        .expect("EvmSpec invariant: the table covers every known version")
}

/// Execution context carrying the block's resolved spec version from
/// `context_for_*` resolution to executor and assembler dispatch.
#[derive(Debug, Clone)]
pub struct AlpenBlockExecutionCtx<'a> {
    inner: EthBlockExecutionCtx<'a>,
    /// Resolved when the context was built — the last point the block was in
    /// hand — and carried to the dispatch sites reth reaches through
    /// context-free getters.
    spec_version: AlpenSpecId,
}

/// Version-dispatching [`BlockExecutorFactory`]: `create_executor` picks the
/// inner factory by the version carried on the context.
#[derive(Debug, Clone)]
pub struct AlpenBlockExecutorFactory {
    inners: Vec<VersionedExecutorFactory>,
}

impl BlockExecutorFactory for AlpenBlockExecutorFactory {
    type EvmFactory = AlpenEvmFactory;
    type ExecutionCtx<'a> = AlpenBlockExecutionCtx<'a>;
    type Transaction = <VersionedExecutorFactory as BlockExecutorFactory>::Transaction;
    type Receipt = <VersionedExecutorFactory as BlockExecutorFactory>::Receipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        // Every version shares the node's EVM factory; any entry serves.
        self.inners
            .first()
            .expect("the version space has at least the genesis version")
            .evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as EvmFactory>::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<<Self::EvmFactory as EvmFactory>::Context<&'a mut State<DB>>> + 'a,
    {
        version_indexed(&self.inners, ctx.spec_version).create_executor(evm, ctx.inner)
    }
}

/// Version-dispatching [`BlockAssembler`]: assembles under the version
/// carried on the execution context and stamps that version into the built
/// header's `extra_data`.
///
/// Stamping happens per assembled block, not via the inner assemblers'
/// static `extra_data` field: the layout is per-block data (future versions
/// add sequencer-set fields like the L1 fee rate), and taking the version
/// from the same context that selected the executor makes a skew between
/// build rules and stamp unrepresentable.
#[derive(Debug, Clone)]
pub struct AlpenBlockAssembler {
    inners: Vec<EthBlockAssembler<ChainSpec>>,
}

impl BlockAssembler<AlpenBlockExecutorFactory> for AlpenBlockAssembler {
    type Block = <EthBlockAssembler<ChainSpec> as BlockAssembler<VersionedExecutorFactory>>::Block;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, AlpenBlockExecutorFactory, Header>,
    ) -> Result<Self::Block, BlockExecutionError> {
        let spec_version = input.execution_ctx.spec_version;
        let assembler = version_indexed(&self.inners, spec_version);
        let mut block =
            assembler.assemble_block(BlockAssemblerInput::<VersionedExecutorFactory>::new(
                input.evm_env,
                input.execution_ctx.inner,
                input.parent,
                input.transactions,
                input.output,
                input.bundle_state,
                input.state_provider,
                input.state_root,
            ))?;
        block.header.extra_data = HeaderExtra::new(spec_version).encode().into();
        Ok(block)
    }
}

impl ConfigureEvm for AlpenEvmConfig {
    type Primitives = EthPrimitives;
    type Error = HeaderExtraError;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = AlpenBlockExecutorFactory;
    type BlockAssembler = AlpenBlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        Ok(infallible(self.config_for_header(header)?.evm_env(header)))
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
        Ok(infallible(
            self.config_for_header(parent)?
                .next_evm_env(parent, attributes),
        ))
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<reth_primitives::Block>,
    ) -> Result<AlpenBlockExecutionCtx<'a>, Self::Error> {
        let spec_version = header_spec_version(block.header())?;
        let config = self.config_for(spec_version);
        Ok(AlpenBlockExecutionCtx {
            inner: infallible(config.context_for_block(block)),
            spec_version,
        })
    }

    /// See [`Self::next_evm_env`] on why the parent's version governs.
    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<AlpenBlockExecutionCtx<'_>, Self::Error> {
        let spec_version = header_spec_version(parent.header())?;
        let config = self.config_for(spec_version);
        Ok(AlpenBlockExecutionCtx {
            inner: infallible(config.context_for_next_block(parent, attributes)),
            spec_version,
        })
    }
}

// Required by the engine launch path: `BasicEngineValidator` executes
// incoming `newPayload` payloads through these before they are ever sealed
// blocks. Resolution reads the same stamped bytes as the block path, from the
// payload's `extra_data`.
impl ConfigureEngineEvm<ExecutionData> for AlpenEvmConfig {
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        let config = self.config_for(payload_spec_version(payload)?);
        Ok(infallible(config.evm_env_for_payload(payload)))
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<AlpenBlockExecutionCtx<'a>, Self::Error> {
        let spec_version = payload_spec_version(payload)?;
        let config = self.config_for(spec_version);
        Ok(AlpenBlockExecutionCtx {
            inner: infallible(config.context_for_payload(payload)),
            spec_version,
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        // Version-invariant, mirroring the inner config's implementation:
        // decoding and signer recovery predate any fork the table can vary.
        Ok(payload
            .payload
            .transactions()
            .clone()
            .into_iter()
            .map(|tx| {
                let tx = reth_primitives::TransactionSigned::decode_2718_exact(tx.as_ref())
                    .map_err(io::Error::other)?;
                let signer = tx.try_recover().map_err(io::Error::other)?;
                Ok::<_, io::Error>(Recovered::new_unchecked(tx, signer))
            }))
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

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use alpen_ee_params::{AlpenSpecId, EvmSpec, HeaderExtra, HeaderExtraError};
    use alpen_reth_evm::evm::AlpenEvmFactory;
    use reth_evm::{
        eth::EthBlockExecutionCtx,
        execute::{BlockAssembler, BlockAssemblerInput, BlockBuilder},
        ConfigureEvm, EvmEnv, NextBlockEnvAttributes,
    };
    use reth_primitives::{Header, SealedHeader};
    use reth_revm::database::StateProviderDatabase;
    use reth_storage_api::noop::NoopProvider;
    use revm::{database::State, primitives::hardfork::SpecId};

    use super::{AlpenBlockExecutionCtx, AlpenEvmConfig};

    /// The real two-version table: v0 up to Prague from the genesis document,
    /// v1 = v0 with Osaka on top (the code-owned delta).
    fn test_config() -> AlpenEvmConfig {
        let evm_spec: EvmSpec = serde_json::from_str(
            r#"{"config":{"chainId":2892,"shanghaiTime":0,"cancunTime":0,"pragueTime":0}}"#,
        )
        .expect("genesis document parses");
        AlpenEvmConfig::new(&evm_spec, AlpenEvmFactory::default())
    }

    fn stamped_header(spec_version: AlpenSpecId) -> Header {
        Header {
            number: 1,
            extra_data: HeaderExtra::new(spec_version).encode().into(),
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

    /// Strict resolution: an unstamped or future-stamped non-genesis header
    /// fails instead of silently executing under some version's rules.
    #[test]
    fn malformed_or_future_stamps_are_refused() {
        let config = test_config();

        let unstamped = Header {
            number: 1,
            ..Default::default()
        };
        assert_eq!(
            config.evm_env(&unstamped),
            Err(HeaderExtraError::TooShort { len: 0 })
        );

        let future = Header {
            number: 1,
            extra_data: Bytes::from_static(&[0x00, 0x07]),
            ..Default::default()
        };
        assert_eq!(
            config.evm_env(&future),
            Err(HeaderExtraError::UnknownVersion(7))
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
    /// version comes out stamped with it — driving a version's inner config
    /// directly would skip the stamping assembler and produce a block strict
    /// import rejects.
    #[test]
    fn production_builder_stamps_the_resolved_version() {
        // Shanghai-only so the empty-state build needs no post-Cancun system
        // calls; the per-version derivation on top is the real one.
        let evm_spec: EvmSpec =
            serde_json::from_str(r#"{"config":{"chainId":2892,"shanghaiTime":0}}"#)
                .expect("genesis document parses");
        let config = AlpenEvmConfig::new(&evm_spec, AlpenEvmFactory::default());
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
            let mut builder = config.builder_for_next_block_with_version(
                &mut db,
                &parent,
                NextBlockEnvAttributes {
                    timestamp: 1,
                    suggested_fee_recipient: Default::default(),
                    prev_randao: Default::default(),
                    gas_limit: 30_000_000,
                    parent_beacon_block_root: None,
                    withdrawals: Some(Default::default()),
                },
                version,
            );
            builder
                .apply_pre_execution_changes()
                .expect("empty pre-execution succeeds");
            let outcome = builder.finish(&provider).expect("empty block assembles");

            assert_eq!(
                outcome.block.header().extra_data,
                Bytes::from(HeaderExtra::new(version).encode()),
                "{version:?}"
            );
        }
    }

    /// The assembler stamps the context's version into the built header —
    /// the same version that selected the build rules, closing the
    /// production/import loop.
    #[test]
    fn assembled_blocks_carry_the_contexts_version_stamp() {
        let config = test_config();
        let parent = SealedHeader::seal_slow(Header::default());
        let output = Default::default();
        let bundle_state = Default::default();
        let provider = NoopProvider::default();

        for version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            let ctx = AlpenBlockExecutionCtx {
                inner: EthBlockExecutionCtx {
                    parent_hash: parent.hash(),
                    parent_beacon_block_root: None,
                    ommers: &[],
                    withdrawals: None,
                },
                spec_version: version,
            };
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
                Bytes::from(HeaderExtra::new(version).encode()),
                "{version:?}"
            );
        }
    }
}
