use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    iter,
    num::NonZeroU32,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex, MutexGuard},
};

use alpen_da_l1_extraction::{
    DaExtractor, DaL1Ref, FetchBlockError, FetchRangeError, L1BlockData, RecoveredDaBlob,
};
use alpen_da_provider::prepare_da_chunks;
use alpen_da_types::{da_blob_version, decode_da_blob, DaBlob, EvmHeaderSummary};
use alpen_database::RecoveredDaDbError;
use alpen_params::{AlpenParams, AlpenSpecId};
use alpen_reth_statediff::{test_utils as statediff_fixtures, BatchStateDiff};
use bitcoin::{
    absolute::LockTime,
    block::{self, Header},
    hashes::Hash,
    pow::CompactTarget,
    script::Builder,
    secp256k1::XOnlyPublicKey,
    transaction, Amount, Block, BlockHash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Txid, Witness,
};
use bitcoind_async_client::error::ClientError;
use futures::{stream, FutureExt, Stream};
use strata_identifiers::{L1BlockCommitment, L1Height};
use strata_l1_envelope_fmt::{test_utils as commit_reveal_fixtures, MAX_ENVELOPE_PAYLOAD_SIZE};
use strata_service::{AsyncService, Response, Service};

use crate::{
    bitcoin::FetchBitcoinTipError,
    context::DaVerifierContext,
    da_extraction::{DaRecoveryDriver, DaRecoveryError},
    service::DaVerifierService,
    state::{DaRecoveryState, DaVerifierError, DaVerifierServiceState},
};

type L1BlockResults = Vec<Result<L1BlockData, FetchBlockError>>;

// Defaults shared by the verifier test fixture.
const SEQUENCER_KEY_SEED: u8 = 7;
const TEST_L1_REORG_SAFE_DEPTH: u32 = 6;
const TEST_MAX_L1_SCAN_WINDOW_SIZE: NonZeroU32 = NonZeroU32::new(500).expect("500 is nonzero");

// Short-chain recovery scenarios.
const TEST_GENESIS_L1_HEIGHT: L1Height = 10;
/// Completion height when a commit at genesis has every reveal in the next block.
const TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT: L1Height = TEST_GENESIS_L1_HEIGHT + 1;
/// Last height a scan may reach, leaving a three-block window from genesis.
const TEST_REORG_SAFE_TIP: L1Height = TEST_GENESIS_L1_HEIGHT + 2;

// Bounded L1 scan-window scenarios.
const TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE: NonZeroU32 = NonZeroU32::new(3).expect("3 is nonzero");
const TEST_WINDOWED_REORG_SAFE_TIP: L1Height = TEST_GENESIS_L1_HEIGHT + 7;
const TEST_WINDOWED_BITCOIN_TIP: L1Height = TEST_WINDOWED_REORG_SAFE_TIP + TEST_L1_REORG_SAFE_DEPTH;

// Multi-tick recovery scenario.
const MULTI_TICK_GENESIS_L1_HEIGHT: L1Height = 100;
const MULTI_TICK_FIRST_TICK_SAFE_TIP: L1Height = 145;
const MULTI_TICK_FIRST_TICK_TIP: L1Height =
    MULTI_TICK_FIRST_TICK_SAFE_TIP + TEST_L1_REORG_SAFE_DEPTH;
const MULTI_TICK_SECOND_TICK_SAFE_TIP: L1Height = MULTI_TICK_FIRST_TICK_TIP;
const MULTI_TICK_SECOND_TICK_TIP: L1Height =
    MULTI_TICK_SECOND_TICK_SAFE_TIP + TEST_L1_REORG_SAFE_DEPTH;
const MULTI_TICK_UPDATE_1_COMPLETION_HEIGHT: L1Height = 105;
const MULTI_TICK_UPDATE_2_COMMIT_HEIGHT: L1Height = 110;
const MULTI_TICK_UPDATE_3_COMMIT_HEIGHT: L1Height = 111;
const MULTI_TICK_UPDATES_2_AND_3_COMPLETION_HEIGHT: L1Height = 112;
const MULTI_TICK_UPDATE_4_COMMIT_HEIGHT: L1Height = 118;
const MULTI_TICK_UPDATE_4_FIRST_REVEAL_HEIGHT: L1Height = 119;
const MULTI_TICK_UPDATE_4_COMPLETION_HEIGHT: L1Height = 120;
const MULTI_TICK_UPDATE_6_COMMIT_HEIGHT: L1Height = 125;
const MULTI_TICK_UPDATE_5_COMMIT_HEIGHT: L1Height = 126;
const MULTI_TICK_UPDATE_6_COMPLETION_HEIGHT: L1Height = 127;
const MULTI_TICK_UPDATE_5_COMPLETION_HEIGHT: L1Height = 128;
const MULTI_TICK_UPDATE_7_COMMIT_HEIGHT: L1Height = 132;
const MULTI_TICK_UPDATE_7_COMPLETION_HEIGHT: L1Height = 133;
const MULTI_TICK_UPDATE_8_COMPLETION_HEIGHT: L1Height = 140;
const MULTI_TICK_UPDATE_9_COMMIT_HEIGHT: L1Height = 145;
const MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT: L1Height = MULTI_TICK_UPDATE_9_COMMIT_HEIGHT + 1;

#[derive(Clone)]
struct TestEeUpdate {
    blob: DaBlob,
    commit_txid: Txid,
    commit: Transaction,
    reveals: Vec<Transaction>,
}

impl TestEeUpdate {
    fn blob(&self) -> &DaBlob {
        &self.blob
    }

    fn commit_txid(&self) -> Txid {
        self.commit_txid
    }

    fn commit_transaction(&self) -> &Transaction {
        &self.commit
    }

    fn reveal_transactions(&self) -> &[Transaction] {
        &self.reveals
    }
}

struct TestEeSequencer {
    key_seed: u8,
    next_update_seq_no: u64,
}

impl TestEeSequencer {
    fn new(key_seed: u8) -> Self {
        Self {
            key_seed,
            next_update_seq_no: 0,
        }
    }

    fn public_key(&self) -> XOnlyPublicKey {
        commit_reveal_fixtures::make_xonly_pubkey(self.key_seed)
    }

    fn produce_update(&mut self, params: &AlpenParams, state_diff: BatchStateDiff) -> TestEeUpdate {
        self.produce_update_with_max_chunk_payload(params, state_diff, MAX_ENVELOPE_PAYLOAD_SIZE)
    }

