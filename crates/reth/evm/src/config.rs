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
//! The per-transaction DA-coverage report is a separate, determinism-neutral *output* side
//! channel owned by each EVM instance
//! ([`AlpenAlloyEvm::da_report_handle`](crate::apis::AlpenAlloyEvm::da_report_handle)), not
//! by this config; it is threaded independently of the `da_rate` input handled here.
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
    Block, BlockBody, BlockHeader, Header, Transaction as _, TransactionEnvelope, TxReceipt,
    EMPTY_OMMER_ROOT_HASH,
};
use alloy_eips::{eip4895::Withdrawals, eip7840::BlobParams, merge::BEACON_NONCE, Encodable2718};
use alloy_rpc_types_engine::ExecutionData;
use alpen_params::{AlpenParams, AlpenSpecId, HeaderExtra, HeaderExtraError};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{
    block::{
        BlockExecutionResult, BlockExecutor, BlockExecutorFactory, ExecutableTx, GasOutput,
        OnStateHook, StateDB,
    },
    eth::{EthBlockExecutionCtx, EthTxResult},
    execute::{BlockAssembler, BlockAssemblerInput, BlockExecutionError},
    ConfigureEngineEvm, ConfigureEvm, Evm, EvmEnvFor, EvmFactory, ExecutableTxIterator,
    ExecutionCtxFor, NextBlockEnvAttributes, RecoveredTx,
};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{logs_bloom, SealedBlock, SealedHeader, SignedTransaction};
use revm::{context::Block as _, Inspector};
use revm_primitives::U256;
use strata_bridge_params::BridgeParams;

use crate::evm::AlpenEvmFactory;

/// The inner reth Ethereum EVM config specialised with the Alpen EVM factory.
type Inner = EthEvmConfig<ChainSpec, AlpenEvmFactory>;

/// The inner reth Ethereum block executor factory (with the Alpen EVM factory).
type InnerBef = <Inner as ConfigureEvm>::BlockExecutorFactory;

/// The inner reth Ethereum block assembler.
type InnerAssembler = <Inner as ConfigureEvm>::BlockAssembler;

fn infallible<T>(result: Result<T, Infallible>) -> T {
    result.unwrap_or_else(|never| match never {})
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
    type ExecutionCtx<'a> = AlpenBlockExecutionCtx<'a>;
    type Transaction = <InnerBef as BlockExecutorFactory>::Transaction;
    type Receipt = <InnerBef as BlockExecutorFactory>::Receipt;
    type TxExecutionResult = <InnerBef as BlockExecutorFactory>::TxExecutionResult;
    type Executor<'a, DB: StateDB, I: Inspector<<Self::EvmFactory as EvmFactory>::Context<DB>>> =
        AlpenBlockExecutor<<InnerBef as BlockExecutorFactory>::Executor<'a, DB, I>>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        mut evm: <Self::EvmFactory as EvmFactory>::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<<Self::EvmFactory as EvmFactory>::Context<DB>>,
    {
        // The one chokepoint: every block execution path reaches `create_executor`, so the
        // committed rate is applied here regardless of how the EVM was created.
        evm.set_da_rate(U256::from(ctx.header_extra.da_rate().unwrap_or(0)));
        AlpenBlockExecutor {
            inner: self.inner.create_executor(evm, ctx.inner),
        }
    }
}

/// Block executor that drops reth's per-transaction `gas_limit <= available block gas` bound,
/// delegating everything else to the wrapped Ethereum executor.
///
/// Under the fee model a transaction's signed `gas_limit` is the DA-inflated *authorized*
/// envelope (execution gas + DA-fee headroom), not execution work — DA is a separate balance
/// debit, not metered gas. A storage-heavy tx can therefore carry a `gas_limit` above the
/// block gas limit while its real execution fits. Block space is bounded on ACTUAL `gas_used`
/// instead: the builder stops filling on real gas (payload side) and re-execution/consensus
/// rejects any block whose `header.gas_used > header.gas_limit`. Only
/// [`execute_transaction_without_commit`](BlockExecutor::execute_transaction_without_commit)
/// changes (it mirrors `EthBlockExecutor` minus the
/// gas-availability check); all receipt/gas/commit logic is delegated untouched.
///
/// HARDENING NOTE: executed gas per tx is still bounded only by the tx's own signed
/// `gas_limit` (prepaid via balance), so a crafted invalid block could make a re-executor
/// burn up to that limit before the block-level check rejects it. A follow-up should cap
/// execution at the block gas limit while preserving the signed value for DA-headroom
/// accounting.
#[expect(
    missing_debug_implementations,
    reason = "thin executor wrapper over a non-Debug inner executor"
)]
pub struct AlpenBlockExecutor<E> {
    inner: E,
}

impl<E, H, T> BlockExecutor for AlpenBlockExecutor<E>
where
    E: BlockExecutor<Result = EthTxResult<H, T>>,
    E::Transaction: SignedTransaction + TransactionEnvelope<TxType = T>,
    E::Evm: Evm<HaltReason = H>,
    H: Send + 'static,
    T: Send + 'static,
{
    type Transaction = E::Transaction;
    type Receipt = E::Receipt;
    type Evm = E::Evm;
    type Result = E::Result;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        // Mirror `EthBlockExecutor` minus the `gas_limit <= available` check.
        let (tx_env, tx) = tx.into_parts();
        let result = self.inner.evm_mut().transact(tx_env).map_err(|err| {
            let hash = tx.tx().trie_hash();
            BlockExecutionError::evm(err, hash)
        })?;
        Ok(EthTxResult {
            result,
            blob_gas_used: tx.tx().blob_gas_used().unwrap_or_default(),
            tx_type: tx.tx().tx_type(),
        })
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        self.inner.commit_transaction(output)
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        self.inner.finish()
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.inner.set_state_hook(hook);
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

        let timestamp = evm_env.block_env.timestamp().saturating_to();

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
            beneficiary: evm_env.block_env.beneficiary(),
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp,
            mix_hash: evm_env.block_env.prevrandao().unwrap_or_default(),
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(evm_env.block_env.basefee()),
            number: evm_env.block_env.number().saturating_to(),
            gas_limit: evm_env.block_env.gas_limit(),
            difficulty: evm_env.block_env.difficulty(),
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
            block_assembler: AlpenBlockAssembler {
                inner: inner.block_assembler.clone(),
            },
            inner,
        }
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
    /// built header.
    pub fn context_for_next_block_with(
        &self,
        parent: &SealedHeader,
        attributes: NextBlockEnvAttributes,
        header_extra: HeaderExtra,
    ) -> AlpenBlockExecutionCtx<'_> {
        AlpenBlockExecutionCtx {
            inner: infallible(self.inner.context_for_next_block(parent, attributes)),
            header_extra,
        }
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
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(AlpenBlockExecutionCtx {
            inner: infallible(self.inner.context_for_block(block)),
            header_extra: HeaderExtra::of_header(block.header())?,
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
        Ok(self.context_for_next_block_with(
            parent,
            attributes,
            HeaderExtra::new(self.spec_version, 0),
        ))
    }
}

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
        Ok(AlpenBlockExecutionCtx {
            inner: infallible(self.inner.context_for_payload(payload)),
            header_extra: HeaderExtra::decode(&payload.payload.as_v1().extra_data)?,
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        Ok(infallible(self.inner.tx_iterator_for_payload(payload)))
    }
}
