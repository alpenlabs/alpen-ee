//! Alpen EVM configuration.
//!
//! [`AlpenEvmConfig`] is the single seam through which the per-block data-availability (DA)
//! rate reaches the in-EVM DA fee charge. It wraps reth's [`EthEvmConfig`] parameterised
//! with [`AlpenEvmFactory`] and threads a `da_rate` through the one value every block
//! execution funnels through: the [`BlockExecutorFactory::ExecutionCtx`].
//!
//! # Why the execution context
//!
//! reth builds the executor for a block in exactly one shape — `create_executor(evm, ctx)`
//! — reached by *every* execution path:
//!
//! - engine `newPayload` validation on full nodes (`context_for_payload`),
//! - live sync and the EE-STF chunk executor and the ZK proof guest (`BasicBlockExecutor` →
//!   `context_for_block`),
//! - block building on the sequencer (`context_for_next_block`).
//!
//! Deriving the rate when the context is built — from the block/payload `extra_data` on the
//! re-execution paths, from the pending rate on the build path — and stamping it onto the
//! EVM inside [`AlpenBlockExecutorFactory::create_executor`] means the charge always sees
//! the block's committed rate with no per-call-site plumbing, and no path can silently
//! charge a stale rate. Deriving it in `evm_for_block` alone is *not* sufficient: the engine
//! validator builds its EVM via `evm_with_env` + `create_executor` and never calls
//! `evm_for_block`.
//!
//! Because the rate rides the per-execution context/EVM rather than shared factory state,
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
//! assembly (the DA rate does not affect assembly — it only affects execution — so the
//! header logic is a faithful copy); keep it in sync when bumping reth.

use std::sync::Arc;

use alloy_consensus::{
    proofs::{self, calculate_receipt_root},
    Block, BlockBody, BlockHeader, Header, TxReceipt, EMPTY_OMMER_ROOT_HASH,
};
use alloy_eips::{eip7840::BlobParams, merge::BEACON_NONCE, Encodable2718};
use alloy_rpc_types_engine::ExecutionData;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{
    block::{
        BlockExecutionResult, BlockExecutor, BlockExecutorFactory, BlockExecutorFor, ExecutableTx,
        OnStateHook,
    },
    eth::EthBlockExecutionCtx,
    execute::{BlockAssembler, BlockAssemblerInput, BlockExecutionError},
    ConfigureEngineEvm, ConfigureEvm, Database, Evm, EvmEnvFor, EvmFactory, ExecutableTxIterator,
    ExecutionCtxFor, NextBlockEnvAttributes,
};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{logs_bloom, SealedBlock, SealedHeader, SignedTransaction};
use revm::{
    context::{result::ResultAndState, Block as _},
    database::State,
    Inspector,
};
use revm_primitives::{Bytes, U256};

use crate::{da_fee::da_rate_from_extra_data, evm::AlpenEvmFactory};

/// The inner reth Ethereum EVM config specialised with the Alpen EVM factory.
type Inner = EthEvmConfig<ChainSpec, AlpenEvmFactory>;

/// The inner reth Ethereum block executor factory (with the Alpen EVM factory).
type InnerBef = <Inner as ConfigureEvm>::BlockExecutorFactory;

/// The inner reth Ethereum block assembler.
type InnerAssembler = <Inner as ConfigureEvm>::BlockAssembler;

/// Reads the per-block DA rate (wei per byte) committed in a header `extra_data`.
fn da_rate_of(extra_data: &Bytes) -> U256 {
    U256::from(da_rate_from_extra_data(extra_data))
}

/// Per-block execution context: the standard Ethereum context plus the block's DA rate.
#[derive(Debug, Clone)]
pub struct AlpenBlockExecutionCtx<'a> {
    inner: EthBlockExecutionCtx<'a>,
    da_rate: U256,
}

impl<'a> AlpenBlockExecutionCtx<'a> {
    /// Creates a context pairing a standard Ethereum context with the
    /// block's DA rate.
    pub const fn new(inner: EthBlockExecutionCtx<'a>, da_rate: U256) -> Self {
        Self { inner, da_rate }
    }
}

impl AlpenBlockExecutionCtx<'_> {
    /// Returns the DA rate (wei per byte) this block executes under.
    ///
    /// The block assembler reads it back to commit the same rate into the
    /// header, so what a block charges and what it claims cannot diverge.
    pub const fn da_rate(&self) -> U256 {
        self.da_rate
    }
}