    fn produce_two_reveal_update(
        &mut self,
        params: &AlpenParams,
        state_diff: BatchStateDiff,
    ) -> TestEeUpdate {
        let blob = build_da_blob(self.next_update_seq_no, state_diff);
        let encoded_len = blob
            .encode_to_vec()
            .expect("test DA blob should encode")
            .len();
        let max_chunk_payload = encoded_len.div_ceil(2);
        let update = self.produce_blob(params, blob, max_chunk_payload);
        assert_eq!(
            update.reveal_transactions().len(),
            2,
            "test chunk size must produce two reveals"
        );
        update
    }

    fn produce_update_with_max_chunk_payload(
        &mut self,
        params: &AlpenParams,
        state_diff: BatchStateDiff,
        max_chunk_payload: usize,
    ) -> TestEeUpdate {
        let blob = build_da_blob(self.next_update_seq_no, state_diff);
        self.produce_blob(params, blob, max_chunk_payload)
    }

    fn produce_blob(
        &mut self,
        params: &AlpenParams,
        blob: DaBlob,
        max_chunk_payload: usize,
    ) -> TestEeUpdate {
        let update_seq_no = self.next_update_seq_no;
        self.next_update_seq_no = self
            .next_update_seq_no
            .checked_add(1)
            .expect("test update sequence should not overflow");
        assert_eq!(blob.update_seq_no, update_seq_no);
        let chunks =
            prepare_da_chunks(&blob, max_chunk_payload).expect("test DA blob should encode");
        let mut transactions = commit_reveal_fixtures::build_commit_reveal_set(
            &params.blob_spec().magic_bytes(),
            &da_blob_version(blob.spec_version).to_be_bytes(),
            &chunks,
            self.key_seed,
        );

        // `commit_reveal_fixtures::build_commit_reveal_set` uses a null funding outpoint.
        // An update-specific outpoint guarantees each test envelope a distinct funding input
        // and commit txid even if its payload construction changes.
        let mut funding_txid_bytes = [0; 32];
        funding_txid_bytes[..8].copy_from_slice(&self.next_update_seq_no.to_le_bytes());
        let funding_txid = Txid::from_byte_array(funding_txid_bytes);
        transactions.commit.input[0].previous_output = OutPoint::new(funding_txid, 0);
        let commit_txid = transactions.commit.compute_txid();
        for reveal in &mut transactions.reveals {
            reveal.input[0].previous_output.txid = commit_txid;
        }

        TestEeUpdate {
            blob,
            commit_txid,
            commit: transactions.commit,
            reveals: transactions.reveals,
        }
    }
}

/// Builds an L1 block carrying a coinbase followed by `txs`.
///
/// Recovery reads the block ID and transactions without validating the merkle
/// root. The `block_discriminator` gives blocks at the same height distinct IDs.
fn build_l1_block_data(
    height: L1Height,
    block_discriminator: u32,
    txs: Vec<Transaction>,
) -> L1BlockData {
    let mut txdata = vec![build_coinbase_tx(height)];
    txdata.extend(txs);
    let block = Block {
        header: Header {
            version: block::Version::from_consensus(1),
            prev_blockhash: BlockHash::from_byte_array([height.wrapping_sub(1) as u8; 32]),
            merkle_root: TxMerkleNode::all_zeros(),
            time: height,
            bits: CompactTarget::from_consensus(0),
            nonce: block_discriminator,
        },
        txdata,
    };
    L1BlockData::new(height, block)
}

/// Builds the coinbase every L1 block carries.
///
/// A block with no transactions cannot exist. A coinbase-only block models
/// "no EE DA at this height" with a structurally valid transaction the scanner
/// ignores. The height goes in the script as BIP34 requires, which also keeps
/// each block's coinbase txid distinct.
fn build_coinbase_tx(height: L1Height) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: Builder::new().push_int(i64::from(height)).into_script(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(5_000_000_000),
            script_pubkey: ScriptBuf::new(),
        }],
    }
}

/// Builds a deterministic, non-empty batch state diff.
///
/// Recovery never interprets the diff, but real content is what makes chunk
/// reassembly observable: a dropped, duplicated or reordered chunk changes the
/// encoded blob instead of only its header.
fn build_test_state_diff() -> BatchStateDiff {
    let mut block = statediff_fixtures::block_diff();
    let account = statediff_fixtures::addr(1);
    let code_hash = statediff_fixtures::hash(2);
    statediff_fixtures::account_change(
        &mut block,
        account,
        None,
        Some(statediff_fixtures::snapshot(1_000, 1, code_hash)),
    );
    statediff_fixtures::storage_change(
        &mut block,
        account,
        statediff_fixtures::slot(7),
        statediff_fixtures::value(0),
        statediff_fixtures::value(42),
    );
    statediff_fixtures::deployed_bytecode(
        &mut block,
        code_hash,
        statediff_fixtures::bytecode(&[0x60; 128]),
    );
    let diff = statediff_fixtures::batch_diff(&[block]);
    assert!(!diff.is_empty());
    diff
}

fn build_da_blob(update_seq_no: u64, state_diff: BatchStateDiff) -> DaBlob {
    DaBlob {
        spec_version: AlpenSpecId::V0,
        update_seq_no,
        evm_header: EvmHeaderSummary {
            block_num: update_seq_no + 1,
            timestamp: 1_700_000_000 + update_seq_no,
            base_fee: 100,
            gas_used: 21_000,
            gas_limit: 36_000_000,
            da_rate: None,
        },
        state_diff,
    }
}

#[derive(Default)]
struct MockBitcoinChain {
    tip_height: L1Height,
    blocks: BTreeMap<L1Height, L1BlockData>,
    pending_transactions: HashMap<Txid, PendingBitcoinTransaction>,
    confirmed_transactions: HashMap<Txid, L1BlockCommitment>,
}

struct PendingBitcoinTransaction {
    transaction: Transaction,
    required_commit: Option<Txid>,
}

impl MockBitcoinChain {
    fn set_tip_height(&mut self, tip_height: L1Height) {
        assert!(
            tip_height >= self.blocks.keys().next_back().copied().unwrap_or_default(),
            "Bitcoin tip cannot precede a mined test block"
        );
        self.tip_height = tip_height;
    }

    fn publish_da(&mut self, update: &TestEeUpdate) {
        self.insert_pending(update.commit.clone(), None);
        for reveal in &update.reveals {
            self.insert_pending(reveal.clone(), Some(update.commit_txid));
        }
    }

    fn insert_pending(&mut self, transaction: Transaction, required_commit: Option<Txid>) {
        let txid = transaction.compute_txid();
        assert!(
            self.pending_transactions
                .insert(
                    txid,
                    PendingBitcoinTransaction {
                        transaction,
                        required_commit,
                    },
                )
                .is_none(),
            "test transaction {txid} was already submitted"
        );
    }

