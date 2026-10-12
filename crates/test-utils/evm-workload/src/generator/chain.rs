//! A dev chain on a throwaway reth database, built the way the sequencer
//! builds blocks.

use std::{
    cell::Cell,
    sync::{atomic::Ordering, Arc},
};

use alloy_eips::eip4895::{Withdrawal, Withdrawals};
use alloy_primitives::{address, Address, Bytes, B256};
use alpen_exex::build_accessed_state;
use alpen_params::{AlpenParams, AlpenSpecId, HeaderExtra};
use alpen_reth_evm::{
    base_fee::next_floored_base_fee,
    da_fee::{DA_COVERAGE_CAPPED, DA_COVERAGE_UNKNOWN},
};
use alpen_reth_node::{
    build_block_witness_from_executed_state, BlockWitnessRecord, MultiSpecEvmConfig,
};
use alpen_reth_statediff::BlockStateChanges;
use alpen_witness::RangeWitnessExtractor;
use eyre::{bail, Context};
use reth_chainspec::ChainSpec;
use reth_db_common::init::init_genesis_with_settings;
use reth_evm::{
    execute::{BlockBuilder, BlockBuilderOutcome},
    ConfigureEvm, NextBlockEnvAttributes,
};
use reth_primitives_traits::SealedHeader;
use reth_provider::{
    providers::BlockchainProvider,
    test_utils::{create_test_provider_factory_with_chain_spec, MockNodeTypesWithDB},
    BlockWriter, ExecutionOutcome, HistoryWriter, OriginalValuesKnown, ProviderFactory,
    StageCheckpointWriter, StateWriteConfig, StateWriter, StorageSettings, TrieWriter,
};
use reth_revm::{database::StateProviderDatabase, db::State};
use strata_acct_types::Hash;

use super::{accounts::SignedTx, store::MemAccessedStateStore};

/// The spec version every generated block is built and stamped under. It
/// matches the version the SP1 guests prove under.
pub(crate) const SPEC_VERSION: AlpenSpecId = AlpenSpecId::V1;

/// DA rate stamped into every block, in wei per DA byte. About what a 2 sat/vB
/// L1 fee rate gives, so the in-EVM DA charge runs with a realistic value.
pub(crate) const DA_RATE: u64 = 5_000_000_000;

/// Seconds between blocks, the sequencer's default block time.
const BLOCK_TIME_SECS: u64 = 5;

/// Fee recipient the sequencer uses by default.
const BENEFICIARY: Address = address!("5400000000000000000000000000000000000010");

/// What a transaction did, for the caller to check against what it meant.
#[derive(Debug)]
pub(crate) struct TxOutcome {
    pub(crate) success: bool,
    pub(crate) output: Bytes,
}

/// A block built on the dev chain, with what the node records for it.
#[derive(Debug)]
pub(crate) struct BuiltBlock {
    pub(crate) hash: B256,
    pub(crate) number: u64,
    pub(crate) gas_used: u64,
    pub(crate) outcomes: Vec<TxOutcome>,
    /// The proof witness the payload builder captures.
    pub(crate) witness: BlockWitnessRecord,
    /// The state changes the state-diff exex records.
    pub(crate) state_changes: BlockStateChanges,
}

/// A chain that starts at the genesis in `params` and grows one block at a
/// time.
#[derive(Debug)]
pub(crate) struct DevChain {
    factory: ProviderFactory<MockNodeTypesWithDB>,
    evm_config: MultiSpecEvmConfig,
    chain_spec: Arc<ChainSpec>,
    base_fee_floor: u64,
    parent: SealedHeader,
    accessed_state: Arc<MemAccessedStateStore>,
}

impl DevChain {
    pub(crate) fn new(params: &AlpenParams) -> eyre::Result<Self> {
        let chain_spec = params.chain_spec(SPEC_VERSION).clone();
        let factory = create_test_provider_factory_with_chain_spec(chain_spec.clone());
        // The legacy layout keeps the history indices in MDBX, where
        // `build_block` can rebuild them from the changesets it writes. The
        // accessed-state records and the range witness both read state as of
        // earlier blocks through those indices.
        init_genesis_with_settings(&factory, StorageSettings::v1()).context("init genesis")?;
        let parent = SealedHeader::seal_slow(chain_spec.genesis_header().clone());

        Ok(Self {
            factory,
            evm_config: MultiSpecEvmConfig::new(params),
            chain_spec,
            base_fee_floor: params.fee_spec().base_fee_floor(SPEC_VERSION),
            parent,
            accessed_state: Arc::new(MemAccessedStateStore::default()),
        })
    }

    pub(crate) fn chain_id(&self) -> u64 {
        self.chain_spec.chain.id()
    }

    /// Builds the next block from `txs` and `deposits`, persists it, and
    /// returns what the node would record for it.
    ///
    /// Mirrors `try_build_payload` in `alpen-reth-node`: the same EVM config,
    /// base fee floor, stamp and witness capture. Unlike the payload builder it
    /// includes every transaction it is given, so it fails rather than drop
    /// one that does not fit or does not cover its DA fee.
    pub(crate) fn build_block(
        &mut self,
        txs: Vec<SignedTx>,
        deposits: &[(Address, u64)],
    ) -> eyre::Result<BuiltBlock> {
        let state_provider = self.factory.latest()?;
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(&state_provider))
            .with_bundle_update()
            .build();

