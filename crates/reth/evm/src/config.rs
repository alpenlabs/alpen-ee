//! Alpen EVM configuration.
//!
//! [`AlpenEvmConfig`] executes blocks under one Alpen spec version. It is built from the chain
//! params and that version, so the node and the provers take the chain spec and the bridge
//! params from the same source.
//!
//! It is also the single seam through which a block's `extra_data` stamp ([`HeaderExtra`]: the
//! spec version and, from V1 on, the data-availability (DA) rate) reaches execution. It wraps
//! reth's [`EthEvmConfig`] parameterised with [`AlpenEvmFactory`] and carries the stamp in the one
//! value every block execution funnels through: the [`BlockExecutorFactory::ExecutionCtx`].
//!
//! # Why the execution context
//!
//! reth builds the executor for a block in exactly one shape — `create_executor(evm, ctx)`
//! — reached by *every* execution path:
//!
//! - engine `newPayload` validation on full nodes (`context_for_payload`),
//! - live sync and the EE-STF chunk executor and the ZK proof guest (`BasicBlockExecutor` →
//!   `context_for_block`),
//! - block building on the sequencer ([`AlpenEvmConfig::context_for_next_block_with`]).
//!
//! On the re-execution paths the context methods decode the stamp from the block or payload
//! `extra_data`, and a stamp that does not decode fails the block. On the build path the caller
//! passes the stamp in. [`AlpenBlockExecutorFactory::create_executor`] then sets the stamp's DA
//! rate on the EVM (a V0 stamp has none, so V0 blocks charge no DA fee), and
//! [`AlpenBlockAssembler`] writes the same stamp into the header it builds. So the charge always
//! sees the block's committed rate with no per-call-site plumbing, and a built block cannot claim a
//! rate other than the one it charged. Deriving the rate in `evm_for_block` alone is *not*
//! sufficient: the engine validator builds its EVM via `evm_with_env` + `create_executor` and never
//! calls `evm_for_block`.
//!
//! Because the stamp rides the per-execution context rather than shared config state,
//! concurrent executions (e.g. RPC re-execution racing the builder) cannot cross rates.
//!
//! Settlement returns the DA-coverage result on the detached transaction output.
//! The payload builder can discard a capped transaction without mutating accepted state;
//! replay applies the bounded charge and does not reject the transaction for being capped.
//!
//! # Version check
//!
//! Each context method also rejects a stamp that names a different spec version from the
//! config's ([`StampError::WrongVersion`]). The proof guests need this. A guest's version is
//! built into its program, so its verifying key binds it. Without the check, a guest would
//! prove a block under its own rules even when the block claims another version. Full nodes
//! run that block under the rules its stamp names, so the proof could settle a state they
//! never accepted. In the node the stamp picks the config, so the check never fails there. It
//! sits in the context methods because every execution path goes through them.
//!
//! # Cost of the custom context
//!
//! reth's `EthBlockAssembler` is bound to
//! `ExecutionCtx = EthBlockExecutionCtx`, so a custom context obliges a custom
//! [`BlockAssembler`]. [`AlpenBlockAssembler::assemble_block`] mirrors reth's header
//! assembly, except that it takes `extra_data` from the context's stamp; keep it in sync
//! when bumping reth.

use std::{convert::Infallible, sync::Arc};

use alloy_consensus::{
    proofs::{self, calculate_receipt_root},
    Block, BlockBody, BlockHeader, Header, Transaction as _, TxReceipt, EMPTY_OMMER_ROOT_HASH,
};
use alloy_eips::{
    eip4895::Withdrawals,
    eip7840::BlobParams,
    eip7928::{BlockAccessIndex, BlockAccessList},
    merge::BEACON_NONCE,
};
#[cfg(feature = "host")]
use alloy_rpc_types_engine::ExecutionData;
use alpen_params::{AlpenParams, AlpenSpecId, HeaderExtra, HeaderExtraError};
use evm2::{
    block::ethereum::{execution, EthTransactionResultWithState},
    evm::{Bal, DynDatabase},
};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{
    BlockAssembler, BlockAssemblerInput, BlockExecutionError, BlockExecutionOutput, BlockExecutor,
    BlockExecutorFactory, BundleState, ConfigureEvm, EvmEnvFor, ExecutionCtxFor, ExecutorTx,
    GasOutput, NextBlockEnvAttributes, RecoveredTx, StateUpdate,
};
#[cfg(feature = "host")]
use reth_evm::{ConfigureEngineEvm, ExecutableTxIterator};
use reth_evm_ethereum::{EthBlockExecutionCtx, EthEvmConfig};
use reth_primitives_traits::{logs_bloom, SealedBlock, SealedHeader};
use strata_bridge_params::BridgeParams;