    fn mine_block(&mut self, height: L1Height, transaction_ids: &[Txid]) -> L1BlockCommitment {
        assert!(
            !self.blocks.contains_key(&height),
            "test block {height} was already mined"
        );
        if let Some(last_height) = self.blocks.keys().next_back() {
            assert!(
                height > *last_height,
                "test block {height} must follow mined height {last_height}"
            );
        }

        let mut available_transactions = self
            .confirmed_transactions
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        for txid in transaction_ids {
            let pending = self
                .pending_transactions
                .get(txid)
                .unwrap_or_else(|| panic!("test transaction {txid} is not pending"));
            if let Some(commit_txid) = pending.required_commit {
                assert!(
                    available_transactions.contains(&commit_txid),
                    "reveal {txid} cannot confirm before commit {commit_txid}"
                );
            }
            available_transactions.insert(*txid);
        }

        let transactions = transaction_ids
            .iter()
            .map(|txid| {
                self.pending_transactions
                    .remove(txid)
                    .expect("validated test transaction remains pending")
                    .transaction
            })
            .collect();
        let block = build_l1_block_data(height, 0, transactions);
        let commitment = L1BlockCommitment::new(block.height(), block.block_id());
        for txid in transaction_ids {
            assert!(
                self.confirmed_transactions
                    .insert(*txid, commitment)
                    .is_none(),
                "test transaction {txid} was already confirmed"
            );
        }
        self.blocks.insert(height, block);
        self.tip_height = self.tip_height.max(height);
        commitment
    }

    fn block_range(&self, start_height: L1Height, end_height: L1Height) -> L1BlockResults {
        assert!(
            end_height <= self.tip_height,
            "L1 range end {end_height} exceeds modeled Bitcoin tip {}",
            self.tip_height
        );
        (start_height..=end_height)
            .map(|height| {
                Ok(self
                    .blocks
                    .get(&height)
                    .cloned()
                    .unwrap_or_else(|| build_l1_block_data(height, 0, Vec::new())))
            })
            .collect()
    }
}

#[derive(Default)]
struct InMemoryRecoveredDaStore {
    blobs: BTreeMap<RecoveredDaCandidateKey, StoredRecoveredDaValue>,
}

type RecoveredDaCandidateKey = (u64, Txid);

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredRecoveredDaValue {
    completion_block: L1BlockCommitment,
    spec_version: AlpenSpecId,
    payload_bytes: Vec<u8>,
}

impl InMemoryRecoveredDaStore {
    fn put(&mut self, recovered_blobs: &[RecoveredDaBlob]) -> Result<(), RecoveredDaDbError> {
        let encoded_blobs = recovered_blobs
            .iter()
            .map(encode_recovered_da_blob)
            .collect::<Result<Vec<_>, _>>()?;

        for (key, value) in &encoded_blobs {
            if self.blobs.get(key).is_some_and(|stored| stored != value) {
                return Err(RecoveredDaDbError::BlobConflict {
                    update_seq_no: key.0,
                    commit_txid: key.1,
                });
            }
        }

        for (key, value) in encoded_blobs {
            self.blobs.entry(key).or_insert(value);
        }
        Ok(())
    }

    fn blobs(&self) -> Vec<RecoveredDaBlob> {
        self.blobs
            .iter()
            .map(|(&(_, commit_txid), stored)| {
                let blob = decode_da_blob(&stored.payload_bytes, stored.spec_version)
                    .expect("mock store contains only encoded DA blobs");
                RecoveredDaBlob::new(DaL1Ref::new(commit_txid, stored.completion_block), blob)
            })
            .collect()
    }
}

fn encode_recovered_da_blob(
    recovered_blob: &RecoveredDaBlob,
) -> Result<(RecoveredDaCandidateKey, StoredRecoveredDaValue), RecoveredDaDbError> {
    let l1_ref = recovered_blob.l1_ref();
    Ok((
        (recovered_blob.blob().update_seq_no, l1_ref.commit_txid()),
        StoredRecoveredDaValue {
            completion_block: l1_ref.completion_block(),
            spec_version: recovered_blob.blob().spec_version,
            payload_bytes: recovered_blob.blob().encode_to_vec()?,
        },
    ))
}

#[derive(Default)]
struct InjectedContextBehavior {
    bitcoin_tip_failures: VecDeque<FetchBitcoinTipError>,
    block_fetch_failures: BTreeMap<L1Height, FetchBlockError>,
    l1_block_range_overrides: VecDeque<Result<L1BlockResults, FetchRangeError>>,
    recovered_da_write_failures: VecDeque<RecoveredDaDbError>,
}

impl InjectedContextBehavior {
    fn fail_next_bitcoin_tip(&mut self, error: FetchBitcoinTipError) {
        self.bitcoin_tip_failures.push_back(error);
    }

    fn take_next_bitcoin_tip_failure(&mut self) -> Option<FetchBitcoinTipError> {
        self.bitcoin_tip_failures.pop_front()
    }

    fn override_next_l1_block_range(&mut self, result: Result<L1BlockResults, FetchRangeError>) {
        self.l1_block_range_overrides.push_back(result);
    }

    fn take_next_l1_block_range_override(
        &mut self,
    ) -> Option<Result<L1BlockResults, FetchRangeError>> {
        self.l1_block_range_overrides.pop_front()
    }

    fn fail_block_fetch_at(&mut self, height: L1Height, error: FetchBlockError) {
        assert!(
            self.block_fetch_failures.insert(height, error).is_none(),
            "L1 block fetch failure already injected at height {height}"
        );
    }

    fn take_block_fetch_failure(&mut self, height: L1Height) -> Option<FetchBlockError> {
        self.block_fetch_failures.remove(&height)
    }

    fn fail_next_recovered_da_write(&mut self, error: RecoveredDaDbError) {
        self.recovered_da_write_failures.push_back(error);
    }

    fn take_next_recovered_da_write_failure(&mut self) -> Option<RecoveredDaDbError> {
        self.recovered_da_write_failures.pop_front()
    }

