use std::{
    cell::Cell,
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use alloy_consensus::{Header, Transaction};
use alpen_reth_evm::{
    base_fee::apply_base_fee_floor,
    constants::BRIDGEOUT_PRECOMPILE_ADDRESS,
    da_fee::{DA_COVERAGE_CAPPED, DA_COVERAGE_UNKNOWN},
    extract_withdrawal_intents,
};
use alpen_reth_primitives::WithdrawalIntent;
use reth_basic_payload_builder::*;
use reth_chainspec::{ChainSpec, ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_errors::{BlockExecutionError, BlockValidationError};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{
    block::CommitChanges,
    execute::{BlockBuilder, BlockBuilderOutcome},
    Evm, NextBlockEnvAttributes,
};
use reth_node_api::{ConfigureEvm, FullNodeTypes, NodeTypes, PayloadBuilderAttributes};
use reth_node_builder::{components::PayloadBuilderBuilder, BuilderContext, PayloadBuilderConfig};
use reth_payload_builder::{BlobSidecars, EthBuiltPayload, PayloadBuilderError};
use reth_primitives::{EthPrimitives, InvalidTransactionError, Receipt};
use reth_provider::{HeaderProvider, StateProviderFactory};
use reth_revm::database::StateProviderDatabase;
use reth_transaction_pool::{
    error::InvalidPoolTransactionError, BestTransactions, BestTransactionsAttributes,
    PoolTransaction, TransactionPool, ValidPoolTransaction,
};
use revm::{context::Block, database::State};
use revm_primitives::U256;
use tracing::{debug, info, trace, warn};

use crate::{
    block_witness::build_block_witness_from_executed_state,
    engine::AlpenEngineTypes,
    evm_config::AlpenEvmConfig,
    payload::{AlpenBuiltPayload, AlpenPayloadBuilderAttributes},
};

/// Intrinsic gas floor of the cheapest possible transaction (a plain value transfer).
///
/// Used to stop filling a block once the remaining gas can't fit even a minimal tx.
/// Block space is accounted on actual `gas_used`, so we do not pre-reject on the
/// DA-inflated signed `gas_limit`; the precise per-tx fit is checked post-execution.
const MIN_TX_GAS_LIMIT: u64 = 21_000;

/// A custom payload service builder that supports the custom engine types
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct AlpenPayloadBuilderBuilder {
    /// Live DA rate (wei per byte), shared with the payload builder.
    ///
    /// Shared and atomic — not because it changes *within* a block, but because the
    /// sequencer updates it *between* blocks, out of band from the build task (from
    /// its Bitcoin fee rate; see [`crate::payload_builder`]). The builder samples it
    /// once and freezes that value into the block, so a single relaxed load/store on
    /// an [`AtomicU64`] is all the synchronization the hand-off needs.
    pub live_da_rate: Arc<AtomicU64>,
}

impl<Node, Pool> PayloadBuilderBuilder<Node, Pool, AlpenEvmConfig> for AlpenPayloadBuilderBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Payload = AlpenEngineTypes,
            ChainSpec = ChainSpec,
            Primitives = EthPrimitives,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>
        + Unpin
        + 'static,
{
    type PayloadBuilder = AlpenPayloadBuilder<Pool, Node::Provider>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: AlpenEvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let conf = ctx.payload_builder_config();
        let chain = ctx.chain_spec().chain();
        let gas_limit = conf.gas_limit_for(chain);

        Ok(AlpenPayloadBuilder::new(
            ctx.provider().clone(),
            pool,
            evm_config,
            EthereumBuilderConfig::new().with_gas_limit(gas_limit),
            self.live_da_rate,
        ))
    }
}

/// The type responsible for building custom payloads
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AlpenPayloadBuilder<Pool, Client> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool.
    pool: Pool,
    /// The node's version-aware EVM config; payload jobs select the inner
    /// per-version config by the version carried on their attributes.
    evm_config: AlpenEvmConfig,
    /// Payload builder configuration.
    builder_config: EthereumBuilderConfig,
    /// Live DA rate (wei per byte) sampled and frozen per block.
    live_da_rate: Arc<AtomicU64>,
}