use crate::{evm::AlpenEvmFactory, evm2_settlement::AlpenEvmTypes};

/// The inner reth Ethereum EVM config specialised with the Alpen EVM factory.
type Inner = EthEvmConfig<ChainSpec, AlpenEvmFactory>;

/// The inner reth Ethereum block executor factory (with the Alpen EVM factory).
type InnerBef = <Inner as ConfigureEvm>::BlockExecutorFactory;

/// The inner reth Ethereum block assembler.
type InnerAssembler = reth_evm_ethereum::EthBlockAssembler<ChainSpec>;

fn infallible<T>(result: Result<T, Infallible>) -> T {
    result.unwrap_or_else(|never| match never {})
}

/// Why a block's `extra_data` stamp can't run under an [`AlpenEvmConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StampError {
    /// The stamp does not decode under any layout this binary knows.
    #[error(transparent)]
    Decode(#[from] HeaderExtraError),

    /// The stamp names a different spec version from the one the config executes.
    #[error("block is stamped {stamped:?}, but this config executes {expected:?}")]
    WrongVersion {
        /// The version the config executes.
        expected: AlpenSpecId,
        /// The version the stamp names.
        stamped: AlpenSpecId,
    },
}

/// Per-block execution context: the standard Ethereum context plus the block's `extra_data`
/// stamp.
///
/// Only [`AlpenEvmConfig`]'s context methods build one, so a context always carries the stamp
/// its config resolved for the block.
#[derive(Debug, Clone)]
pub struct AlpenBlockExecutionCtx<'a> {
    inner: EthBlockExecutionCtx<'a>,
    header_extra: HeaderExtra,
}

impl AlpenBlockExecutionCtx<'_> {
    /// Returns the stamp the block executes under: its spec version and DA rate.
    pub const fn header_extra(&self) -> HeaderExtra {
        self.header_extra
    }
}

/// Block executor factory that stamps the per-block DA rate onto the EVM before execution.
///
/// Wraps reth's `EthBlockExecutorFactory`: the block's stamp travels in
/// [`AlpenBlockExecutionCtx`], and its DA rate is applied to the EVM in
/// [`create_executor`](Self::create_executor); everything else delegates unchanged.
#[derive(Debug, Clone)]
pub struct AlpenBlockExecutorFactory {
    inner: InnerBef,
}

impl BlockExecutorFactory for AlpenBlockExecutorFactory {
    type EvmFactory = AlpenEvmFactory;
    type EvmTypes = AlpenEvmTypes;
    type EvmEnv = <InnerBef as BlockExecutorFactory>::EvmEnv;
    type Evm<'a> = <InnerBef as BlockExecutorFactory>::Evm<'a>;
    type ExecutionCtx<'a> = AlpenBlockExecutionCtx<'a>;
    type Transaction = <InnerBef as BlockExecutorFactory>::Transaction;
    type Receipt = <InnerBef as BlockExecutorFactory>::Receipt;
    type Executor<'a> = AlpenBlockExecutor<<InnerBef as BlockExecutorFactory>::Executor<'a>>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.evm_factory()
    }

    fn evm_with_env<'a, DB: DynDatabase + 'a>(
        &self,
        db: DB,
        mut env: Self::EvmEnv,
    ) -> Self::Evm<'a> {
        let gas_limit = env.block.gas_limit.saturating_to::<u64>();
        let cap = gas_limit.saturating_mul(4);
        env.version
            .features
            .remove(evm2::EvmFeatures::BLOCK_GAS_LIMIT_CHECK);
        if env.version.feature(evm2::EvmFeatures::EIP8037) {
            env.version.tx_total_gas_cap = env.version.tx_total_gas_cap.min(cap);
        } else {
            env.version.tx_gas_limit_cap = env.version.tx_gas_limit_cap.min(cap);
        }
        self.inner.evm_with_env(db, env)
    }

    fn create_executor<'a>(
        &'a self,
        mut evm: Self::Evm<'a>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a>
    where
        Self: 'a,
    {
        let mut block = *evm.block();
        block.ext = ctx.header_extra.da_rate().unwrap_or(0);
        evm.set_block(block);
        AlpenBlockExecutor {
            inner: self.inner.create_executor(evm, ctx.inner),
            building_bal: false,
        }
    }
}