    fn assert_consumed(&self) {
        assert!(
            self.bitcoin_tip_failures.is_empty(),
            "unconsumed Bitcoin tip failures"
        );
        assert!(
            self.block_fetch_failures.is_empty(),
            "unconsumed L1 block fetch failures"
        );
        assert!(
            self.l1_block_range_overrides.is_empty(),
            "unconsumed L1 block range overrides"
        );
        assert!(
            self.recovered_da_write_failures.is_empty(),
            "unconsumed recovered-DA write failures"
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextOperation {
    FetchBitcoinTip,
    FetchL1BlockRange {
        start_height: L1Height,
        end_height: L1Height,
    },
    PutRecoveredDa,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveredDaWriteOutcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug)]
struct RecoveredDaWriteAttempt {
    blobs: Vec<RecoveredDaBlob>,
    outcome: RecoveredDaWriteOutcome,
}

impl RecoveredDaWriteAttempt {
    fn blobs(&self) -> &[RecoveredDaBlob] {
        &self.blobs
    }

    fn outcome(&self) -> RecoveredDaWriteOutcome {
        self.outcome
    }
}

#[derive(Clone, Debug)]
enum ContextEvent {
    FetchBitcoinTip,
    FetchL1BlockRange {
        start_height: L1Height,
        end_height: L1Height,
    },
    PutRecoveredDa(RecoveredDaWriteAttempt),
}

impl ContextEvent {
    fn operation(&self) -> ContextOperation {
        match self {
            Self::FetchBitcoinTip => ContextOperation::FetchBitcoinTip,
            Self::FetchL1BlockRange {
                start_height,
                end_height,
            } => ContextOperation::FetchL1BlockRange {
                start_height: *start_height,
                end_height: *end_height,
            },
            Self::PutRecoveredDa(_) => ContextOperation::PutRecoveredDa,
        }
    }
}

#[derive(Default)]
struct ContextEventLog(Vec<ContextEvent>);

impl ContextEventLog {
    fn record(&mut self, event: ContextEvent) {
        self.0.push(event);
    }

    fn operations(&self) -> Vec<ContextOperation> {
        self.0.iter().map(ContextEvent::operation).collect()
    }

    fn recovered_da_write_attempts(&self) -> Vec<RecoveredDaWriteAttempt> {
        self.0
            .iter()
            .filter_map(|event| match event {
                ContextEvent::PutRecoveredDa(attempt) => Some(attempt.clone()),
                ContextEvent::FetchBitcoinTip | ContextEvent::FetchL1BlockRange { .. } => None,
            })
            .collect()
    }
}

#[derive(Default)]
struct MockDaVerifierContextState {
    bitcoin: MockBitcoinChain,
    recovered_da: InMemoryRecoveredDaStore,
    injected_behavior: InjectedContextBehavior,
    // A unified log preserves ordering across all external capabilities.
    events: ContextEventLog,
}

struct MockDaVerifierContext {
    state: Mutex<MockDaVerifierContextState>,
}

impl MockDaVerifierContext {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MockDaVerifierContextState::default()),
        })
    }

    fn state(&self) -> MutexGuard<'_, MockDaVerifierContextState> {
        self.state.lock().expect("mock verifier context lock")
    }
}

impl DaVerifierContext for MockDaVerifierContext {
    async fn fetch_bitcoin_tip_height(&self) -> Result<L1Height, FetchBitcoinTipError> {
        let mut state = self.state();
        state.events.record(ContextEvent::FetchBitcoinTip);
        if let Some(error) = state.injected_behavior.take_next_bitcoin_tip_failure() {
            return Err(error);
        }
        Ok(state.bitcoin.tip_height)
    }

    fn fetch_l1_block_range(
        &self,
        start_height: L1Height,
        end_height: L1Height,
    ) -> Result<impl Stream<Item = Result<L1BlockData, FetchBlockError>> + Send + '_, FetchRangeError>
    {
        let mut state = self.state();
        state.events.record(ContextEvent::FetchL1BlockRange {
            start_height,
            end_height,
        });
        let range_override = state.injected_behavior.take_next_l1_block_range_override();
        let has_range_override = range_override.is_some();
        let mut blocks = match range_override {
            Some(result) => result?,
            None => state.bitcoin.block_range(start_height, end_height),
        };
        if !has_range_override {
            for (height, block) in (start_height..=end_height).zip(&mut blocks) {
                if let Some(error) = state.injected_behavior.take_block_fetch_failure(height) {
                    *block = Err(error);
                }
            }
        }
        Ok(stream::iter(blocks))
    }

    async fn put_recovered_da(
        &self,
        recovered_blobs: Vec<RecoveredDaBlob>,
    ) -> Result<(), RecoveredDaDbError> {
        let mut state = self.state();
        let result = if let Some(error) = state
            .injected_behavior
            .take_next_recovered_da_write_failure()
        {
            Err(error)
        } else {
            state.recovered_da.put(&recovered_blobs)
        };
        let outcome = if result.is_ok() {
            RecoveredDaWriteOutcome::Succeeded
        } else {
            RecoveredDaWriteOutcome::Failed
        };
        state
            .events
            .record(ContextEvent::PutRecoveredDa(RecoveredDaWriteAttempt {
                blobs: recovered_blobs,
                outcome,
            }));

        result
    }
}

struct DaVerifierFixture {
    params: AlpenParams,
    genesis_l1_height: L1Height,
    l1_reorg_safe_depth: u32,
    max_l1_scan_window_size: NonZeroU32,
    context: Arc<MockDaVerifierContext>,
    sequencer: TestEeSequencer,
}

impl DaVerifierFixture {
    fn new(params: AlpenParams, genesis_l1_height: L1Height, l1_reorg_safe_depth: u32) -> Self {
        Self {
            params,
            genesis_l1_height,
            l1_reorg_safe_depth,
            max_l1_scan_window_size: TEST_MAX_L1_SCAN_WINDOW_SIZE,
            context: MockDaVerifierContext::new(),
            sequencer: TestEeSequencer::new(SEQUENCER_KEY_SEED),
        }
    }

    fn with_bitcoin_tip_height(self, tip_height: L1Height) -> Self {
        self.set_bitcoin_tip_height(tip_height);
        self
    }

    fn with_max_l1_scan_window_size(mut self, max_l1_scan_window_size: NonZeroU32) -> Self {
        self.max_l1_scan_window_size = max_l1_scan_window_size;
        self
    }

    fn set_bitcoin_tip_height(&self, tip_height: L1Height) {
        self.context.state().bitcoin.set_tip_height(tip_height);
    }

    fn produce_update(&mut self, state_diff: BatchStateDiff) -> TestEeUpdate {
        self.sequencer.produce_update(&self.params, state_diff)
    }

    fn produce_two_reveal_update(&mut self, state_diff: BatchStateDiff) -> TestEeUpdate {
        self.sequencer
            .produce_two_reveal_update(&self.params, state_diff)
    }

    fn publish_da(&self, update: &TestEeUpdate) {
        self.context.state().bitcoin.publish_da(update);
    }