impl<Pool, Client> AlpenPayloadBuilder<Pool, Client> {
    /// `StrataPayloadBuilder` constructor.
    pub fn new(
        client: Client,
        pool: Pool,
        evm_config: AlpenEvmConfig,
        builder_config: EthereumBuilderConfig,
        live_da_rate: Arc<AtomicU64>,
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            builder_config,
            live_da_rate,
        }
    }
}

impl<Pool, Client> PayloadBuilder for AlpenPayloadBuilder<Pool, Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec = ChainSpec>
        + HeaderProvider<Header = Header>
        + Clone,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    type Attributes = AlpenPayloadBuilderAttributes;
    type BuiltPayload = AlpenBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        try_build_payload(
            self.evm_config.clone(),
            self.live_da_rate.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        let args = BuildArguments::new(Default::default(), config, Default::default(), None);
        try_build_payload(
            self.evm_config.clone(),
            self.live_da_rate.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}

type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// Constructs an Ethereum transaction payload using the best transactions from the pool.
///
/// Given build arguments including an Ethereum client, transaction pool,
/// and configuration, this function creates a transaction payload. Returns
/// a res ult indicating success with the payload or an error in case of failure.
///
/// Adapted from
/// [default_ethereum_payload](reth_ethereum_payload_builder::default_ethereum_payload)
#[inline]
fn try_build_payload<Pool, Client, F>(
    evm_config: AlpenEvmConfig,
    live_da_rate: Arc<AtomicU64>,
    client: Client,
    _pool: Pool,
    builder_config: EthereumBuilderConfig,
    args: BuildArguments<AlpenPayloadBuilderAttributes, AlpenBuiltPayload>,
    best_txs: F,
) -> Result<BuildOutcome<AlpenBuiltPayload>, PayloadBuilderError>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthereumHardforks>
        + HeaderProvider<Header = Header>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
    F: FnOnce(BestTransactionsAttributes) -> BestTransactionsIter<Pool>,
{
    // Freeze the per-block DA rate: sample the live rate once and use it both as the
    // in-EVM charge rate for this build and as the value committed into the block
    // `extra_data`. Freezing per block keeps the charge and the committed rate identical, so
    // the block re-executes to the same state root on full nodes/provers.
    //
    // NOTE: `live_da_rate` currently mirrors the sequencer's Bitcoin publication fee rate
    // (`btcio::writer::fees::resolve_fee_rate`, gossiped from the OL). It should later be
    // decoupled from the publication rate and smoothed/cached for the fee model.
    let da_rate = live_da_rate.load(Ordering::Relaxed);

    let BuildArguments {
        mut cached_reads,
        config,
        cancel,
        best_payload,
    } = args;
    let PayloadConfig {
        parent_header,
        attributes,
    } = config;

    let spec_version = attributes.spec_version();
    let attributes = attributes.inner;

    // Pin the per-block DA rate as the config's pending rate (the in-EVM charge reads it via
    // `context_for_next_block` when the block builder's executor is created). The assembler
    // stamps the committed `extra_data` itself, from the same spec version that selected the
    // build rules, so the charge and the commitment cannot drift and the block re-executes
    // to the same state root.
    let evm_config = evm_config.with_pending_da_rate(U256::from(da_rate));
    let versioned_config = evm_config.config_for(spec_version);

    let state_provider = client.state_by_block_hash(parent_header.hash())?;
    let state = StateProviderDatabase::new(&state_provider);
    let mut db = State::builder()
        .with_database(cached_reads.as_db_mut(state))
        .with_bundle_update()
        .build();

    let next_block_attrs = NextBlockEnvAttributes {
        timestamp: attributes.timestamp(),
        suggested_fee_recipient: attributes.suggested_fee_recipient(),
        prev_randao: attributes.prev_randao(),
        gas_limit: builder_config.gas_limit(parent_header.gas_limit),
        parent_beacon_block_root: attributes.parent_beacon_block_root(),
        withdrawals: Some(attributes.withdrawals().clone()),
    };

    // Build the next block's EVM env and apply the base-fee floor. `next_evm_env`
    // computes the pure EIP-1559 base fee; clamp it to `max(BASE_FEE_FLOOR, .)`. The sealed
    // header takes its base fee from this env, so flooring here keeps the header and the
    // executed base fee consistent, and matches the host consensus + guest, which recompute
    // the same floored value from the parent. This inlines `builder_for_next_block_with_version`
    // so the floor can be inserted between `next_evm_env` and block-builder construction,
    // keeping the floor logic in the builder rather than inside `AlpenEvmConfig`.
    //
    // The env comes from the version's inner config, but the builder is driven through the
    // outer version-aware config: that is what carries `spec_version` into the executor and
    // assembler, so the rules the block builds under are the rules its header claims.
    let mut evm_env = versioned_config
        .next_evm_env(&parent_header, &next_block_attrs)
        .map_err(PayloadBuilderError::other)?;
    evm_env.block_env.basefee = apply_base_fee_floor(evm_env.block_env.basefee);

    let evm = evm_config.evm_with_env(&mut db, evm_env);
    let block_ctx =
        evm_config.context_for_next_block_with_version(&parent_header, next_block_attrs, spec_version);
    let mut builder = evm_config.create_block_builder(evm, &parent_header, block_ctx);

    // Shared handle to *this build EVM's* DA-coverage cell: the in-EVM charge writes it per
    // transaction (`CAPPED` means the DA fee was capped by the tx's unused authorized gas —
    // under-covered / would be subsidized), and the tx loop reads it to skip such txs (see
    // the sequencer-admission skip below). Owned by the EVM, not shared factory state.
    let da_report = builder.evm().da_report_handle();

    // Fork queries must agree with the EVM env, so use the per-version spec
    // the block builds under, not the node's boot chain spec.
    let chain_spec = versioned_config.chain_spec().clone();

    debug!(target: "payload_builder", id=%attributes.id, parent_header = ?parent_header.hash(), parent_number = parent_header.number, "building new payload");
    let mut cumulative_gas_used = 0;
    let block_gas_limit: u64 = builder.evm_mut().block().gas_limit;

    let base_fee = builder.evm_mut().block().basefee;

    let mut best_txs = best_txs(BestTransactionsAttributes::new(
        base_fee,
        builder
            .evm_mut()
            .block()
            .blob_gasprice()
            .map(|gasprice| gasprice as u64),
    ));
    let mut total_fees = U256::ZERO;

    builder.apply_pre_execution_changes().map_err(|err| {
        warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
        PayloadBuilderError::Internal(err.into())
    })?;

    // Bound on the speculative execution the DA-inflated-limit design permits per build.
    //
    // A transaction's signed `gas_limit` is its execution gas plus the DA-fee headroom the
    // wallet reserves (`eth_estimateGas` folds it in). That headroom is a spend-authorization
    // envelope — the DA fee is a separate in-EVM balance debit drawn from the caller's unused
    // gas value, not metered execution — and for a storage-heavy transaction it can be large.
    // So a tx whose signed limit exceeds the remaining budget may still *execute* within it
    // and belongs in the block; its real execution gas is only known by running it, so it is
    // run *speculatively* rather than pre-rejected on the signed limit.
    //
    // That gamble is the attack surface: a tx executed at its full signed limit that then
    // doesn't fit on actual gas, or whose DA fee is under-covered, does full EVM work without
    // committing or paying. `speculative_gas_used` sums the gas burned by *every* such
    // uncommitted execution (both buckets — see the no-commit arms below); once it reaches a
    // block's worth, further *oversized* txs are rejected before execution, so a flood of them
    // can't force unbounded work. The loop keeps going — txs that fit their signed limit are
    // still committed — so this can't be used to truncate the block. (A fitting tx that is
    // merely DA-undercovered is a separate, pool-validation concern; it can't be pre-detected
    // here without executing.)
    let speculative_gas_budget = block_gas_limit;
    let mut speculative_gas_used: u64 = 0;

    while let Some(pool_tx) = best_txs.next() {
        // Stop once even a minimal transaction can no longer fit the remaining budget.
        if cumulative_gas_used + MIN_TX_GAS_LIMIT > block_gas_limit {
            break;
        }

        // Once this build has burned a block's worth of gas on uncommitted executions, stop
        // gambling on oversized txs: reject a tx whose signed limit exceeds the remaining
        // budget before executing it. Txs that fit their signed limit are unaffected and the
        // build continues, so this bounds speculative work without truncating the block.
        let remaining_gas = block_gas_limit.saturating_sub(cumulative_gas_used);
        if pool_tx.gas_limit() > remaining_gas && speculative_gas_used >= speculative_gas_budget {
            trace!(target: "payload_builder", gas_limit = pool_tx.gas_limit(), remaining_gas, "rejecting oversized transaction: speculative execution budget exhausted");
            best_txs.mark_invalid(
                &pool_tx,
                InvalidPoolTransactionError::ExceedsGasLimit(pool_tx.gas_limit(), block_gas_limit),
            );
            continue;
        }

        // check if the job was cancelled, if so we can exit early
        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled);
        }

        // convert tx to a signed transaction
        let tx = pool_tx.to_consensus();

        // Execute at the tx's full signed gas limit — never a reduced cap — so `gasleft()`-
        // and EIP-150-sensitive contracts run identically to a full-node/guest re-execution
        // (a lower cap would change `gasleft` and diverge the state root) and the in-EVM DA
        // charge sees the full authorized-but-unused gas it draws the DA fee from.
        //
        // Commit only if (a) the tx's *actual* execution gas fits the remaining block budget
        // — checked here, not on the DA-inflated signed limit, so a tx that fits despite an
        // oversized limit is kept — and (b) its DA fee is fully covered by its unused
        // authorized gas.
        //
        // Reset the shared coverage cell to `UNKNOWN` first so a stale value from a prior tx
        // (or a tx that skips the charge, e.g. zero-fee) can never be read as covered; the
        // in-EVM charge overwrites it with `OK`/`CAPPED` during execution (before this closure
        // runs). A `CAPPED` charge means the tx under-provisioned and the protocol would
        // subsidize its DA cost, so we skip it (coverage only matters when `da_rate != 0`).
        da_report.store(DA_COVERAGE_UNKNOWN, Ordering::Relaxed);
        // Captures the execution gas and reason of a tx the commit condition rejects, so the
        // skip arms can charge the burned gas against the speculative budget and pick the
        // right invalidation reason (the no-commit path returns neither).
        let does_not_fit = Cell::new(false);
        let rejected_gas = Cell::new(0u64);
        let exec_outcome = builder.execute_transaction_with_commit_condition(tx.clone(), |res| {
            // (a) Fit on actual executed gas, not the DA-inflated signed limit.
            if cumulative_gas_used + res.gas_used() > block_gas_limit {
                does_not_fit.set(true);
                rejected_gas.set(res.gas_used());
                return CommitChanges::No;
            }
            // (b) DA coverage: skip under-covered txs the protocol would subsidize.
            if da_rate != 0 && da_report.load(Ordering::Relaxed) == DA_COVERAGE_CAPPED {
                rejected_gas.set(res.gas_used());
                return CommitChanges::No;
            }
            CommitChanges::Yes
        });

        let gas_used = match exec_outcome {
            Ok(Some(gas_used)) => gas_used,
            Ok(None) => {
                // Executed but not committed — it either didn't fit on actual gas or its DA
                // fee was under-covered. Either way it did full EVM work without paying, so
                // charge that work to the speculative budget (bounding how much uncommitted
                // execution a flood can force per build; see the loop-top break), then skip it
                // and its descendants. The sender's nonce is untouched, so it can resubmit.
                speculative_gas_used = speculative_gas_used.saturating_add(rejected_gas.get());
                if does_not_fit.get() {
                    trace!(target: "payload_builder", ?tx, "skipping transaction that exceeds remaining block gas");
                    best_txs.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::ExceedsGasLimit(
                            pool_tx.gas_limit(),
                            block_gas_limit,
                        ),
                    );
                } else {
                    trace!(target: "payload_builder", ?tx, "skipping DA-undercovered transaction");
                    best_txs.mark_invalid(&pool_tx, InvalidPoolTransactionError::Underpriced);
                }
                continue;
            }
            Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                error, ..
            })) => {
                if error.is_nonce_too_low() {
                    // if the nonce is too low, we can skip this transaction
                    trace!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                } else {
                    // if the transaction is invalid, we can skip it and all of its
                    // descendants
                    trace!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                    best_txs.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::TxTypeNotSupported,
                        ),
                    );
                }
                continue;
            }
            // this is an error that we should treat as fatal for this attempt
            Err(err) => return Err(PayloadBuilderError::evm(err)),
        };

        // update and add to total fees
        let miner_fee = tx
            .effective_tip_per_gas(base_fee)
            .expect("fee is always valid; execution succeeded");
        total_fees += U256::from(miner_fee) * U256::from(gas_used);
        cumulative_gas_used += gas_used;
    }

    // check if we have a better block
    if !is_better_payload(best_payload.as_ref(), total_fees) {
        // Release db
        drop(builder);
        // can skip building the block
        return Ok(BuildOutcome::Aborted {
            fees: total_fees,
            cached_reads,
        });
    }

    let BlockBuilderOutcome {
        execution_result,
        block,
        ..
    } = builder.finish(&state_provider)?;

    // Inline depth-0 proof-witness capture. The block was just executed into
    // `db`; reuse that post-execution state (no re-execution) to read the
    // access set, gather depth-0 trie nodes from the parent `state_provider`,
    // and build the per-block partial-state witness. Encoded here and carried
    // on the payload back to the sequencer, which persists it. A failure fails
    // the payload build, so a block is never produced without its witness.
    let block_num = block.header().number;
    let block_rlp = alloy_rlp::encode(block.sealed_block().clone_block());
    let record = build_block_witness_from_executed_state(
        &db,
        &state_provider,
        &client,
        block_num,
        block_rlp,
        parent_header.header(),
    )
    .map_err(|e| PayloadBuilderError::other(io::Error::other(format!("witness capture: {e}"))))?;
    let block_witness = record.encode().map_err(|e| {
        PayloadBuilderError::other(io::Error::other(format!("witness encode: {e}")))
    })?;

    let requests = chain_spec
        .is_prague_active_at_timestamp(attributes.timestamp)
        .then_some(execution_result.requests);

    let sealed_block = Arc::new(block.sealed_block().clone());
    debug!(target: "payload_builder", id=%attributes.id, sealed_block_header = ?sealed_block.sealed_header(), "sealed built block");

    let eth_payload = EthBuiltPayload::new(attributes.id, sealed_block, total_fees, requests)
        // Blob transactions are not supported in the Alpen environment.
        // Using empty blob sidecars to maintain compatibility with the Engine API.
        .with_sidecars(BlobSidecars::Empty);

    // collect receipts from the executed transactions
    let receipts: Vec<Receipt> = execution_result.receipts;
    let txns: Vec<TransactionSigned> = block.body().transactions().cloned().collect();
    let bridgeout_log_count = receipts
        .iter()
        .flat_map(|receipt| receipt.logs.iter())
        .filter(|log| log.address == BRIDGEOUT_PRECOMPILE_ADDRESS)
        .count();
    let withdrawal_intents: Vec<WithdrawalIntent> = extract_withdrawal_intents(
        &txns,
        &receipts,
        versioned_config.evm_factory().bridge_params(),
    )
    .map_err(PayloadBuilderError::other)?;
    if bridgeout_log_count > 0 || !withdrawal_intents.is_empty() {
        info!(
            target: "payload_builder",
            id = %attributes.id,
            tx_count = txns.len(),
            receipt_count = receipts.len(),
            bridgeout_log_count,
            withdrawal_intent_count = withdrawal_intents.len(),
            "extracted withdrawal intents from built payload receipts",
        );
    }

    let strata_payload =
        AlpenBuiltPayload::new(eth_payload, withdrawal_intents).with_block_witness(block_witness);

    Ok(BuildOutcome::Better {
        payload: strata_payload,
        cached_reads,
    })
}