/// Keeps Ethereum's block processing while allowing a DA-inflated signed gas envelope.
/// Actual gas rather than the signed limit determines how much of the block a tx uses.
#[expect(
    missing_debug_implementations,
    reason = "the inner executor is not Debug"
)]
pub struct AlpenBlockExecutor<E> {
    inner: E,
    building_bal: bool,
}

impl<'a, E> BlockExecutor for AlpenBlockExecutor<E>
where
    E: BlockExecutor<
        Transaction = TransactionSigned,
        Evm = evm2::Evm<'a, AlpenEvmTypes>,
        TransactionResultWithState = EthTransactionResultWithState<AlpenEvmTypes>,
        BlockAccessList = Bal,
    >,
{
    type Transaction = E::Transaction;
    type Receipt = E::Receipt;
    type Evm = E::Evm;
    type TransactionResultWithState = E::TransactionResultWithState;
    type BlockAccessList = Bal;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn validate_transaction_gas_limit(
        &mut self,
        _gas_limit: u64,
    ) -> Result<(), BlockExecutionError> {
        // The EVM's per-transaction limit still applies. The builder bounds actual
        // gas before commit; consensus checks header gas on replay.
        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutorTx<Self>,
    ) -> Result<Self::TransactionResultWithState, BlockExecutionError> {
        let (transaction, tx) = tx.into_parts();
        let tx = tx.tx();
        if self.building_bal {
            let index = self.inner.receipts().len() as u64 + 1;
            self.inner
                .set_block_access_index(BlockAccessIndex::new(index));
        }
        // The registry rejects blob transactions. For all other types, route the
        // detached transaction through the standard receipt and state commit path.
        let result =
            execution::execute_transaction_without_commit(self.inner.evm_mut(), &transaction)
                .map_err(|err| execution::map_transaction_error(err, *tx.tx_hash()))?;
        Ok(EthTransactionResultWithState::new(
            result,
            tx.tx_type(),
            tx.blob_gas_used().unwrap_or_default(),
        ))
    }

    fn commit_transaction(
        &mut self,
        output: Self::TransactionResultWithState,
    ) -> Result<GasOutput, BlockExecutionError> {
        self.inner.commit_transaction(output)
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }

    fn finish_with_block_access_list(
        self,
    ) -> Result<(BlockExecutionOutput<Self::Receipt>, Option<BlockAccessList>), BlockExecutionError>
    {
        self.inner.finish_with_block_access_list()
    }

    fn finish_with_prepared_block_access_list(
        self,
    ) -> Result<(BlockExecutionOutput<Self::Receipt>, Option<Bal>), BlockExecutionError> {
        self.inner.finish_with_prepared_block_access_list()
    }

    fn into_state(self) -> BundleState {
        self.inner.into_state()
    }

    fn set_state_hook(&mut self, hook: impl FnMut(StateUpdate) + Send + 'static) -> bool {
        self.inner.set_state_hook(hook)
    }

    fn convert_block_access_list(list: &BlockAccessList) -> Result<Bal, BlockExecutionError> {
        E::convert_block_access_list(list)
    }

    fn set_block_access_list(&mut self, list: Arc<Bal>) {
        self.inner.set_block_access_list(list);
    }

    fn set_block_access_index(&mut self, index: BlockAccessIndex) {
        self.inner.set_block_access_index(index);
    }

    fn enable_block_access_list_builder(&mut self) {
        self.building_bal = true;
        self.inner.enable_block_access_list_builder();
    }

    fn take_block_access_list(&mut self) -> Option<BlockAccessList> {
        self.inner.take_block_access_list()
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }
}

/// Block assembler mirroring reth's `EthBlockAssembler`.
///
/// A custom [`BlockExecutorFactory::ExecutionCtx`] forces a custom assembler (reth's is bound
/// to `EthBlockExecutionCtx`). This is a faithful copy of reth's `assemble_block` reading the
/// wrapped Ethereum context, except for `extra_data`.
///
/// It writes the context's stamp into `extra_data`. The stamp is the one execution charged
/// under, so the header always commits to the version and DA rate the block actually ran with.
#[derive(Debug, Clone)]
pub struct AlpenBlockAssembler {
    inner: InnerAssembler,
}