/// Block executor factory that stamps the per-block DA rate onto the EVM before execution.
///
/// Wraps reth's `EthBlockExecutorFactory`: the
/// DA rate travels in [`AlpenBlockExecutionCtx`] and is applied to the EVM in
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

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        mut evm: <Self::EvmFactory as EvmFactory>::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<<Self::EvmFactory as EvmFactory>::Context<&'a mut State<DB>>> + 'a,
    {
        // The one chokepoint: every block execution path reaches `create_executor`, so the
        // committed rate is applied here regardless of how the EVM was created.
        evm.set_da_rate(ctx.da_rate);
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

impl<E> BlockExecutor for AlpenBlockExecutor<E>
where
    E: BlockExecutor<Transaction: SignedTransaction>,
{
    type Transaction = E::Transaction;
    type Receipt = E::Receipt;
    type Evm = E::Evm;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<ResultAndState<<Self::Evm as Evm>::HaltReason>, BlockExecutionError> {
        // Mirror `EthBlockExecutor` minus the `gas_limit <= available` check.
        let hash = tx.tx().trie_hash();
        self.inner
            .evm_mut()
            .transact(&tx)
            .map_err(|err| BlockExecutionError::evm(err, hash))
    }

    fn commit_transaction(
        &mut self,
        output: ResultAndState<<Self::Evm as Evm>::HaltReason>,
        tx: impl ExecutableTx<Self>,
    ) -> Result<u64, BlockExecutionError> {
        self.inner.commit_transaction(output, tx)
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
/// to `EthBlockExecutionCtx`). Header assembly is DA-rate independent, so this is a faithful
/// copy of reth's `assemble_block` reading the wrapped Ethereum context.
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
            .then(|| ctx.withdrawals.map(|w| w.into_owned()).unwrap_or_default());

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
            extra_data: self.inner.extra_data.clone(),
            parent_beacon_block_root: ctx.parent_beacon_block_root,
            blob_gas_used: block_blob_gas_used,
            excess_blob_gas,
            requests_hash,
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
/// See the [module docs](self) for how the per-block DA rate is threaded.
#[derive(Debug, Clone)]
pub struct AlpenEvmConfig {
    inner: Inner,
    executor_factory: AlpenBlockExecutorFactory,
    block_assembler: AlpenBlockAssembler,
    /// DA rate (wei per byte) stamped when *building* the next block. Only consulted by
    /// [`context_for_next_block`](ConfigureEvm::context_for_next_block); re-execution paths
    /// derive the rate from the block's committed `extra_data` instead.
    pending_da_rate: U256,
}

impl AlpenEvmConfig {
    /// Creates an [`AlpenEvmConfig`] from a chain spec and the Alpen EVM factory.
    pub fn new(chain_spec: Arc<ChainSpec>, evm_factory: AlpenEvmFactory) -> Self {
        let inner = EthEvmConfig::new_with_evm_factory(chain_spec, evm_factory);
        Self {
            executor_factory: AlpenBlockExecutorFactory {
                inner: inner.executor_factory.clone(),
            },
            block_assembler: AlpenBlockAssembler {
                inner: inner.block_assembler.clone(),
            },
            inner,
            pending_da_rate: U256::ZERO,
        }
    }

    /// Sets the `extra_data` stamped into blocks assembled by this config.
    ///
    /// The sequencer's payload builder commits the per-block DA rate here; re-execution reads
    /// it back when building the execution context.
    pub fn with_extra_data(mut self, extra_data: Bytes) -> Self {
        self.block_assembler.inner.extra_data = extra_data;
        self
    }

    /// Sets the DA rate (wei per byte) applied when building the next block.
    pub const fn with_pending_da_rate(mut self, da_rate: U256) -> Self {
        self.pending_da_rate = da_rate;
        self
    }

    /// Returns the chain specification.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        self.inner.chain_spec()
    }

    /// Returns a reference to the inner Ethereum config.
    pub const fn inner(&self) -> &Inner {
        &self.inner
    }
}

impl ConfigureEvm for AlpenEvmConfig {
    type Primitives = EthPrimitives;
    type Error = <Inner as ConfigureEvm>::Error;
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
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block<TransactionSigned>>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(AlpenBlockExecutionCtx {
            inner: self.inner.context_for_block(block)?,
            da_rate: da_rate_of(&block.header().extra_data),
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        Ok(AlpenBlockExecutionCtx {
            inner: self.inner.context_for_next_block(parent, attributes)?,
            da_rate: self.pending_da_rate,
        })
    }
}

impl ConfigureEngineEvm<ExecutionData> for AlpenEvmConfig {
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(AlpenBlockExecutionCtx {
            inner: self.inner.context_for_payload(payload)?,
            da_rate: da_rate_of(&payload.payload.as_v1().extra_data),
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        self.inner.tx_iterator_for_payload(payload)
    }
}