        let withdrawals = deposits
            .iter()
            .enumerate()
            .map(|(index, (address, sats))| Withdrawal {
                index: index as u64,
                validator_index: 0,
                address: *address,
                // EIP-4895 amounts are in gwei, and 1 sat is 10 gwei.
                amount: sats * 10,
            })
            .collect();
        let timestamp = self.parent.timestamp + BLOCK_TIME_SECS;
        let attributes = NextBlockEnvAttributes {
            timestamp,
            suggested_fee_recipient: BENEFICIARY,
            prev_randao: B256::ZERO,
            gas_limit: self.parent.gas_limit,
            parent_beacon_block_root: Some(B256::ZERO),
            withdrawals: Some(Withdrawals::new(withdrawals)),
            extra_data: Bytes::new(),
            slot_number: None,
        };

        let mut evm_env = self
            .evm_config
            .config_for(SPEC_VERSION)
            .next_evm_env(&self.parent, &attributes)?;
        if let Some(base_fee) = next_floored_base_fee(
            self.parent.header(),
            self.chain_spec.as_ref(),
            self.parent.number + 1,
            timestamp,
            self.base_fee_floor,
        ) {
            evm_env.block_env.basefee = base_fee;
        }

        let evm = self.evm_config.evm_with_env(&mut db, evm_env);
        let ctx = self.evm_config.context_for_next_block_with(
            &self.parent,
            attributes,
            HeaderExtra::new(SPEC_VERSION, DA_RATE),
        );
        let mut builder = self.evm_config.create_block_builder(evm, &self.parent, ctx);
        let da_report = builder.evm().da_report_handle();
        builder.apply_pre_execution_changes()?;

        let mut outcomes = Vec::with_capacity(txs.len());
        for (index, tx) in txs.into_iter().enumerate() {
            da_report.store(DA_COVERAGE_UNKNOWN, Ordering::Relaxed);
            let success = Cell::new(false);
            let output = Cell::new(Bytes::new());
            builder.execute_transaction_with_result_closure(tx, |res| {
                success.set(res.result.result.is_success());
                output.set(res.result.result.output().cloned().unwrap_or_default());
            })?;
            if da_report.load(Ordering::Relaxed) == DA_COVERAGE_CAPPED {
                bail!("tx {index} does not cover its DA fee; raise its gas limit");
            }
            outcomes.push(TxOutcome {
                success: success.get(),
                output: output.take(),
            });
        }

        let BlockBuilderOutcome {
            execution_result,
            hashed_state,
            trie_updates,
            block,
        } = builder.finish(&state_provider, None)?;
        let number = block.number;
        let hash = block.hash();

        let record = build_block_witness_from_executed_state(
            &db,
            &state_provider,
            &self.factory,
            number,
            alloy_rlp::encode(block.sealed_block().clone_block()),
            self.parent.header(),
        )?;
        let state_changes = BlockStateChanges::from(&db.bundle_state);

        let outcome = ExecutionOutcome::new(
            db.take_bundle(),
            vec![execution_result.receipts],
            number,
            vec![execution_result.requests],
        );
        // The same writes as reth's `append_blocks_with_state`, except that the
        // history indices come from the written changesets. That function
        // indexes every account in the bundle reverts, including contracts
        // whose only change was storage. Those get no account changeset, so
        // reading them at an earlier block fails.
        let provider_rw = self.factory.provider_rw()?;
        provider_rw.insert_block(&block)?;
        provider_rw.write_state(
            &outcome,
            OriginalValuesKnown::No,
            StateWriteConfig::default(),
        )?;
        provider_rw.write_hashed_state(&hashed_state.into_sorted())?;
        // The next block's state root and witness read the trie tables, so
        // they have to follow the hashed state.
        provider_rw.write_trie_updates(trie_updates)?;
        provider_rw.update_history_indices(number..=number)?;
        provider_rw.update_pipeline_stages(number, false)?;
        provider_rw.commit()?;

        self.record_accessed_state(hash, number)?;
        self.parent = block.clone_sealed_header();

        Ok(BuiltBlock {
            hash,
            number,
            gas_used: execution_result.gas_used,
            outcomes,
            witness: record,
            state_changes,
        })
    }

    /// Records what the accessed-state exex would have stored for the block,
    /// for the range witness extractor to read later.
    fn record_accessed_state(&self, hash: B256, number: u64) -> eyre::Result<()> {
        let (record, bytecodes) =
            build_accessed_state(self.provider()?, self.evm_config.clone(), number)?;
        self.accessed_state
            .insert(Hash::from(hash.0), record, bytecodes);
        Ok(())
    }

    /// Builds the range witness the account prover reads for the blocks from
    /// `first` to `last`, encoded as an `EvmPartialState`.
    ///
    /// Runs the production extractor, which blocks on async store reads, so
    /// call it from a blocking thread inside a tokio runtime.
    pub(crate) fn range_pre_state(&self, first: B256, last: B256) -> eyre::Result<Vec<u8>> {
        let extractor = RangeWitnessExtractor::new(self.provider()?, self.accessed_state.clone());
        Ok(extractor
            .extract_range_witness(first, last)?
            .raw_partial_pre_state)
    }

    /// A provider over everything persisted so far. The node's helpers take a
    /// full provider, not the bare database factory.
    fn provider(&self) -> eyre::Result<BlockchainProvider<MockNodeTypesWithDB>> {
        Ok(BlockchainProvider::new(self.factory.clone())?)
    }
}