impl BlockAssembler<AlpenBlockExecutorFactory> for AlpenBlockAssembler {
    type Block = Block<TransactionSigned>;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, AlpenBlockExecutorFactory>,
    ) -> Result<Self::Block, BlockExecutionError> {
        let BlockAssemblerInput {
            evm_env,
            execution_ctx: ctx,
            parent,
            transactions,
            output,
            state_root,
            ..
        } = input;
        let header_extra = ctx.header_extra;
        let ctx = ctx.inner;
        let chain_spec = &self.inner.chain_spec;
        let receipts = &output.receipts;

        let timestamp = evm_env.block.timestamp.saturating_to();

        let transactions_root = proofs::calculate_transaction_root(&transactions);
        let receipts_root = calculate_receipt_root(
            &receipts
                .iter()
                .map(|r| r.with_bloom_ref())
                .collect::<Vec<_>>(),
        );
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));

        let withdrawals = chain_spec
            .is_shanghai_active_at_timestamp(timestamp)
            .then(|| Withdrawals::new(ctx.withdrawals.map(|w| w.into_owned()).unwrap_or_default()));

        let withdrawals_root = withdrawals
            .as_deref()
            .map(|w| proofs::calculate_withdrawals_root(w));
        let requests_hash = chain_spec
            .is_prague_active_at_timestamp(timestamp)
            .then(|| output.requests.requests_hash());

        let mut excess_blob_gas = None;
        let mut block_blob_gas_used = None;

        // only determine cancun fields when active
        if chain_spec.is_cancun_active_at_timestamp(timestamp) {
            block_blob_gas_used = Some(output.blob_gas_used);
            excess_blob_gas = if chain_spec.is_cancun_active_at_timestamp(parent.timestamp) {
                parent.maybe_next_block_excess_blob_gas(
                    chain_spec.blob_params_at_timestamp(timestamp),
                )
            } else {
                // for the first post-fork block, both parent.blob_gas_used and
                // parent.excess_blob_gas are evaluated as 0
                Some(BlobParams::cancun().next_block_excess_blob_gas_osaka(0, 0, 0))
            };
        }

        let header = Header {
            parent_hash: ctx.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: evm_env.block.beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp,
            mix_hash: evm_env.block.prevrandao.into(),
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(evm_env.block.basefee.saturating_to()),
            number: evm_env.block.number.saturating_to(),
            gas_limit: evm_env.block.gas_limit.saturating_to(),
            difficulty: evm_env.block.difficulty,
            gas_used: output.gas_used,
            extra_data: header_extra.encode().into(),
            parent_beacon_block_root: ctx.parent_beacon_block_root,
            blob_gas_used: block_blob_gas_used,
            excess_blob_gas,
            requests_hash,
            block_access_list_hash: None,
            slot_number: None,
        };

        Ok(Block {
            header,
            body: BlockBody {
                transactions,
                ommers: Default::default(),
                withdrawals,
            },
        })
    }
}

/// Alpen EVM configuration wrapping reth's [`EthEvmConfig`].
///
/// See the [module docs](self) for how a block's `extra_data` stamp is threaded.
#[derive(Debug, Clone)]
pub struct AlpenEvmConfig {
    /// The spec version whose rules this config executes.
    spec_version: AlpenSpecId,
    inner: Inner,
    executor_factory: AlpenBlockExecutorFactory,
    #[cfg(feature = "host")]
    block_assembler: AlpenBlockAssembler,
}

impl AlpenEvmConfig {
    /// Creates the config that executes blocks under `spec_version`, with that version's chain
    /// spec and the bridge params from `params`.
    pub fn new(params: &AlpenParams, spec_version: AlpenSpecId) -> Self {
        let evm_factory = AlpenEvmFactory::new(*params.bridge_params());
        let inner = EthEvmConfig::new_with_evm_factory(
            params.chain_spec(spec_version).clone(),
            evm_factory,
        );
        Self {
            spec_version,
            executor_factory: AlpenBlockExecutorFactory {
                inner: inner.executor_factory.clone(),
            },
            #[cfg(feature = "host")]
            block_assembler: AlpenBlockAssembler {
                inner: inner.block_assembler.clone(),
            },
            inner,
        }
    }