    fn mine_block<'a>(
        &self,
        height: L1Height,
        transactions: impl IntoIterator<Item = &'a Transaction>,
    ) -> L1BlockCommitment {
        let transaction_ids = transactions
            .into_iter()
            .map(Transaction::compute_txid)
            .collect::<Vec<_>>();
        self.context
            .state()
            .bitcoin
            .mine_block(height, &transaction_ids)
    }

    fn build_da_extractor(&self) -> DaExtractor {
        DaExtractor::new(
            self.params.blob_spec().magic_bytes(),
            self.sequencer.public_key(),
        )
    }

    fn build_verifier_service_state(&self) -> DaVerifierServiceState<MockDaVerifierContext> {
        let recovery_state = DaRecoveryState::new(
            self.l1_reorg_safe_depth,
            self.max_l1_scan_window_size,
            self.build_da_extractor(),
            self.genesis_l1_height,
        );
        DaVerifierServiceState::new(Arc::clone(&self.context), recovery_state)
    }

    fn fail_next_bitcoin_tip(&self, error: FetchBitcoinTipError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_bitcoin_tip(error);
    }

    fn override_next_l1_block_range(&self, result: Result<L1BlockResults, FetchRangeError>) {
        self.context
            .state()
            .injected_behavior
            .override_next_l1_block_range(result);
    }

    fn fail_block_fetch_at(&self, height: L1Height, error: FetchBlockError) {
        self.context
            .state()
            .injected_behavior
            .fail_block_fetch_at(height, error);
    }

    fn fail_next_recovered_da_write(&self, error: RecoveredDaDbError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_recovered_da_write(error);
    }

    fn context_operations(&self) -> Vec<ContextOperation> {
        self.context.state().events.operations()
    }

    fn recovered_da_write_attempts(&self) -> Vec<RecoveredDaWriteAttempt> {
        self.context.state().events.recovered_da_write_attempts()
    }

    fn recovered_da_blobs(&self) -> Vec<RecoveredDaBlob> {
        self.context.state().recovered_da.blobs()
    }

    fn assert_injected_behavior_consumed(&self) {
        self.context.state().injected_behavior.assert_consumed();
    }

    fn build_da_recovery_driver(&self) -> DaRecoveryDriver {
        DaRecoveryDriver::new(self.build_da_extractor(), self.genesis_l1_height)
    }

    async fn recover_da_through(
        &self,
        driver: &mut DaRecoveryDriver,
        end_height: L1Height,
    ) -> Result<(), DaRecoveryError> {
        driver
            .recover_through(self.context.as_ref(), end_height)
            .await
    }
}

/// Returns a panic payload's message.
///
/// A bare `assert!` panics with `&str` and a formatted one with `String`, so
/// both shapes are accepted.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .expect("panic payload is a string")
}

/// Asserts `blobs` holds exactly the expected published blobs and L1 completions.
///
/// The sequence number is asserted first so the common failure names itself,
/// then the encoded blobs are compared, which covers the header and the state
/// diff together. Any lost, duplicated or reordered chunk changes those bytes.
fn assert_recovered_blobs(
    blobs: &[RecoveredDaBlob],
    expected: &[(&TestEeUpdate, L1BlockCommitment)],
) {
    assert_eq!(blobs.len(), expected.len());
    for (blob, (update, completion_block)) in blobs.iter().zip(expected) {
        assert_eq!(blob.blob().update_seq_no, update.blob().update_seq_no);
        assert_eq!(blob.blob().spec_version, update.blob().spec_version);
        assert_eq!(blob.l1_ref().commit_txid(), update.commit_txid());
        assert_eq!(blob.l1_ref().completion_block(), *completion_block);
        assert_eq!(
            blob.blob()
                .encode_to_vec()
                .expect("recovered test blob encodes"),
            update
                .blob()
                .encode_to_vec()
                .expect("published test blob encodes"),
        );
    }
}

fn assert_one_recovered_blob(
    blobs: &[RecoveredDaBlob],
    update: &TestEeUpdate,
    completion_block: L1BlockCommitment,
) {
    assert_recovered_blobs(blobs, &[(update, completion_block)]);
}

/// Bitcoin data for the multi-tick recovery scenario.
struct MultiTickDaScenario {
    updates: [TestEeUpdate; 10],
    first_tick_completion_blocks: FirstTickCompletionBlocks,
}

/// Completion commitments for the updates available during the scenario's first tick.
struct FirstTickCompletionBlocks {
    update_0: L1BlockCommitment,
    update_1: L1BlockCommitment,
    updates_2_and_3: L1BlockCommitment,
    update_4: L1BlockCommitment,
    update_5: L1BlockCommitment,
    update_6: L1BlockCommitment,
    update_7: L1BlockCommitment,
    update_8: L1BlockCommitment,
}

/// Builds the ten-update Bitcoin layout for the multi-tick recovery scenario.
///
/// Updates 0 through 8 are complete. Update 9 has its commit and first reveal mined but remains
/// incomplete. The completed updates include same-block commit/reveal, cross-block reveals,
/// shared completion blocks, and reverse completion order.
fn build_multi_tick_da_scenario(fixture: &mut DaVerifierFixture) -> MultiTickDaScenario {
    let update_0 = fixture.produce_update(build_test_state_diff());
    let update_1 = fixture.produce_update(build_test_state_diff());
    let update_2 = fixture.produce_update(build_test_state_diff());
    let update_3 = fixture.produce_update(build_test_state_diff());
    let update_4 = fixture.produce_two_reveal_update(build_test_state_diff());
    let update_5 = fixture.produce_update(build_test_state_diff());
    let update_6 = fixture.produce_update(build_test_state_diff());
    let update_7 = fixture.produce_two_reveal_update(build_test_state_diff());
    let update_8 = fixture.produce_update(build_test_state_diff());
    let update_9 = fixture.produce_two_reveal_update(build_test_state_diff());
    let updates = [
        update_0, update_1, update_2, update_3, update_4, update_5, update_6, update_7, update_8,
        update_9,
    ];

    for update in &updates {
        fixture.publish_da(update);
    }
    for &index in &[0, 1, 2, 3, 5, 6, 8] {
        assert_eq!(
            updates[index].reveal_transactions().len(),
            1,
            "multi-tick update {index} must use one reveal"
        );
    }

    let update_0_completion = fixture.mine_block(
        MULTI_TICK_GENESIS_L1_HEIGHT,
        iter::once(updates[0].commit_transaction()).chain(updates[0].reveal_transactions()),
    );
    let update_1_completion = fixture.mine_block(
        MULTI_TICK_UPDATE_1_COMPLETION_HEIGHT,
        iter::once(updates[1].commit_transaction()).chain(updates[1].reveal_transactions()),
    );
    fixture.mine_block(
        MULTI_TICK_UPDATE_2_COMMIT_HEIGHT,
        [updates[2].commit_transaction()],
    );
    fixture.mine_block(
        MULTI_TICK_UPDATE_3_COMMIT_HEIGHT,
        [updates[3].commit_transaction()],
    );
    let updates_2_and_3_completion = fixture.mine_block(
        MULTI_TICK_UPDATES_2_AND_3_COMPLETION_HEIGHT,
        [
            &updates[2].reveal_transactions()[0],
            &updates[3].reveal_transactions()[0],
        ],
    );
    fixture.mine_block(
        MULTI_TICK_UPDATE_4_COMMIT_HEIGHT,
        [updates[4].commit_transaction()],
    );
    fixture.mine_block(
        MULTI_TICK_UPDATE_4_FIRST_REVEAL_HEIGHT,
        [&updates[4].reveal_transactions()[0]],
    );
    let update_4_completion = fixture.mine_block(
        MULTI_TICK_UPDATE_4_COMPLETION_HEIGHT,
        [&updates[4].reveal_transactions()[1]],
    );
    fixture.mine_block(
        MULTI_TICK_UPDATE_6_COMMIT_HEIGHT,
        [updates[6].commit_transaction()],
    );
    fixture.mine_block(
        MULTI_TICK_UPDATE_5_COMMIT_HEIGHT,
        [updates[5].commit_transaction()],
    );
    let update_6_completion = fixture.mine_block(
        MULTI_TICK_UPDATE_6_COMPLETION_HEIGHT,
        updates[6].reveal_transactions(),
    );
    let update_5_completion = fixture.mine_block(
        MULTI_TICK_UPDATE_5_COMPLETION_HEIGHT,
        updates[5].reveal_transactions(),
    );
    fixture.mine_block(
        MULTI_TICK_UPDATE_7_COMMIT_HEIGHT,
        [updates[7].commit_transaction()],
    );
    let update_7_completion = fixture.mine_block(
        MULTI_TICK_UPDATE_7_COMPLETION_HEIGHT,
        updates[7].reveal_transactions(),
    );
    let update_8_completion = fixture.mine_block(
        MULTI_TICK_UPDATE_8_COMPLETION_HEIGHT,
        iter::once(updates[8].commit_transaction()).chain(updates[8].reveal_transactions()),
    );
    fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMMIT_HEIGHT,
        [
            updates[9].commit_transaction(),
            &updates[9].reveal_transactions()[0],
        ],
    );

    MultiTickDaScenario {
        updates,
        first_tick_completion_blocks: FirstTickCompletionBlocks {
            update_0: update_0_completion,
            update_1: update_1_completion,
            updates_2_and_3: updates_2_and_3_completion,
            update_4: update_4_completion,
            update_5: update_5_completion,
            update_6: update_6_completion,
            update_7: update_7_completion,
            update_8: update_8_completion,
        },
    }
}

#[tokio::test]
async fn test_da_recovery_waits_below_reorg_safe_depth() {
    // 1. Configure a Bitcoin tip below the six-block reorg-safe depth.
    let fixture = DaVerifierFixture::new(AlpenParams::default(), 0, TEST_L1_REORG_SAFE_DEPTH)
        .with_bitcoin_tip_height(5);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick while no Bitcoin block is reorg-safe.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show the tick only reads the tip, leaves the cursor at genesis, and reports no safe tip.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_l1_height, 0);
    assert_eq!(status.reorg_safe_tip, None);
    assert_eq!(
        fixture.context_operations(),
        [ContextOperation::FetchBitcoinTip]
    );
}

#[tokio::test]
async fn test_tick_scans_successive_windows_until_reorg_safe_tip() {
    // 1. Configure a three-block scan window over eight reorg-safe blocks.
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_max_l1_scan_window_size(TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE)
    .with_bitcoin_tip_height(TEST_WINDOWED_BITCOIN_TIP);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick, which freezes the safe tip and scans through it.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show one tip fetch drives two full windows and one final window truncated at the
    // frozen safe tip.
    assert_eq!(state.next_l1_height(), TEST_WINDOWED_REORG_SAFE_TIP + 1);
    assert_eq!(
        fixture.context_operations(),
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_GENESIS_L1_HEIGHT + 2,
            },
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT + 3,
                end_height: TEST_GENESIS_L1_HEIGHT + 5,
            },
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT + 6,
                end_height: TEST_WINDOWED_REORG_SAFE_TIP,
            },
        ]
    );
}

#[tokio::test]
async fn test_da_recovery_preserves_extractor_state_across_windows() {
    // 1. Put an update's commit in the last block of one scan window and its reveal in the
    // first block of the next window.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_max_l1_scan_window_size(TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE)
    .with_bitcoin_tip_height(TEST_WINDOWED_BITCOIN_TIP);
    let update = fixture.produce_update(build_test_state_diff());
    fixture.publish_da(&update);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 2, [update.commit_transaction()]);
    let completion_block =
        fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 3, update.reveal_transactions());
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick across all three scan windows.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show the reveal completes the commit retained by the extractor from the prior window.
    let writes = fixture.recovered_da_write_attempts();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].outcome(), RecoveredDaWriteOutcome::Succeeded);
    assert_one_recovered_blob(writes[0].blobs(), &update, completion_block);
    assert_eq!(state.next_l1_height(), TEST_WINDOWED_REORG_SAFE_TIP + 1);
}