    /// Returns the executor factory for native or zkVM replay.
    pub const fn block_executor_factory(&self) -> &AlpenBlockExecutorFactory {
        &self.executor_factory
    }

    /// Returns the spec version whose rules this config executes.
    pub const fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }

    /// Returns the bridge withdrawal policy the precompiles validate against.
    pub fn bridge_params(&self) -> &BridgeParams {
        self.executor_factory.evm_factory().bridge_params()
    }

    /// Returns the chain specification.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        self.inner.chain_spec()
    }

    /// Returns a reference to the inner Ethereum config.
    pub const fn inner(&self) -> &Inner {
        &self.inner
    }

    /// Builds the execution context for a new block on top of `parent`, stamped with
    /// `header_extra`.
    ///
    /// This is the block production path. The caller decides the stamp: the version the block
    /// builds under and the DA rate it charges. The assembler writes the same stamp into the
    /// built header. Fails if the stamp names a different version from this config's.
    pub fn context_for_next_block_with(
        &self,
        parent: &SealedHeader,
        attributes: NextBlockEnvAttributes,
        header_extra: HeaderExtra,
    ) -> Result<AlpenBlockExecutionCtx<'_>, StampError> {
        Ok(AlpenBlockExecutionCtx {
            inner: infallible(self.inner.context_for_next_block(parent, attributes)),
            header_extra: self.check_version(header_extra)?,
        })
    }

    /// Passes `header_extra` through if it names this config's spec version.
    fn check_version(&self, header_extra: HeaderExtra) -> Result<HeaderExtra, StampError> {
        let stamped = header_extra.spec_version();
        if stamped != self.spec_version {
            return Err(StampError::WrongVersion {
                expected: self.spec_version,
                stamped,
            });
        }
        Ok(header_extra)
    }
}

impl ConfigureEvm for AlpenEvmConfig {
    type Primitives = EthPrimitives;
    type Error = StampError;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = AlpenBlockExecutorFactory;
    #[cfg(feature = "host")]
    type BlockAssembler = AlpenBlockAssembler;

    #[cfg(feature = "host")]
    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    #[cfg(feature = "host")]
    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        Ok(infallible(self.inner.evm_env(header)))
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        Ok(infallible(self.inner.next_evm_env(parent, attributes)))
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block<TransactionSigned>>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error>
    where
        Self: 'a,
    {
        Ok(AlpenBlockExecutionCtx {
            inner: infallible(self.inner.context_for_block(block)),
            header_extra: self.check_version(HeaderExtra::of_header(block.header())?)?,
        })
    }

    /// Builds the context for a speculative next block, such as an RPC simulation.
    ///
    /// Block production uses [`AlpenEvmConfig::context_for_next_block_with`] instead. With no
    /// caller to pick the stamp, this one runs under this config's version and charges no DA
    /// fee.
    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        self.context_for_next_block_with(parent, attributes, HeaderExtra::new(self.spec_version, 0))
    }
}

#[cfg(feature = "host")]
impl ConfigureEngineEvm<ExecutionData> for AlpenEvmConfig {
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        Ok(infallible(self.inner.evm_env_for_payload(payload)))
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        // No genesis exemption: the genesis block is initialized locally and never arrives
        // as a payload.
        let header_extra = HeaderExtra::decode(&payload.payload.as_v1().extra_data)?;
        Ok(AlpenBlockExecutionCtx {
            inner: infallible(self.inner.context_for_payload(payload)),
            header_extra: self.check_version(header_extra)?,
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        Ok(infallible(self.inner.tx_iterator_for_payload(payload)))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Block, BlockBody, Header};
    use alloy_rpc_types_engine::{ExecutionData, ExecutionPayload};
    use alpen_params::{AlpenParams, AlpenSpecId, HeaderExtra, HeaderExtraError};
    use reth_ethereum_primitives::TransactionSigned;
    use reth_evm::{ConfigureEngineEvm, ConfigureEvm, NextBlockEnvAttributes};
    use reth_primitives_traits::{SealedBlock, SealedHeader};
    use revm_primitives::Bytes;

    use super::{AlpenEvmConfig, StampError};

    const DA_RATE: u64 = 7;

    fn config(spec_version: AlpenSpecId) -> AlpenEvmConfig {
        AlpenEvmConfig::new(&AlpenParams::default(), spec_version)
    }

    fn stamp(spec_version: AlpenSpecId) -> Bytes {
        HeaderExtra::new(spec_version, DA_RATE).encode().into()
    }

    fn block(number: u64, extra_data: Bytes) -> SealedBlock<Block<TransactionSigned>> {
        SealedBlock::seal_slow(Block {
            header: Header {
                number,
                extra_data,
                ..Default::default()
            },
            body: BlockBody::default(),
        })
    }

    fn wrong_version(expected: AlpenSpecId, stamped: AlpenSpecId) -> Option<StampError> {
        Some(StampError::WrongVersion { expected, stamped })
    }

    #[test]
    fn context_for_block_accepts_its_own_version() {
        for version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            let block = block(1, stamp(version));
            let ctx = config(version)
                .context_for_block(&block)
                .expect("a block stamped with the config's version runs");
            assert_eq!(ctx.header_extra(), HeaderExtra::new(version, DA_RATE));
        }
    }