#[tokio::test]
async fn test_recoverable_failure_stops_l1_window_loop() {
    // 1. Fail the second block fetch in the first of three scan windows.
    let failed_height = TEST_GENESIS_L1_HEIGHT + 1;
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_max_l1_scan_window_size(TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE)
    .with_bitcoin_tip_height(TEST_WINDOWED_BITCOIN_TIP);
    fixture.fail_block_fetch_at(
        failed_height,
        FetchBlockError::RetriesExhausted {
            height: failed_height,
            max_retries: 3,
            source: ClientError::Timeout,
        },
    );
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick through the recoverable fetch failure.
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the L1 block fetch failure");

    // 3. Show the cursor retains progress through the preceding block and no later scan window
    // is requested.
    assert!(matches!(
        &error,
        DaVerifierError::DaRecovery(DaRecoveryError::FetchBlock(
            FetchBlockError::RetriesExhausted { height, .. }
        )) if *height == failed_height
    ));
    assert!(error.is_recoverable());
    assert_eq!(state.next_l1_height(), failed_height);
    assert_eq!(
        fixture.context_operations(),
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_GENESIS_L1_HEIGHT + 2,
            },
        ]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_da_recovery_persists_each_batch_at_its_completion_block() {
    // 1. Build the ten-update scenario with updates 0..=8 complete below the first safe tip.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        MULTI_TICK_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let scenario = build_multi_tick_da_scenario(&mut fixture);
    fixture.set_bitcoin_tip_height(MULTI_TICK_FIRST_TICK_TIP);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process tick 1 through the first reorg-safe tip.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show every completed update is written at its actual completion block and the cursor
    // advances beyond the entire inclusive range, including blocks without DA.
    assert_eq!(state.next_l1_height(), MULTI_TICK_FIRST_TICK_SAFE_TIP + 1);
    let tick_one_writes = fixture.recovered_da_write_attempts();
    let expected_tick_one_writes: [(L1BlockCommitment, &[usize]); 8] = [
        (scenario.first_tick_completion_blocks.update_0, &[0]),
        (scenario.first_tick_completion_blocks.update_1, &[1]),
        (
            scenario.first_tick_completion_blocks.updates_2_and_3,
            &[2, 3],
        ),
        (scenario.first_tick_completion_blocks.update_4, &[4]),
        (scenario.first_tick_completion_blocks.update_6, &[6]),
        (scenario.first_tick_completion_blocks.update_5, &[5]),
        (scenario.first_tick_completion_blocks.update_7, &[7]),
        (scenario.first_tick_completion_blocks.update_8, &[8]),
    ];
    assert_eq!(tick_one_writes.len(), expected_tick_one_writes.len());
    for (write_index, (attempt, (completion_block, batch_indices))) in tick_one_writes
        .iter()
        .zip(&expected_tick_one_writes)
        .enumerate()
    {
        assert_eq!(
            attempt.outcome(),
            RecoveredDaWriteOutcome::Succeeded,
            "tick-one write {write_index} failed"
        );
        let expected_blobs = batch_indices
            .iter()
            .map(|&batch_index| (&scenario.updates[batch_index], *completion_block))
            .collect::<Vec<_>>();
        assert_recovered_blobs(attempt.blobs(), &expected_blobs);
    }

    let mut expected_tick_one_operations = vec![
        ContextOperation::FetchBitcoinTip,
        ContextOperation::FetchL1BlockRange {
            start_height: MULTI_TICK_GENESIS_L1_HEIGHT,
            end_height: MULTI_TICK_FIRST_TICK_SAFE_TIP,
        },
    ];
    expected_tick_one_operations.extend(iter::repeat_n(
        ContextOperation::PutRecoveredDa,
        expected_tick_one_writes.len(),
    ));
    let tick_one_operations = fixture.context_operations();
    assert_eq!(tick_one_operations, expected_tick_one_operations);

    // 4. Complete update 9 above tick 1's safe tip, advance the tip, and process tick 2.
    let update_9_completion = fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT,
        [&scenario.updates[9].reveal_transactions()[1]],
    );
    fixture.set_bitcoin_tip_height(MULTI_TICK_SECOND_TICK_TIP);
    state.handle_tick().await.expect("tick processing succeeds");

    // 5. Show extractor state retained from tick 1 completes and persists update 9 exactly once.
    assert_eq!(state.next_l1_height(), MULTI_TICK_SECOND_TICK_SAFE_TIP + 1);
    let tick_two_writes = fixture.recovered_da_write_attempts();
    assert_eq!(tick_two_writes.len(), 9);
    assert_one_recovered_blob(
        tick_two_writes[8].blobs(),
        &scenario.updates[9],
        update_9_completion,
    );
    assert_eq!(fixture.recovered_da_blobs().len(), 10);

    // 6. Without adding Bitcoin data, process tick 3 after recovery has passed the safe tip.
    state.handle_tick().await.expect("tick processing succeeds");

    // 7. Show caught-up recovery still publishes the safe tip but requests no block range.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_l1_height, MULTI_TICK_SECOND_TICK_SAFE_TIP + 1);
    assert_eq!(status.reorg_safe_tip, Some(MULTI_TICK_SECOND_TICK_SAFE_TIP));
    let all_operations = fixture.context_operations();
    assert_eq!(
        &all_operations[tick_one_operations.len()..],
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: MULTI_TICK_FIRST_TICK_SAFE_TIP + 1,
                end_height: MULTI_TICK_SECOND_TICK_SAFE_TIP,
            },
            ContextOperation::PutRecoveredDa,
            ContextOperation::FetchBitcoinTip,
        ]
    );
}