    /// A proof guest's version is fixed by its program, so a block stamped for
    /// another version must fail instead of running under the guest's rules.
    #[test]
    fn context_for_block_rejects_another_version() {
        for (expected, stamped) in [
            (AlpenSpecId::V0, AlpenSpecId::V1),
            (AlpenSpecId::V1, AlpenSpecId::V0),
        ] {
            assert_eq!(
                config(expected)
                    .context_for_block(&block(1, stamp(stamped)))
                    .err(),
                wrong_version(expected, stamped)
            );
        }
    }

    /// An empty stamp reads as V0, so a V1 config refuses an unstamped block.
    #[test]
    fn unstamped_block_runs_only_under_v0() {
        let unstamped = block(1, Bytes::new());
        assert!(config(AlpenSpecId::V0)
            .context_for_block(&unstamped)
            .is_ok());
        assert_eq!(
            config(AlpenSpecId::V1).context_for_block(&unstamped).err(),
            wrong_version(AlpenSpecId::V1, AlpenSpecId::V0)
        );
    }

    /// The genesis block is V0 whatever its `extra_data` holds.
    #[test]
    fn genesis_block_runs_only_under_v0() {
        let genesis = block(0, Bytes::from_static(b"SC"));
        assert!(config(AlpenSpecId::V0).context_for_block(&genesis).is_ok());
        assert_eq!(
            config(AlpenSpecId::V1).context_for_block(&genesis).err(),
            wrong_version(AlpenSpecId::V1, AlpenSpecId::V0)
        );
    }

    #[test]
    fn malformed_stamp_fails_to_decode() {
        assert_eq!(
            config(AlpenSpecId::V0)
                .context_for_block(&block(1, Bytes::from_static(&[0x00])))
                .err(),
            Some(StampError::Decode(HeaderExtraError::TooShort { len: 1 }))
        );
    }

    #[test]
    fn context_for_payload_rejects_another_version() {
        let sealed = block(1, stamp(AlpenSpecId::V0));
        let (payload, sidecar) =
            ExecutionPayload::from_block_unchecked(sealed.hash(), &sealed.clone_block());
        let payload = ExecutionData { payload, sidecar };

        assert!(config(AlpenSpecId::V0)
            .context_for_payload(&payload)
            .is_ok());
        assert_eq!(
            config(AlpenSpecId::V1).context_for_payload(&payload).err(),
            wrong_version(AlpenSpecId::V1, AlpenSpecId::V0)
        );
    }

    #[test]
    fn build_path_rejects_another_version() {
        let parent = SealedHeader::seal_slow(Header::default());
        let attributes = NextBlockEnvAttributes {
            timestamp: 1,
            suggested_fee_recipient: Default::default(),
            prev_randao: Default::default(),
            gas_limit: 30_000_000,
            parent_beacon_block_root: None,
            withdrawals: Some(Default::default()),
            extra_data: Default::default(),
            slot_number: None,
        };

        assert_eq!(
            config(AlpenSpecId::V1)
                .context_for_next_block_with(
                    &parent,
                    attributes,
                    HeaderExtra::new(AlpenSpecId::V0, 0)
                )
                .err(),
            wrong_version(AlpenSpecId::V1, AlpenSpecId::V0)
        );
    }
}