#[tokio::test]
async fn test_da_recovery_rejects_non_contiguous_blocks() {
    // 1. Override the requested range with blocks at genesis and the safe tip, omitting the
    // required middle height.
    let blocks = vec![
        Ok(build_l1_block_data(TEST_GENESIS_L1_HEIGHT, 0, Vec::new())),
        Ok(build_l1_block_data(TEST_REORG_SAFE_TIP, 0, Vec::new())),
    ];
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_REORG_SAFE_TIP + TEST_L1_REORG_SAFE_DEPTH);
    fixture.override_next_l1_block_range(Ok(blocks));
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick against the non-contiguous stream.
    let error = state
        .handle_tick()
        .await
        .expect_err("skipped L1 height must fail");

    // 3. Show recovery reports the exact gap and leaves the cursor at the missing height.
    assert!(matches!(
        error,
        DaVerifierError::DaRecovery(DaRecoveryError::NonContiguousBlocks {
            expected,
            actual,
        }) if expected == TEST_GENESIS_L1_HEIGHT + 1 && actual == TEST_REORG_SAFE_TIP
    ));
    assert_eq!(state.next_l1_height(), TEST_GENESIS_L1_HEIGHT + 1);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_da_recovery_rejects_incomplete_block_range() {
    // 1. Return only the first block from a requested three-block range.
    let blocks = vec![Ok(build_l1_block_data(
        TEST_GENESIS_L1_HEIGHT,
        0,
        Vec::new(),
    ))];
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_REORG_SAFE_TIP + TEST_L1_REORG_SAFE_DEPTH);
    fixture.override_next_l1_block_range(Ok(blocks));
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick against the truncated stream.
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the incomplete L1 range");

    // 3. Show recovery retains the first block's progress but rejects the incomplete range.
    assert!(matches!(
        &error,
        DaVerifierError::DaRecovery(DaRecoveryError::IncompleteBlockRange {
            next_height,
            end_height,
        }) if *next_height == TEST_GENESIS_L1_HEIGHT + 1 && *end_height == TEST_REORG_SAFE_TIP
    ));
    assert!(!error.is_recoverable());
    assert_eq!(state.next_l1_height(), TEST_GENESIS_L1_HEIGHT + 1);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_da_recovery_records_progress_before_fetch_failure() {
    // 1. Let two blocks arrive, then fail the final block fetch in the requested range.
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_REORG_SAFE_TIP + TEST_L1_REORG_SAFE_DEPTH);
    fixture.fail_block_fetch_at(
        TEST_REORG_SAFE_TIP,
        FetchBlockError::RetriesExhausted {
            height: TEST_REORG_SAFE_TIP,
            max_retries: 3,
            source: ClientError::Timeout,
        },
    );
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick through the mid-stream fetch failure.
    let error = state
        .handle_tick()
        .await
        .expect_err("midstream fetch failure must be returned");

    // 3. Show the exact fetch error is returned after retaining progress through prior blocks.
    assert!(matches!(
        error,
        DaVerifierError::DaRecovery(DaRecoveryError::FetchBlock(
            FetchBlockError::RetriesExhausted {
                height: TEST_REORG_SAFE_TIP,
                ..
            }
        ))
    ));
    assert_eq!(state.next_l1_height(), TEST_REORG_SAFE_TIP);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_recoverable_da_persistence_failure_retries_pending_write() {
    // 1. Confirm one update across two blocks and fail its first candidate-store write.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);
    let update = fixture.produce_update(build_test_state_diff());
    fixture.publish_da(&update);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [update.commit_transaction()]);
    let completion_block = fixture.mine_block(
        TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT,
        update.reveal_transactions(),
    );
    fixture.fail_next_recovered_da_write(RecoveredDaDbError::WorkerCancelled);
    let mut service_state = fixture.build_verifier_service_state();

    // 2. Process the first tick and show the failed write leaves the cursor at the unpersisted
    // completion height.
    let error = service_state
        .handle_tick()
        .await
        .expect_err("the first persistence attempt must fail");
    assert!(matches!(
        error,
        DaVerifierError::DaRecovery(DaRecoveryError::Database(
            RecoveredDaDbError::WorkerCancelled
        ))
    ));
    assert_eq!(
        service_state.next_l1_height(),
        TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT
    );

    // 3. Process another tick, which must flush the pending write before fetching more blocks.
    service_state
        .handle_tick()
        .await
        .expect("tick processing succeeds after retrying the pending write");

    // 4. Show the same blob was retried successfully before the next tip fetch, with no second
    // block-range request.
    assert_eq!(
        service_state.next_l1_height(),
        TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT + 1
    );
    assert_eq!(
        fixture.context_operations(),
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT,
            },
            ContextOperation::PutRecoveredDa,
            ContextOperation::PutRecoveredDa,
            ContextOperation::FetchBitcoinTip,
        ]
    );

    let write_attempts = fixture.recovered_da_write_attempts();
    assert_eq!(write_attempts.len(), 2);
    for attempt in &write_attempts {
        assert_one_recovered_blob(attempt.blobs(), &update, completion_block);
    }
    assert_eq!(
        write_attempts
            .iter()
            .map(RecoveredDaWriteAttempt::outcome)
            .collect::<Vec<_>>(),
        [
            RecoveredDaWriteOutcome::Failed,
            RecoveredDaWriteOutcome::Succeeded,
        ]
    );
    assert_one_recovered_blob(&fixture.recovered_da_blobs(), &update, completion_block);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_fatal_da_persistence_failure_retains_pending_write() {
    // 1. Confirm one update across two blocks and make its first write fail fatally.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let update = fixture.produce_update(build_test_state_diff());
    fixture.publish_da(&update);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [update.commit_transaction()]);
    let completion_block = fixture.mine_block(
        TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT,
        update.reveal_transactions(),
    );
    fixture.set_bitcoin_tip_height(TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);
    fixture.fail_next_recovered_da_write(RecoveredDaDbError::WorkerPanicked(
        "worker panic".to_owned(),
    ));
    let mut state = fixture.build_verifier_service_state();

    // 2. Process the first tick and show the fatal write leaves the cursor at the unpersisted
    // completion height.
    let error = state
        .handle_tick()
        .await
        .expect_err("fatal persistence failure must be returned");
    assert!(matches!(
        error,
        DaVerifierError::DaRecovery(DaRecoveryError::Database(
            RecoveredDaDbError::WorkerPanicked(ref message)
        )) if message == "worker panic"
    ));
    assert_eq!(state.next_l1_height(), TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT);

    // 3. Process a second in-memory tick to show the driver retained the unpersisted blob.
    // Production terminates on the fatal error, so this retry is only a state-level probe.
    state
        .handle_tick()
        .await
        .expect("tick processing succeeds after retrying the retained write");

    // 4. Show the retry persists the same blob before the next tip fetch, with no second
    // block-range request.
    assert_eq!(
        state.next_l1_height(),
        TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT + 1
    );
    assert_eq!(
        fixture.context_operations(),
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT,
            },
            ContextOperation::PutRecoveredDa,
            ContextOperation::PutRecoveredDa,
            ContextOperation::FetchBitcoinTip,
        ]
    );
    let write_attempts = fixture.recovered_da_write_attempts();
    assert_eq!(write_attempts.len(), 2);
    for attempt in &write_attempts {
        assert_one_recovered_blob(attempt.blobs(), &update, completion_block);
    }
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_da_recovery_driver_rejects_blocks_while_write_pending() {
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let update = fixture.produce_update(build_test_state_diff());
    fixture.publish_da(&update);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [update.commit_transaction()]);
    fixture.mine_block(
        TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT,
        update.reveal_transactions(),
    );
    fixture.fail_next_recovered_da_write(RecoveredDaDbError::WorkerCancelled);
    let mut driver = fixture.build_da_recovery_driver();

    fixture
        .recover_da_through(&mut driver, TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT)
        .await
        .expect_err("failed persistence must leave recovered DA pending");

    // The failed write leaves the blob pending and the candidate store empty.
    let write_attempts = fixture.recovered_da_write_attempts();
    assert_eq!(write_attempts.len(), 1);
    assert_eq!(write_attempts[0].outcome(), RecoveredDaWriteOutcome::Failed);
    assert!(fixture.recovered_da_blobs().is_empty());

    let payload = AssertUnwindSafe(
        fixture.recover_da_through(&mut driver, TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT),
    )
    .catch_unwind()
    .await
    .expect_err("recovery with pending DA must panic");

    assert!(panic_message(payload.as_ref()).contains("pending recovered DA must be persisted"));
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_service_continues_on_recoverable_error() {
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    fixture.fail_next_bitcoin_tip(FetchBitcoinTipError::Rpc(ClientError::Timeout));
    let mut state = fixture.build_verifier_service_state();

    let response = DaVerifierService::<MockDaVerifierContext>::process_input(&mut state, ())
        .await
        .expect("recoverable error must not stop the service");
    assert_eq!(response, Response::Continue);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_service_propagates_fatal_error() {
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    fixture.fail_next_bitcoin_tip(FetchBitcoinTipError::Rpc(ClientError::MissingUserPassword));
    let mut state = fixture.build_verifier_service_state();

    let error = DaVerifierService::<MockDaVerifierContext>::process_input(&mut state, ())
        .await
        .expect_err("fatal error must stop the service");
    assert!(matches!(
        error.downcast_ref::<DaVerifierError>(),
        Some(DaVerifierError::FetchBitcoinTip(FetchBitcoinTipError::Rpc(
            ClientError::MissingUserPassword
        )))
    ));
    fixture.assert_injected_behavior_consumed();
}
