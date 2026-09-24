use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    io, iter,
    num::NonZeroU32,
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use alpen_acct_state::{
    compute_ee_account_inner_root, EeAccountReconstructionError, EeAccountUpdateManifest,
};
use alpen_acct_types::{EeAccountState, UpdateExtraData};
use alpen_batch_replay::BatchReplaySnapshot;
use alpen_da_l1_extraction::{
    DaExtractor, DaL1Ref, FetchBlockError, FetchRangeError, L1BlockData, RecoveredDaBlob,
};
use alpen_da_provider::prepare_da_chunks;
use alpen_da_types::{da_blob_version, decode_da_blob, DaBlob, EvmHeaderSummary};
use alpen_database::RecoveredDaDbError;
use alpen_genesis::build_genesis_ee_account_state;
use alpen_l1_reconstruction::{BatchSequenceError, L1ReconstructionError};
use alpen_params::{AlpenParams, AlpenSpecId};
use alpen_reth_statediff::{test_utils as statediff_fixtures, BatchStateDiff};
use async_trait::async_trait;
use bitcoin::{
    absolute::LockTime,
    block::{self, Header},
    hashes::Hash as _,
    pow::CompactTarget,
    script::Builder,
    secp256k1::XOnlyPublicKey,
    transaction, Amount, Block, BlockHash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Txid, Witness,
};
use bitcoind_async_client::error::ClientError;
use futures::{stream, FutureExt, Stream};
use strata_acct_types::{AccountId, Hash};
use strata_identifiers::{Buf32, L1BlockCommitment, L1BlockId, L1Height};
use strata_l1_envelope_fmt::{test_utils as commit_reveal_fixtures, MAX_ENVELOPE_PAYLOAD_SIZE};
use strata_service::{AsyncService, Response, Service};
use strata_snark_acct_types::Seqno;

use crate::{
    account_state::{
        AccountStateVerificationError, OLAccountUpdate, OLAccountUpdateError,
        OLAccountUpdateSource, VerifiedAccountState, VerifyAccountStateError,
    },
    bitcoin::{CheckL1BlockError, FetchBitcoinTipError, FetchCommitBlockError},
    context::DaVerifierContext,
    da_extraction::{DaRecoveryDriver, DaRecoveryError},
    evm_state::reconstruct_evm_state,
    service::DaVerifierService,
    snapshot::{
        ReconstructionSnapshot, SnapshotDeleteError, SnapshotLoadError, SnapshotSaveError,
        SnapshotValidationError,
    },
    state::{DaRecoveryState, DaVerificationState, DaVerifierError, DaVerifierServiceState},
};

type L1BlockResults = Vec<Result<L1BlockData, FetchBlockError>>;

// Defaults shared by the verifier test fixture.
const SEQUENCER_KEY_SEED: u8 = 7;
const TEST_L1_REORG_SAFE_DEPTH: u32 = 6;
const TEST_MAX_L1_SCAN_WINDOW_SIZE: NonZeroU32 = NonZeroU32::new(500).expect("500 is nonzero");

// Short-chain recovery and verification scenarios.
const TEST_GENESIS_L1_HEIGHT: L1Height = 10;
/// Completion height when a commit at genesis has every reveal in the next block.
const TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT: L1Height = TEST_GENESIS_L1_HEIGHT + 1;
/// Last height a scan may reach, leaving a three-block window from genesis.
const TEST_REORG_SAFE_TIP: L1Height = TEST_GENESIS_L1_HEIGHT + 2;
/// Bitcoin tip whose reorg-safe tip is verifier genesis, so recovery scans one block.
const TEST_ONE_BLOCK_SCAN_BITCOIN_TIP: L1Height = TEST_GENESIS_L1_HEIGHT + TEST_L1_REORG_SAFE_DEPTH;

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
    account_update: OLAccountUpdate,
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

    fn account_update(&self) -> &OLAccountUpdate {
        &self.account_update
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
    replay_snapshot: Option<BatchReplaySnapshot>,
    account_state: EeAccountState,
}

impl TestEeSequencer {
    fn new(key_seed: u8, params: &AlpenParams) -> Self {
        Self {
            key_seed,
            next_update_seq_no: 0,
            replay_snapshot: None,
            account_state: build_genesis_ee_account_state(params),
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

        let account_update = self.build_matching_account_update(params, &blob, commit_txid);

        TestEeUpdate {
            blob,
            account_update,
            commit_txid,
            commit: transactions.commit,
            reveals: transactions.reveals,
        }
    }

    fn build_matching_account_update(
        &mut self,
        params: &AlpenParams,
        blob: &DaBlob,
        commit_txid: Txid,
    ) -> OLAccountUpdate {
        let recovered_blob = RecoveredDaBlob::new(
            DaL1Ref::new(commit_txid, L1BlockCommitment::new(0, Default::default())),
            blob.clone(),
        );
        let reconstruction =
            reconstruct_evm_state(params, self.replay_snapshot.take(), vec![recovered_blob])
                .expect("test update should reconstruct")
                .expect("test update should produce replay output");
        let replay_outcome = reconstruction.batch_replay_outcome();
        let applied_root = replay_outcome
            .applied_roots()
            .first()
            .expect("one test update produces one applied root");
        let update_seq_no = applied_root.update_seq_no();
        let tip_seed = u8::try_from(update_seq_no.inner() + 1)
            .expect("test sequence should fit in an execution tip seed");
        let new_tip_blkid = Hash::new([tip_seed; 32]);
        let evm_state_root = Hash::new(applied_root.post_state_root().0);
        self.account_state.set_last_exec_blkid(new_tip_blkid);
        self.account_state.set_last_exec_state_root(evm_state_root);
        let manifest = EeAccountUpdateManifest::try_new(
            update_seq_no,
            compute_ee_account_inner_root(&self.account_state),
            0,
            0,
            UpdateExtraData::new(new_tip_blkid, evm_state_root, 0, 0),
        )
        .expect("test manifest should have valid inbox cursors");

        let last_block_num = replay_outcome.applied_range().last_block_num();
        let state_root = replay_outcome.final_state_root();
        let replay_outcome = reconstruction.into_batch_replay_outcome();
        self.replay_snapshot = Some(
            BatchReplaySnapshot::try_new(
                Seqno::new(
                    update_seq_no
                        .inner()
                        .checked_add(1)
                        .expect("test update sequence should advance"),
                ),
                last_block_num,
                state_root,
                replay_outcome.into_final_state(),
            )
            .expect("test replay snapshot should be valid"),
        );

        OLAccountUpdate::new(manifest, Vec::new())
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
fn build_test_state_diff(seed: u8) -> BatchStateDiff {
    let mut block = statediff_fixtures::block_diff();
    let account = statediff_fixtures::addr(seed);
    let code_hash = statediff_fixtures::hash(seed.wrapping_add(64));
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

    fn fetch_commit_block(
        &self,
        commit_txid: Txid,
    ) -> Result<L1BlockCommitment, FetchCommitBlockError> {
        self.confirmed_transactions
            .get(&commit_txid)
            .copied()
            .ok_or(FetchCommitBlockError::Unconfirmed { txid: commit_txid })
    }

    fn is_l1_block_canonical(&self, expected: L1BlockCommitment) -> bool {
        self.blocks
            .get(&expected.height())
            .is_some_and(|block| block.block_id() == *expected.blkid())
    }

    fn replace_block(&mut self, height: L1Height, block_discriminator: u32) -> L1BlockCommitment {
        let transactions = self
            .blocks
            .get(&height)
            .unwrap_or_else(|| panic!("test block {height} is not mined"))
            .block()
            .txdata
            .iter()
            .skip(1)
            .cloned()
            .collect::<Vec<_>>();
        let replacement = build_l1_block_data(height, block_discriminator, transactions);
        let commitment = L1BlockCommitment::new(height, replacement.block_id());
        for transaction in &replacement.block().txdata {
            if let Some(confirmed_in) = self
                .confirmed_transactions
                .get_mut(&transaction.compute_txid())
            {
                *confirmed_in = commitment;
            }
        }
        self.blocks.insert(height, replacement);
        commitment
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
        self.blobs.iter().map(decode_stored_recovered_da).collect()
    }

    fn get_contiguous_from(
        &self,
        first_update_seq_no: u64,
        recovered_l1_frontier: L1Height,
    ) -> Vec<RecoveredDaBlob> {
        let mut expected_update_seq_no = first_update_seq_no;
        let mut found_eligible_candidate = false;
        let mut recovered_blobs = Vec::new();

        for (key @ (update_seq_no, _), stored) in self
            .blobs
            .iter()
            .filter(|((sequence, _), _)| *sequence >= first_update_seq_no)
        {
            if *update_seq_no > expected_update_seq_no {
                if !found_eligible_candidate {
                    break;
                }
                let Some(next_expected_update_seq_no) = expected_update_seq_no.checked_add(1)
                else {
                    break;
                };
                expected_update_seq_no = next_expected_update_seq_no;
                found_eligible_candidate = false;
                if *update_seq_no != expected_update_seq_no {
                    break;
                }
            }

            if stored.completion_block.height() <= recovered_l1_frontier {
                found_eligible_candidate = true;
                recovered_blobs.push(decode_stored_recovered_da((key, stored)));
            }
        }

        recovered_blobs
    }

    fn prune_before(&mut self, update_seq_no: u64) {
        self.blobs
            .retain(|(sequence, _), _| *sequence >= update_seq_no);
    }

    fn clear(&mut self) {
        self.blobs.clear();
    }
}

fn decode_stored_recovered_da(
    (&(_, commit_txid), stored): (&RecoveredDaCandidateKey, &StoredRecoveredDaValue),
) -> RecoveredDaBlob {
    let blob = decode_da_blob(&stored.payload_bytes, stored.spec_version)
        .expect("mock store contains only encoded DA blobs");
    RecoveredDaBlob::new(DaL1Ref::new(commit_txid, stored.completion_block), blob)
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

struct MockOLAccountUpdateSource {
    account_id: AccountId,
    updates: BTreeMap<u64, OLAccountUpdate>,
}

impl MockOLAccountUpdateSource {
    fn new(account_id: AccountId) -> Self {
        Self {
            account_id,
            updates: BTreeMap::new(),
        }
    }

    fn process_sau(&mut self, update: &TestEeUpdate) {
        let sequence = *update.account_update().manifest().update_seq_no().inner();
        assert_eq!(
            sequence,
            self.updates.len() as u64,
            "mock OL account processes SAUs in sequence"
        );
        assert!(
            self.updates
                .insert(sequence, update.account_update().clone())
                .is_none(),
            "mock OL account update {sequence} was already processed"
        );
    }

    fn next_update_seq_no(&self, account_id: AccountId) -> Seqno {
        assert_eq!(account_id, self.account_id, "unexpected OL account id");
        Seqno::new(self.updates.len() as u64)
    }

    fn fetch_update(
        &self,
        account_id: AccountId,
        update_seq_no: Seqno,
    ) -> Result<OLAccountUpdate, OLAccountUpdateError> {
        assert_eq!(account_id, self.account_id, "unexpected OL account id");
        self.updates
            .get(update_seq_no.inner())
            .cloned()
            .ok_or_else(|| {
                OLAccountUpdateError::new(
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "OL account update {} is not yet available",
                            update_seq_no.inner()
                        ),
                    ),
                    true,
                )
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredReconstructionSnapshot {
    replay_snapshot: BatchReplaySnapshot,
    verified_account_state: VerifiedAccountState,
    resume_l1_block: L1BlockCommitment,
    completion_block: L1BlockCommitment,
}

#[derive(Default)]
struct InMemorySnapshotStore {
    snapshot: Option<StoredReconstructionSnapshot>,
}

impl InMemorySnapshotStore {
    fn save(&mut self, snapshot: StoredReconstructionSnapshot) {
        self.snapshot = Some(snapshot);
    }

    fn snapshot(&self) -> Option<&StoredReconstructionSnapshot> {
        self.snapshot.as_ref()
    }

    fn load(&self) -> Option<ReconstructionSnapshot> {
        self.snapshot.as_ref().map(|snapshot| {
            ReconstructionSnapshot::try_new(
                snapshot.replay_snapshot.clone(),
                snapshot.verified_account_state.clone(),
                snapshot.resume_l1_block,
                snapshot.completion_block,
            )
            .expect("the in-memory snapshot store only contains validated state")
        })
    }

    fn delete(&mut self) {
        self.snapshot = None;
    }
}

#[derive(Default)]
struct InjectedContextBehavior {
    bitcoin_tip_failures: VecDeque<FetchBitcoinTipError>,
    block_fetch_failures: BTreeMap<L1Height, FetchBlockError>,
    l1_block_range_overrides: VecDeque<Result<L1BlockResults, FetchRangeError>>,
    recovered_da_write_failures: VecDeque<RecoveredDaDbError>,
    recovered_da_read_failures: VecDeque<RecoveredDaDbError>,
    recovered_da_prune_failures: VecDeque<RecoveredDaDbError>,
    recovered_da_clear_failures: VecDeque<RecoveredDaDbError>,
    finalized_frontier_results: VecDeque<Result<Seqno, OLAccountUpdateError>>,
    ol_update_failures: BTreeMap<u64, OLAccountUpdateError>,
    commit_block_lookup_failures: VecDeque<FetchCommitBlockError>,
    l1_block_canonicality_failures: VecDeque<CheckL1BlockError>,
    snapshot_load_failures: VecDeque<SnapshotLoadError>,
    snapshot_delete_failures: VecDeque<SnapshotDeleteError>,
    snapshot_save_failures: VecDeque<SnapshotSaveError>,
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

    fn fail_next_recovered_da_read(&mut self, error: RecoveredDaDbError) {
        self.recovered_da_read_failures.push_back(error);
    }

    fn take_next_recovered_da_read_failure(&mut self) -> Option<RecoveredDaDbError> {
        self.recovered_da_read_failures.pop_front()
    }

    fn fail_next_recovered_da_prune(&mut self, error: RecoveredDaDbError) {
        self.recovered_da_prune_failures.push_back(error);
    }

    fn take_next_recovered_da_prune_failure(&mut self) -> Option<RecoveredDaDbError> {
        self.recovered_da_prune_failures.pop_front()
    }

    fn fail_next_recovered_da_clear(&mut self, error: RecoveredDaDbError) {
        self.recovered_da_clear_failures.push_back(error);
    }

    fn take_next_recovered_da_clear_failure(&mut self) -> Option<RecoveredDaDbError> {
        self.recovered_da_clear_failures.pop_front()
    }

    fn fail_ol_update_at(&mut self, update_seq_no: Seqno, error: OLAccountUpdateError) {
        assert!(
            self.ol_update_failures
                .insert(*update_seq_no.inner(), error)
                .is_none(),
            "OL update failure already injected at sequence {}",
            update_seq_no.inner()
        );
    }

    fn override_next_finalized_frontier(&mut self, result: Result<Seqno, OLAccountUpdateError>) {
        self.finalized_frontier_results.push_back(result);
    }

    fn take_next_finalized_frontier_override(
        &mut self,
    ) -> Option<Result<Seqno, OLAccountUpdateError>> {
        self.finalized_frontier_results.pop_front()
    }

    fn take_ol_update_failure(&mut self, update_seq_no: Seqno) -> Option<OLAccountUpdateError> {
        self.ol_update_failures.remove(update_seq_no.inner())
    }

    fn fail_next_commit_block_lookup(&mut self, error: FetchCommitBlockError) {
        self.commit_block_lookup_failures.push_back(error);
    }

    fn take_next_commit_block_lookup_failure(&mut self) -> Option<FetchCommitBlockError> {
        self.commit_block_lookup_failures.pop_front()
    }

    fn fail_next_l1_block_canonicality_check(&mut self, error: CheckL1BlockError) {
        self.l1_block_canonicality_failures.push_back(error);
    }

    fn take_next_l1_block_canonicality_failure(&mut self) -> Option<CheckL1BlockError> {
        self.l1_block_canonicality_failures.pop_front()
    }

    fn fail_next_snapshot_load(&mut self, error: SnapshotLoadError) {
        self.snapshot_load_failures.push_back(error);
    }

    fn take_next_snapshot_load_failure(&mut self) -> Option<SnapshotLoadError> {
        self.snapshot_load_failures.pop_front()
    }

    fn fail_next_snapshot_delete(&mut self, error: SnapshotDeleteError) {
        self.snapshot_delete_failures.push_back(error);
    }

    fn take_next_snapshot_delete_failure(&mut self) -> Option<SnapshotDeleteError> {
        self.snapshot_delete_failures.pop_front()
    }

    fn fail_next_snapshot_save(&mut self, error: SnapshotSaveError) {
        self.snapshot_save_failures.push_back(error);
    }

    fn take_next_snapshot_save_failure(&mut self) -> Option<SnapshotSaveError> {
        self.snapshot_save_failures.pop_front()
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
        assert!(
            self.recovered_da_read_failures.is_empty(),
            "unconsumed recovered-DA read failures"
        );
        assert!(
            self.recovered_da_prune_failures.is_empty(),
            "unconsumed recovered-DA prune failures"
        );
        assert!(
            self.recovered_da_clear_failures.is_empty(),
            "unconsumed recovered-DA clear failures"
        );
        assert!(
            self.finalized_frontier_results.is_empty(),
            "unconsumed finalized OL sequence results"
        );
        assert!(
            self.ol_update_failures.is_empty(),
            "unconsumed OL update failures"
        );
        assert!(
            self.commit_block_lookup_failures.is_empty(),
            "unconsumed commit-block lookup failures"
        );
        assert!(
            self.l1_block_canonicality_failures.is_empty(),
            "unconsumed L1 block canonicality failures"
        );
        assert!(
            self.snapshot_load_failures.is_empty(),
            "unconsumed snapshot-load failures"
        );
        assert!(
            self.snapshot_delete_failures.is_empty(),
            "unconsumed snapshot-delete failures"
        );
        assert!(
            self.snapshot_save_failures.is_empty(),
            "unconsumed snapshot-save failures"
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextOperation {
    LoadSnapshot,
    DeleteSnapshot,
    CheckL1BlockCanonicality {
        commitment: L1BlockCommitment,
    },
    FetchBitcoinTip,
    FetchL1BlockRange {
        start_height: L1Height,
        end_height: L1Height,
    },
    PutRecoveredDa,
    GetContiguousRecoveredDa {
        first_update_seq_no: u64,
        recovered_l1_frontier: L1Height,
    },
    PruneRecoveredDa {
        update_seq_no: u64,
    },
    ClearRecoveredDa,
    FetchFinalizedAccountState {
        account_id: AccountId,
    },
    FetchAccountUpdate {
        account_id: AccountId,
        update_seq_no: Seqno,
    },
    FetchCommitBlock {
        commit_txid: Txid,
    },
    SaveSnapshot {
        resume_l1_block: L1BlockCommitment,
        completion_block: L1BlockCommitment,
    },
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotSaveOutcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug)]
struct SnapshotSaveAttempt {
    snapshot: StoredReconstructionSnapshot,
    outcome: SnapshotSaveOutcome,
}

impl RecoveredDaWriteAttempt {
    fn blobs(&self) -> &[RecoveredDaBlob] {
        &self.blobs
    }

    fn outcome(&self) -> RecoveredDaWriteOutcome {
        self.outcome
    }
}

impl SnapshotSaveAttempt {
    fn snapshot(&self) -> &StoredReconstructionSnapshot {
        &self.snapshot
    }

    fn outcome(&self) -> SnapshotSaveOutcome {
        self.outcome
    }
}

#[derive(Clone, Debug)]
enum ContextEvent {
    LoadSnapshot,
    DeleteSnapshot,
    CheckL1BlockCanonicality {
        commitment: L1BlockCommitment,
    },
    FetchBitcoinTip,
    FetchL1BlockRange {
        start_height: L1Height,
        end_height: L1Height,
    },
    PutRecoveredDa(RecoveredDaWriteAttempt),
    GetContiguousRecoveredDa {
        first_update_seq_no: u64,
        recovered_l1_frontier: L1Height,
    },
    PruneRecoveredDa {
        update_seq_no: u64,
    },
    ClearRecoveredDa,
    FetchFinalizedAccountState {
        account_id: AccountId,
    },
    FetchAccountUpdate {
        account_id: AccountId,
        update_seq_no: Seqno,
    },
    FetchCommitBlock {
        commit_txid: Txid,
    },
    SaveSnapshot(Box<SnapshotSaveAttempt>),
}

impl ContextEvent {
    fn operation(&self) -> ContextOperation {
        match self {
            Self::LoadSnapshot => ContextOperation::LoadSnapshot,
            Self::DeleteSnapshot => ContextOperation::DeleteSnapshot,
            Self::CheckL1BlockCanonicality { commitment } => {
                ContextOperation::CheckL1BlockCanonicality {
                    commitment: *commitment,
                }
            }
            Self::FetchBitcoinTip => ContextOperation::FetchBitcoinTip,
            Self::FetchL1BlockRange {
                start_height,
                end_height,
            } => ContextOperation::FetchL1BlockRange {
                start_height: *start_height,
                end_height: *end_height,
            },
            Self::PutRecoveredDa(_) => ContextOperation::PutRecoveredDa,
            Self::GetContiguousRecoveredDa {
                first_update_seq_no,
                recovered_l1_frontier,
            } => ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: *first_update_seq_no,
                recovered_l1_frontier: *recovered_l1_frontier,
            },
            Self::PruneRecoveredDa { update_seq_no } => ContextOperation::PruneRecoveredDa {
                update_seq_no: *update_seq_no,
            },
            Self::ClearRecoveredDa => ContextOperation::ClearRecoveredDa,
            Self::FetchFinalizedAccountState { account_id } => {
                ContextOperation::FetchFinalizedAccountState {
                    account_id: *account_id,
                }
            }
            Self::FetchAccountUpdate {
                account_id,
                update_seq_no,
            } => ContextOperation::FetchAccountUpdate {
                account_id: *account_id,
                update_seq_no: *update_seq_no,
            },
            Self::FetchCommitBlock { commit_txid } => ContextOperation::FetchCommitBlock {
                commit_txid: *commit_txid,
            },
            Self::SaveSnapshot(attempt) => ContextOperation::SaveSnapshot {
                resume_l1_block: attempt.snapshot.resume_l1_block,
                completion_block: attempt.snapshot.completion_block,
            },
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
                ContextEvent::LoadSnapshot
                | ContextEvent::DeleteSnapshot
                | ContextEvent::CheckL1BlockCanonicality { .. }
                | ContextEvent::FetchBitcoinTip
                | ContextEvent::FetchL1BlockRange { .. }
                | ContextEvent::GetContiguousRecoveredDa { .. }
                | ContextEvent::FetchFinalizedAccountState { .. }
                | ContextEvent::FetchAccountUpdate { .. }
                | ContextEvent::FetchCommitBlock { .. }
                | ContextEvent::SaveSnapshot(_)
                | ContextEvent::PruneRecoveredDa { .. }
                | ContextEvent::ClearRecoveredDa => None,
            })
            .collect()
    }

    fn recovery_operations(&self) -> Vec<ContextOperation> {
        self.0
            .iter()
            .filter_map(|event| match event {
                ContextEvent::FetchBitcoinTip
                | ContextEvent::FetchL1BlockRange { .. }
                | ContextEvent::PutRecoveredDa(_) => Some(event.operation()),
                ContextEvent::LoadSnapshot
                | ContextEvent::DeleteSnapshot
                | ContextEvent::CheckL1BlockCanonicality { .. }
                | ContextEvent::GetContiguousRecoveredDa { .. }
                | ContextEvent::FetchFinalizedAccountState { .. }
                | ContextEvent::FetchAccountUpdate { .. }
                | ContextEvent::FetchCommitBlock { .. }
                | ContextEvent::SaveSnapshot(_)
                | ContextEvent::PruneRecoveredDa { .. }
                | ContextEvent::ClearRecoveredDa => None,
            })
            .collect()
    }

    fn recovered_da_read_requests(&self) -> Vec<(u64, L1Height)> {
        self.0
            .iter()
            .filter_map(|event| match event {
                ContextEvent::GetContiguousRecoveredDa {
                    first_update_seq_no,
                    recovered_l1_frontier,
                } => Some((*first_update_seq_no, *recovered_l1_frontier)),
                _ => None,
            })
            .collect()
    }

    fn account_update_requests(&self) -> Vec<(AccountId, Seqno)> {
        self.0
            .iter()
            .filter_map(|event| match event {
                ContextEvent::FetchAccountUpdate {
                    account_id,
                    update_seq_no,
                } => Some((*account_id, *update_seq_no)),
                _ => None,
            })
            .collect()
    }

    fn commit_block_lookups(&self) -> Vec<Txid> {
        self.0
            .iter()
            .filter_map(|event| match event {
                ContextEvent::FetchCommitBlock { commit_txid } => Some(*commit_txid),
                _ => None,
            })
            .collect()
    }

    fn snapshot_save_attempts(&self) -> Vec<SnapshotSaveAttempt> {
        self.0
            .iter()
            .filter_map(|event| match event {
                ContextEvent::SaveSnapshot(attempt) => Some(attempt.as_ref().clone()),
                _ => None,
            })
            .collect()
    }

    fn snapshot_load_count(&self) -> usize {
        self.0
            .iter()
            .filter(|event| matches!(event, ContextEvent::LoadSnapshot))
            .count()
    }

    fn l1_block_canonicality_checks(&self) -> Vec<L1BlockCommitment> {
        self.0
            .iter()
            .filter_map(|event| match event {
                ContextEvent::CheckL1BlockCanonicality { commitment } => Some(*commitment),
                _ => None,
            })
            .collect()
    }
}

struct MockDaVerifierContextState {
    bitcoin: MockBitcoinChain,
    recovered_da: InMemoryRecoveredDaStore,
    account_update_source: MockOLAccountUpdateSource,
    snapshots: InMemorySnapshotStore,
    injected_behavior: InjectedContextBehavior,
    // A unified log preserves ordering across all external capabilities.
    events: ContextEventLog,
}

impl MockDaVerifierContextState {
    fn new(account_id: AccountId) -> Self {
        Self {
            bitcoin: MockBitcoinChain::default(),
            recovered_da: InMemoryRecoveredDaStore::default(),
            account_update_source: MockOLAccountUpdateSource::new(account_id),
            snapshots: InMemorySnapshotStore::default(),
            injected_behavior: InjectedContextBehavior::default(),
            events: ContextEventLog::default(),
        }
    }
}

struct MockDaVerifierContext {
    state: Mutex<MockDaVerifierContextState>,
}

impl MockDaVerifierContext {
    fn new(account_id: AccountId) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MockDaVerifierContextState::new(account_id)),
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

    async fn get_contiguous_recovered_da(
        &self,
        first_update_seq_no: u64,
        recovered_l1_frontier: L1Height,
    ) -> Result<Vec<RecoveredDaBlob>, RecoveredDaDbError> {
        let mut state = self.state();
        state.events.record(ContextEvent::GetContiguousRecoveredDa {
            first_update_seq_no,
            recovered_l1_frontier,
        });
        if let Some(error) = state
            .injected_behavior
            .take_next_recovered_da_read_failure()
        {
            return Err(error);
        }
        Ok(state
            .recovered_da
            .get_contiguous_from(first_update_seq_no, recovered_l1_frontier))
    }

    async fn prune_recovered_da_before(
        &self,
        update_seq_no: u64,
    ) -> Result<(), RecoveredDaDbError> {
        let mut state = self.state();
        state
            .events
            .record(ContextEvent::PruneRecoveredDa { update_seq_no });
        if let Some(error) = state
            .injected_behavior
            .take_next_recovered_da_prune_failure()
        {
            return Err(error);
        }
        state.recovered_da.prune_before(update_seq_no);
        Ok(())
    }

    async fn clear_recovered_da(&self) -> Result<(), RecoveredDaDbError> {
        let mut state = self.state();
        state.events.record(ContextEvent::ClearRecoveredDa);
        if let Some(error) = state
            .injected_behavior
            .take_next_recovered_da_clear_failure()
        {
            return Err(error);
        }
        state.recovered_da.clear();
        Ok(())
    }

    async fn fetch_commit_block(
        &self,
        commit_txid: Txid,
    ) -> Result<L1BlockCommitment, FetchCommitBlockError> {
        let mut state = self.state();
        state
            .events
            .record(ContextEvent::FetchCommitBlock { commit_txid });
        if let Some(error) = state
            .injected_behavior
            .take_next_commit_block_lookup_failure()
        {
            return Err(error);
        }
        state.bitcoin.fetch_commit_block(commit_txid)
    }

    async fn is_l1_block_canonical(
        &self,
        expected: L1BlockCommitment,
    ) -> Result<bool, CheckL1BlockError> {
        let mut state = self.state();
        state.events.record(ContextEvent::CheckL1BlockCanonicality {
            commitment: expected,
        });
        if let Some(error) = state
            .injected_behavior
            .take_next_l1_block_canonicality_failure()
        {
            return Err(error);
        }
        Ok(state.bitcoin.is_l1_block_canonical(expected))
    }

    fn load_reconstruction_snapshot(
        &self,
    ) -> Result<Option<ReconstructionSnapshot>, SnapshotLoadError> {
        let mut state = self.state();
        state.events.record(ContextEvent::LoadSnapshot);
        if let Some(error) = state.injected_behavior.take_next_snapshot_load_failure() {
            return Err(error);
        }
        Ok(state.snapshots.load())
    }

    fn delete_reconstruction_snapshot(&self) -> Result<(), SnapshotDeleteError> {
        let mut state = self.state();
        state.events.record(ContextEvent::DeleteSnapshot);
        if let Some(error) = state.injected_behavior.take_next_snapshot_delete_failure() {
            return Err(error);
        }
        state.snapshots.delete();
        Ok(())
    }

    fn save_reconstruction_snapshot(
        &self,
        replay_snapshot: &BatchReplaySnapshot,
        verified_account_state: &VerifiedAccountState,
        resume_l1_block: L1BlockCommitment,
        completion_block: L1BlockCommitment,
    ) -> Result<(), SnapshotSaveError> {
        let snapshot = StoredReconstructionSnapshot {
            replay_snapshot: replay_snapshot.clone(),
            verified_account_state: verified_account_state.clone(),
            resume_l1_block,
            completion_block,
        };
        let mut state = self.state();
        let result = match state.injected_behavior.take_next_snapshot_save_failure() {
            Some(error) => Err(error),
            None => {
                state.snapshots.save(snapshot.clone());
                Ok(())
            }
        };
        let outcome = if result.is_ok() {
            SnapshotSaveOutcome::Succeeded
        } else {
            SnapshotSaveOutcome::Failed
        };
        state
            .events
            .record(ContextEvent::SaveSnapshot(Box::new(SnapshotSaveAttempt {
                snapshot,
                outcome,
            })));
        result
    }
}

#[async_trait]
impl OLAccountUpdateSource for MockDaVerifierContext {
    async fn fetch_finalized_next_update_seq_no(
        &self,
        account_id: AccountId,
    ) -> Result<Seqno, OLAccountUpdateError> {
        let mut state = self.state();
        state
            .events
            .record(ContextEvent::FetchFinalizedAccountState { account_id });
        if let Some(result) = state
            .injected_behavior
            .take_next_finalized_frontier_override()
        {
            return result;
        }
        Ok(state.account_update_source.next_update_seq_no(account_id))
    }

    async fn fetch_account_update(
        &self,
        account_id: AccountId,
        update_seq_no: Seqno,
    ) -> Result<OLAccountUpdate, OLAccountUpdateError> {
        let mut state = self.state();
        state.events.record(ContextEvent::FetchAccountUpdate {
            account_id,
            update_seq_no,
        });
        if let Some(error) = state
            .injected_behavior
            .take_ol_update_failure(update_seq_no)
        {
            return Err(error);
        }
        state
            .account_update_source
            .fetch_update(account_id, update_seq_no)
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
        let account_id = params.strata_exec_account_id();
        let sequencer = TestEeSequencer::new(SEQUENCER_KEY_SEED, &params);
        Self {
            params,
            genesis_l1_height,
            l1_reorg_safe_depth,
            max_l1_scan_window_size: TEST_MAX_L1_SCAN_WINDOW_SIZE,
            context: MockDaVerifierContext::new(account_id),
            sequencer,
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

    fn replace_bitcoin_block(
        &self,
        height: L1Height,
        block_discriminator: u32,
    ) -> L1BlockCommitment {
        self.context
            .state()
            .bitcoin
            .replace_block(height, block_discriminator)
    }

    /// Account whose EE updates this verifier follows.
    fn account_id(&self) -> AccountId {
        self.params.strata_exec_account_id()
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

    fn process_sau(&self, update: &TestEeUpdate) {
        self.context
            .state()
            .account_update_source
            .process_sau(update);
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
        let verification_state = DaVerificationState::new(self.params.clone());
        DaVerifierServiceState::new(
            Arc::clone(&self.context),
            recovery_state,
            verification_state,
        )
    }

    /// Builds fresh in-memory service state over the existing external test world.
    fn restart_verifier_service_state(&self) -> DaVerifierServiceState<MockDaVerifierContext> {
        self.build_verifier_service_state()
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

    fn fail_next_recovered_da_read(&self, error: RecoveredDaDbError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_recovered_da_read(error);
    }

    fn fail_next_recovered_da_prune(&self, error: RecoveredDaDbError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_recovered_da_prune(error);
    }

    fn fail_next_recovered_da_clear(&self, error: RecoveredDaDbError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_recovered_da_clear(error);
    }

    fn fail_ol_update_at(&self, update_seq_no: Seqno, error: OLAccountUpdateError) {
        self.context
            .state()
            .injected_behavior
            .fail_ol_update_at(update_seq_no, error);
    }

    fn override_next_finalized_frontier(&self, result: Result<Seqno, OLAccountUpdateError>) {
        self.context
            .state()
            .injected_behavior
            .override_next_finalized_frontier(result);
    }

    fn fail_next_commit_block_lookup(&self, error: FetchCommitBlockError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_commit_block_lookup(error);
    }

    fn fail_next_l1_block_canonicality_check(&self, error: CheckL1BlockError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_l1_block_canonicality_check(error);
    }

    fn fail_next_snapshot_load(&self, error: SnapshotLoadError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_snapshot_load(error);
    }

    fn fail_next_snapshot_delete(&self, error: SnapshotDeleteError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_snapshot_delete(error);
    }

    fn fail_next_snapshot_save(&self, error: SnapshotSaveError) {
        self.context
            .state()
            .injected_behavior
            .fail_next_snapshot_save(error);
    }

    fn seed_recovered_da(&self, recovered_blobs: &[RecoveredDaBlob]) {
        self.context
            .state()
            .recovered_da
            .put(recovered_blobs)
            .expect("test recovered DA should seed the store");
    }

    /// Stores valid snapshot data without exercising the current process's save path.
    ///
    /// Restart tests use this to model a snapshot file written by another process or runtime
    /// configuration, such as one whose recovery boundary predates the current genesis height.
    /// Snapshot-internal invariants must still hold because the mock load path validates them.
    fn store_snapshot_for_restart(&self, snapshot: StoredReconstructionSnapshot) {
        self.context.state().snapshots.save(snapshot);
    }

    /// Confirms one update on L1 and seeds its recovered candidate without its OL update.
    fn mine_and_seed_recovered_da_candidate(
        &self,
        update: &TestEeUpdate,
        height: L1Height,
    ) -> L1BlockCommitment {
        self.publish_da(update);
        let completion_block = self.mine_block(
            height,
            iter::once(update.commit_transaction()).chain(update.reveal_transactions()),
        );
        self.seed_recovered_da(&[RecoveredDaBlob::new(
            DaL1Ref::new(update.commit_txid(), completion_block),
            update.blob().clone(),
        )]);
        completion_block
    }

    fn seed_ol_updates_and_recovered_da_candidates(&self, updates: &[TestEeUpdate]) {
        for (index, update) in updates.iter().enumerate() {
            let height = self
                .genesis_l1_height
                .checked_add(u32::try_from(index).expect("test index fits in L1 height"))
                .expect("test candidate height fits in L1 height");
            self.mine_and_seed_recovered_da_candidate(update, height);
            self.process_sau(update);
        }
    }

    fn recovery_operations(&self) -> Vec<ContextOperation> {
        self.context.state().events.recovery_operations()
    }

    fn recovered_da_read_requests(&self) -> Vec<(u64, L1Height)> {
        self.context.state().events.recovered_da_read_requests()
    }

    fn account_update_requests(&self) -> Vec<(AccountId, Seqno)> {
        self.context.state().events.account_update_requests()
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

    fn commit_block_lookups(&self) -> Vec<Txid> {
        self.context.state().events.commit_block_lookups()
    }

    fn snapshot_save_attempts(&self) -> Vec<SnapshotSaveAttempt> {
        self.context.state().events.snapshot_save_attempts()
    }

    fn snapshot_load_count(&self) -> usize {
        self.context.state().events.snapshot_load_count()
    }

    fn l1_block_canonicality_checks(&self) -> Vec<L1BlockCommitment> {
        self.context.state().events.l1_block_canonicality_checks()
    }

    fn saved_snapshot(&self) -> Option<StoredReconstructionSnapshot> {
        self.context.state().snapshots.snapshot().cloned()
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

fn assert_stored_snapshot_is_consistent(snapshot: &StoredReconstructionSnapshot) {
    assert_eq!(
        Hash::new(snapshot.replay_snapshot.state_root().0),
        snapshot
            .verified_account_state
            .state()
            .last_exec_state_root()
    );
    assert_eq!(
        compute_ee_account_inner_root(snapshot.verified_account_state.state()),
        snapshot.verified_account_state.expected_inner_state_root()
    );
}

fn make_test_l1_commitment(seed: u8) -> L1BlockCommitment {
    L1BlockCommitment::new(
        TEST_GENESIS_L1_HEIGHT,
        L1BlockId::from(Buf32::from([seed; 32])),
    )
}

fn produce_test_updates(fixture: &mut DaVerifierFixture, count: u8) -> Vec<TestEeUpdate> {
    (1..=count)
        .map(|seed| fixture.produce_update(build_test_state_diff(seed)))
        .collect()
}

/// Bitcoin and OL data for the multi-tick verifier scenario.
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

/// Builds the ten-update Bitcoin and OL layout for the multi-tick verifier scenario.
///
/// Updates 0 through 8 are complete and processed on OL. Update 9 has its commit and first reveal
/// mined but remains incomplete and absent from OL. The completed updates include same-block
/// commit/reveal, cross-block reveals, shared completion blocks, and reverse completion order.
fn build_multi_tick_da_scenario(fixture: &mut DaVerifierFixture) -> MultiTickDaScenario {
    let update_0 = fixture.produce_update(build_test_state_diff(1));
    let update_1 = fixture.produce_update(build_test_state_diff(2));
    let update_2 = fixture.produce_update(build_test_state_diff(3));
    let update_3 = fixture.produce_update(build_test_state_diff(4));
    let update_4 = fixture.produce_two_reveal_update(build_test_state_diff(5));
    let update_5 = fixture.produce_update(build_test_state_diff(6));
    let update_6 = fixture.produce_update(build_test_state_diff(7));
    let update_7 = fixture.produce_two_reveal_update(build_test_state_diff(8));
    let update_8 = fixture.produce_update(build_test_state_diff(9));
    let update_9 = fixture.produce_two_reveal_update(build_test_state_diff(10));
    let updates = [
        update_0, update_1, update_2, update_3, update_4, update_5, update_6, update_7, update_8,
        update_9,
    ];

    for update in &updates {
        fixture.publish_da(update);
    }
    for update in &updates[..9] {
        fixture.process_sau(update);
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

/// Builds the multi-tick DA scenario and processes its first verifier tick.
///
/// The returned service state has recovered and verified updates 0 through 8 and saved their
/// snapshot. Update 9 remains incomplete, allowing callers to define the second-tick outcome.
async fn run_multi_tick_da_scenario_first_tick() -> (
    DaVerifierFixture,
    MultiTickDaScenario,
    DaVerifierServiceState<MockDaVerifierContext>,
) {
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        MULTI_TICK_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let scenario = build_multi_tick_da_scenario(&mut fixture);
    fixture.set_bitcoin_tip_height(MULTI_TICK_FIRST_TICK_TIP);
    let mut state = fixture.build_verifier_service_state();
    state.handle_tick().await.expect("tick processing succeeds");
    (fixture, scenario, state)
}

/// Makes update 9 available to the multi-tick scenario's next verifier tick.
///
/// Mines its final reveal, processes its OL account update, and advances the Bitcoin tip without
/// running the verifier.
fn make_multi_tick_update_9_available_for_second_tick(
    fixture: &DaVerifierFixture,
    scenario: &MultiTickDaScenario,
) -> L1BlockCommitment {
    let completion_block = fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT,
        [&scenario.updates[9].reveal_transactions()[1]],
    );
    fixture.process_sau(&scenario.updates[9]);
    fixture.set_bitcoin_tip_height(MULTI_TICK_SECOND_TICK_TIP);
    completion_block
}

/// Builds a fixture whose saved snapshot has distinct resume and completion blocks.
///
/// Update 1 is the final update in sequence order, so its earlier commit is the resume block.
/// Update 0 completes later, so its completion is the snapshot's highest covered L1 block.
async fn build_fixture_with_distinct_snapshot_boundaries() -> DaVerifierFixture {
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let updates = produce_test_updates(&mut fixture, 2);
    for update in &updates {
        fixture.publish_da(update);
    }

    fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [updates[1].commit_transaction()]);
    fixture.mine_block(
        TEST_GENESIS_L1_HEIGHT + 1,
        [updates[0].commit_transaction()],
    );
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 2, updates[1].reveal_transactions());
    let completion_block =
        fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 3, updates[0].reveal_transactions());
    fixture.set_bitcoin_tip_height(completion_block.height() + TEST_L1_REORG_SAFE_DEPTH);
    for update in &updates {
        fixture.process_sau(update);
    }

    let mut state = fixture.build_verifier_service_state();
    state.handle_tick().await.expect("tick processing succeeds");
    fixture
}

/// Processes a fresh service state's first tick with an injected snapshot-load failure.
async fn run_first_tick_with_snapshot_load_failure(
    load_error: SnapshotLoadError,
) -> (
    DaVerifierFixture,
    DaVerifierServiceState<MockDaVerifierContext>,
    DaVerifierError,
) {
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    fixture.fail_next_snapshot_load(load_error);
    let mut state = fixture.build_verifier_service_state();
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the injected snapshot load failure");

    (fixture, state, error)
}

/// Asserts that snapshot initialization has not changed either service frontier.
#[track_caller]
fn assert_service_uninitialized(
    state: &DaVerifierServiceState<MockDaVerifierContext>,
    genesis_l1_height: L1Height,
) {
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(state);
    assert_eq!(status.next_l1_height, genesis_l1_height);
    assert_eq!(status.reorg_safe_tip, None);
    assert_eq!(status.next_update_seq_no, 0);
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
        fixture.recovery_operations(),
        [ContextOperation::FetchBitcoinTip]
    );
}

#[tokio::test]
async fn test_genesis_start_clears_recovered_da() {
    // 1. Seed a stale candidate without a snapshot and expose one reorg-safe block.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(update.commit_txid(), make_test_l1_commitment(1)),
        update.blob().clone(),
    )]);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process the first tick from genesis.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show initialization clears stale DA before recovery requests its first L1 range.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_l1_height, TEST_GENESIS_L1_HEIGHT + 1);
    assert_eq!(status.reorg_safe_tip, Some(TEST_GENESIS_L1_HEIGHT));
    assert_eq!(status.next_update_seq_no, 0);
    assert!(fixture.recovered_da_blobs().is_empty());
    assert_eq!(
        fixture.context_operations(),
        [
            ContextOperation::LoadSnapshot,
            ContextOperation::ClearRecoveredDa,
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_GENESIS_L1_HEIGHT,
            },
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 0,
                recovered_l1_frontier: TEST_GENESIS_L1_HEIGHT,
            },
        ]
    );
}

#[tokio::test]
async fn test_failed_clear_stops_initialization() {
    // 1. Seed retained DA without a snapshot and make the initialization clear fail.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(update.commit_txid(), make_test_l1_commitment(1)),
        update.blob().clone(),
    )]);
    fixture.fail_next_recovered_da_clear(RecoveredDaDbError::WorkerCancelled);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process the first tick and show the clear failure is returned before recovery starts.
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the recovered-DA clear failure");
    assert!(matches!(
        error,
        DaVerifierError::RecoveredDaDb(RecoveredDaDbError::WorkerCancelled)
    ));

    // 3. Show initialization remains pending and retained DA stays intact for a retry.
    assert_service_uninitialized(&state, TEST_GENESIS_L1_HEIGHT);
    assert_one_recovered_blob(
        &fixture.recovered_da_blobs(),
        &update,
        make_test_l1_commitment(1),
    );
    assert_eq!(
        fixture.context_operations(),
        [
            ContextOperation::LoadSnapshot,
            ContextOperation::ClearRecoveredDa,
        ]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_waiting_for_reorg_safe_tip_verifies_da_through_previous_frontier() {
    // 1. Recover candidate 0 from the genesis block without its OL update, then
    // retain that frontier while the mocked Bitcoin tip falls below the reorg-safe depth.
    let genesis_l1_height = TEST_L1_REORG_SAFE_DEPTH - 1;
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        genesis_l1_height,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(genesis_l1_height + TEST_L1_REORG_SAFE_DEPTH);
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.publish_da(&update);
    let completion_block = fixture.mine_block(
        genesis_l1_height,
        iter::once(update.commit_transaction()).chain(update.reveal_transactions()),
    );
    let mut state = fixture.build_verifier_service_state();
    state.handle_tick().await.expect("tick processing succeeds");
    let initial_scan_operations = fixture.context_operations();
    fixture.set_bitcoin_tip_height(genesis_l1_height);

    // 2. Finalize update 0 on OL, then process a tick with no L1 range to scan.
    fixture.process_sau(&update);
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show the tick verifies through the retained frontier without
    // requesting another L1 block range.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 1);
    let operations = fixture.context_operations();
    assert_eq!(
        &operations[initial_scan_operations.len()..],
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 0,
                recovered_l1_frontier: genesis_l1_height,
            },
            ContextOperation::FetchFinalizedAccountState {
                account_id: fixture.account_id(),
            },
            ContextOperation::FetchAccountUpdate {
                account_id: fixture.account_id(),
                update_seq_no: Seqno::zero(),
            },
            ContextOperation::FetchCommitBlock {
                commit_txid: update.commit_txid(),
            },
            ContextOperation::SaveSnapshot {
                resume_l1_block: completion_block,
                completion_block,
            },
            ContextOperation::PruneRecoveredDa { update_seq_no: 1 },
        ]
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
        fixture.recovery_operations(),
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
    let updates = produce_test_updates(&mut fixture, 2);
    let update = &updates[0];
    fixture.publish_da(update);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 2, [update.commit_transaction()]);
    let completion_block =
        fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 3, update.reveal_transactions());
    // Keep finalized OL one update ahead so reaching update 1 does not stop the scan loop
    // before the final L1 window. Recovery across the window boundary is what this test asserts.
    for update in &updates {
        fixture.process_sau(update);
    }
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick across all three scan windows.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show the reveal completes the commit retained by the extractor from the prior window.
    let writes = fixture.recovered_da_write_attempts();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].outcome(), RecoveredDaWriteOutcome::Succeeded);
    assert_one_recovered_blob(writes[0].blobs(), update, completion_block);
    assert_eq!(state.next_l1_height(), TEST_WINDOWED_REORG_SAFE_TIP + 1);
}

#[tokio::test]
async fn test_recoverable_window_failure_verifies_stored_da_before_ending_tick() {
    // 1. Confirm and seed candidate 0 with its finalized OL update, then fail
    // the second block fetch in the first of three scan windows.
    let failed_height = TEST_GENESIS_L1_HEIGHT + 1;
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_max_l1_scan_window_size(TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE)
    .with_bitcoin_tip_height(TEST_WINDOWED_BITCOIN_TIP);
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.process_sau(&update);
    let completion_block =
        fixture.mine_and_seed_recovered_da_candidate(&update, TEST_GENESIS_L1_HEIGHT);
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
    state
        .handle_tick()
        .await
        .expect("a recoverable fetch failure does not fail the tick");

    // 3. Show the cursor retains progress through the preceding block, candidate
    // 0 verifies at that frontier, and no later scan window is requested.
    assert_eq!(state.next_l1_height(), failed_height);
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 1);
    assert_eq!(
        fixture.context_operations(),
        [
            ContextOperation::LoadSnapshot,
            ContextOperation::ClearRecoveredDa,
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_GENESIS_L1_HEIGHT + 2,
            },
            ContextOperation::PutRecoveredDa,
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 0,
                recovered_l1_frontier: TEST_GENESIS_L1_HEIGHT,
            },
            ContextOperation::FetchFinalizedAccountState {
                account_id: fixture.account_id(),
            },
            ContextOperation::FetchAccountUpdate {
                account_id: fixture.account_id(),
                update_seq_no: Seqno::zero(),
            },
            ContextOperation::FetchCommitBlock {
                commit_txid: update.commit_txid(),
            },
            ContextOperation::SaveSnapshot {
                resume_l1_block: completion_block,
                completion_block,
            },
            ContextOperation::PruneRecoveredDa { update_seq_no: 1 },
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
    let tick_one_operations = fixture.recovery_operations();
    assert_eq!(tick_one_operations, expected_tick_one_operations);

    // 4. Complete update 9 above tick 1's safe tip, advance the tip, and process tick 2.
    let update_9_completion = fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT,
        [&scenario.updates[9].reveal_transactions()[1]],
    );
    fixture.set_bitcoin_tip_height(MULTI_TICK_SECOND_TICK_TIP);
    state.handle_tick().await.expect("tick processing succeeds");

    // 5. Show extractor state retained from tick 1 completes and persists update 9 exactly once,
    // while the finalized OL frontier leaves verification waiting at seqno 9.
    assert_eq!(state.next_l1_height(), MULTI_TICK_SECOND_TICK_SAFE_TIP + 1);
    let tick_two_writes = fixture.recovered_da_write_attempts();
    assert_eq!(tick_two_writes.len(), 9);
    assert_one_recovered_blob(
        tick_two_writes[8].blobs(),
        &scenario.updates[9],
        update_9_completion,
    );
    assert_one_recovered_blob(
        &fixture.recovered_da_blobs(),
        &scenario.updates[9],
        update_9_completion,
    );
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 9);

    // 6. Make update 9's OL account update available, then process tick 3 without adding
    // Bitcoin data after recovery has passed the safe tip.
    fixture.process_sau(&scenario.updates[9]);
    state.handle_tick().await.expect("tick processing succeeds");

    // 7. Show caught-up recovery requests no block range and verification advances through
    // update 9.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_l1_height, MULTI_TICK_SECOND_TICK_SAFE_TIP + 1);
    assert_eq!(status.reorg_safe_tip, Some(MULTI_TICK_SECOND_TICK_SAFE_TIP));
    assert_eq!(status.next_update_seq_no, 10);
    let all_operations = fixture.recovery_operations();
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
async fn test_ol_lag_stops_before_next_l1_scan_window() {
    // 1. Put candidate 0 in the first of two three-block scan windows, but
    // leave finalized OL state at seqno 0.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_max_l1_scan_window_size(TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE);
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.publish_da(&update);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [update.commit_transaction()]);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 1, update.reveal_transactions());
    let reorg_safe_tip = TEST_GENESIS_L1_HEIGHT + 5;
    fixture.set_bitcoin_tip_height(reorg_safe_tip + L1Height::from(TEST_L1_REORG_SAFE_DEPTH));
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick and show OL lag stops catch-up after the first window.
    state.handle_tick().await.expect("tick processing succeeds");

    assert_eq!(state.next_l1_height(), TEST_GENESIS_L1_HEIGHT + 3);
    assert_eq!(
        fixture.recovery_operations(),
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_GENESIS_L1_HEIGHT + 2,
            },
            ContextOperation::PutRecoveredDa,
        ]
    );
    let operations_after_first_tick = fixture.context_operations().len();

    // 3. Finalize OL update 0 and process another tick. The retained candidate
    // verifies, reaches the frozen finalized frontier, and ends the tick before recovery.
    fixture.process_sau(&update);
    state.handle_tick().await.expect("tick processing succeeds");

    assert_eq!(state.next_l1_height(), TEST_GENESIS_L1_HEIGHT + 3);
    let saved_snapshot = fixture
        .saved_snapshot()
        .expect("verifying update 0 saves its snapshot");
    let operations = fixture.context_operations();
    assert_eq!(
        &operations[operations_after_first_tick..],
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 0,
                recovered_l1_frontier: TEST_GENESIS_L1_HEIGHT + 2,
            },
            ContextOperation::FetchFinalizedAccountState {
                account_id: fixture.account_id(),
            },
            ContextOperation::FetchAccountUpdate {
                account_id: fixture.account_id(),
                update_seq_no: Seqno::zero(),
            },
            ContextOperation::FetchCommitBlock {
                commit_txid: update.commit_txid(),
            },
            ContextOperation::SaveSnapshot {
                resume_l1_block: saved_snapshot.resume_l1_block,
                completion_block: saved_snapshot.completion_block,
            },
            ContextOperation::PruneRecoveredDa { update_seq_no: 1 },
        ]
    );
    let operations_after_second_tick = operations.len();

    // 4. Process a third tick without adding data. The retained-candidate check is now empty,
    // so recovery resumes with the second L1 window.
    state.handle_tick().await.expect("tick processing succeeds");

    assert_eq!(state.next_l1_height(), reorg_safe_tip + 1);
    let operations = fixture.context_operations();
    assert_eq!(
        &operations[operations_after_second_tick..],
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 1,
                recovered_l1_frontier: TEST_GENESIS_L1_HEIGHT + 2,
            },
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT + 3,
                end_height: reorg_safe_tip,
            },
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 1,
                recovered_l1_frontier: reorg_safe_tip,
            },
        ]
    );
    assert_eq!(
        fixture.recovery_operations(),
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_GENESIS_L1_HEIGHT + 2,
            },
            ContextOperation::PutRecoveredDa,
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT + 3,
                end_height: reorg_safe_tip,
            },
        ]
    );
}

#[tokio::test]
async fn test_tick_verifies_recovered_da_before_scanning_next_l1_window() {
    // 1. Put one DA candidate in each of two three-block L1 scan windows and
    // finalize both matching OL updates.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_max_l1_scan_window_size(TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE);
    let updates = produce_test_updates(&mut fixture, 2);
    for update in &updates {
        fixture.publish_da(update);
    }
    let first_commit_block =
        fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [updates[0].commit_transaction()]);
    let first_completion_block =
        fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 1, updates[0].reveal_transactions());
    let second_commit_block = fixture.mine_block(
        TEST_GENESIS_L1_HEIGHT + 3,
        [updates[1].commit_transaction()],
    );
    let second_completion_block =
        fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 4, updates[1].reveal_transactions());
    for update in &updates {
        fixture.process_sau(update);
    }
    let reorg_safe_tip = TEST_GENESIS_L1_HEIGHT + 5;
    fixture.set_bitcoin_tip_height(reorg_safe_tip + L1Height::from(TEST_L1_REORG_SAFE_DEPTH));
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick across both windows.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show update 0 is loaded and verified before recovery requests the second window.
    assert_eq!(
        fixture.context_operations(),
        [
            ContextOperation::LoadSnapshot,
            ContextOperation::ClearRecoveredDa,
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_GENESIS_L1_HEIGHT + 2,
            },
            ContextOperation::PutRecoveredDa,
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 0,
                recovered_l1_frontier: TEST_GENESIS_L1_HEIGHT + 2,
            },
            ContextOperation::FetchFinalizedAccountState {
                account_id: fixture.account_id(),
            },
            ContextOperation::FetchAccountUpdate {
                account_id: fixture.account_id(),
                update_seq_no: Seqno::zero(),
            },
            ContextOperation::FetchCommitBlock {
                commit_txid: updates[0].commit_txid(),
            },
            ContextOperation::SaveSnapshot {
                resume_l1_block: first_commit_block,
                completion_block: first_completion_block,
            },
            ContextOperation::PruneRecoveredDa { update_seq_no: 1 },
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT + 3,
                end_height: reorg_safe_tip,
            },
            ContextOperation::PutRecoveredDa,
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 1,
                recovered_l1_frontier: reorg_safe_tip,
            },
            ContextOperation::FetchAccountUpdate {
                account_id: fixture.account_id(),
                update_seq_no: Seqno::new(1),
            },
            ContextOperation::FetchCommitBlock {
                commit_txid: updates[1].commit_txid(),
            },
            ContextOperation::SaveSnapshot {
                resume_l1_block: second_commit_block,
                completion_block: second_completion_block,
            },
            ContextOperation::PruneRecoveredDa { update_seq_no: 2 },
        ]
    );
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 2);
}

#[tokio::test]
async fn test_caught_up_verification_retries_before_next_l1_scan_window() {
    // 1. Recover candidate 0, but fail its first candidate-store read after recovery has
    // advanced through the candidate's completion block.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.publish_da(&update);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [update.commit_transaction()]);
    fixture.mine_block(
        TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT,
        update.reveal_transactions(),
    );
    fixture.process_sau(&update);
    fixture.set_bitcoin_tip_height(TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);
    fixture.fail_next_recovered_da_read(RecoveredDaDbError::WorkerCancelled);
    let mut state = fixture.build_verifier_service_state();
    let error = state
        .handle_tick()
        .await
        .expect_err("candidate-store read failure ends tick processing");
    assert!(matches!(
        error,
        DaVerifierError::RecoveredDaDb(RecoveredDaDbError::WorkerCancelled)
    ));

    // 2. Process a caught-up tick. It verifies candidate 0 and records that verification
    // reached the finalized OL frontier.
    state.handle_tick().await.expect("tick processing succeeds");
    assert_eq!(state.next_update_seq_no(), 1);
    let operations_after_caught_up_tick = fixture.context_operations().len();

    // 3. Advance the safe tip by one block. The next tick must retry verification against
    // the prior frontier before requesting the new L1 range.
    let next_reorg_safe_tip = TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT + 1;
    fixture.set_bitcoin_tip_height(next_reorg_safe_tip + TEST_L1_REORG_SAFE_DEPTH);
    state.handle_tick().await.expect("tick processing succeeds");

    let operations = fixture.context_operations();
    assert_eq!(
        &operations[operations_after_caught_up_tick..],
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 1,
                recovered_l1_frontier: TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT,
            },
            ContextOperation::FetchL1BlockRange {
                start_height: next_reorg_safe_tip,
                end_height: next_reorg_safe_tip,
            },
            ContextOperation::GetContiguousRecoveredDa {
                first_update_seq_no: 1,
                recovered_l1_frontier: next_reorg_safe_tip,
            },
        ]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_finalized_ol_frontier_is_fetched_once_per_tick() {
    // 1. Put one OL-attested candidate in each of two three-block scan windows.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_max_l1_scan_window_size(TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE);
    let updates = produce_test_updates(&mut fixture, 2);
    for update in &updates {
        fixture.publish_da(update);
    }
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [updates[0].commit_transaction()]);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 1, updates[0].reveal_transactions());
    fixture.mine_block(
        TEST_GENESIS_L1_HEIGHT + 3,
        [updates[1].commit_transaction()],
    );
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 4, updates[1].reveal_transactions());
    for update in &updates {
        fixture.process_sau(update);
    }
    let reorg_safe_tip = TEST_GENESIS_L1_HEIGHT + 5;
    fixture.set_bitcoin_tip_height(reorg_safe_tip + L1Height::from(TEST_L1_REORG_SAFE_DEPTH));
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick across both windows.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show both candidates verify against one frozen finalized OL frontier.
    let finalized_frontier_reads = fixture
        .context_operations()
        .into_iter()
        .filter(|operation| {
            matches!(
                operation,
                ContextOperation::FetchFinalizedAccountState { .. }
            )
        })
        .count();
    assert_eq!(finalized_frontier_reads, 1);
    assert_eq!(fixture.recovered_da_read_requests(), [(0, 12), (1, 15)]);
    assert_eq!(
        fixture.account_update_requests(),
        [
            (fixture.account_id(), Seqno::zero()),
            (fixture.account_id(), Seqno::new(1)),
        ]
    );
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 2);
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

    // 2. Process one tick; the recoverable Bitcoin failure still permits verification of stored
    // candidates, so the empty candidate store lets tick processing finish successfully.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show recovery retained progress through the blocks preceding the failed height.
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
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.process_sau(&update);
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

    // 4. Show the same blob was retried successfully before the next tip fetch,
    // became eligible at its completion height, and required no second block-range request.
    // Snapshot persistence then prunes the verified candidate.
    assert_eq!(
        service_state.next_l1_height(),
        TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT + 1
    );
    assert_eq!(
        fixture.recovery_operations(),
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
    assert!(fixture.recovered_da_blobs().is_empty());
    assert_eq!(
        fixture.recovered_da_read_requests(),
        [(0, TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT)]
    );
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&service_state);
    assert_eq!(status.next_update_seq_no, 1);
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
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.process_sau(&update);
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
        fixture.recovery_operations(),
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
    let update = fixture.produce_update(build_test_state_diff(1));
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
async fn test_recoverable_bitcoin_failure_allows_stored_da_verification() {
    // 1. Recover candidate 0 while OL has not finalized its matching update.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.mine_and_seed_recovered_da_candidate(&update, TEST_GENESIS_L1_HEIGHT);
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);
    let mut state = fixture.build_verifier_service_state();
    state.handle_tick().await.expect("tick processing succeeds");
    assert_eq!(state.next_update_seq_no(), 0);

    // 2. Finalize update 0 on OL, then make the next Bitcoin tip lookup time out recoverably.
    fixture.process_sau(&update);
    fixture.fail_next_bitcoin_tip(FetchBitcoinTipError::Rpc(ClientError::Timeout));

    // 3. Process another tick and show the stored candidate still verifies through seqno 0.
    state.handle_tick().await.expect("tick processing succeeds");

    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 1);
    assert_eq!(
        fixture.account_update_requests(),
        [(fixture.account_id(), Seqno::zero())]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_fatal_bitcoin_failure_blocks_stored_da_verification() {
    // 1. Scan the genesis block so the service has a real recovered L1 frontier.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let mut state = fixture.build_verifier_service_state();
    state.handle_tick().await.expect("tick processing succeeds");
    let recovered_da_reads_before_failure = fixture.recovered_da_read_requests();

    // 2. Seed candidate 0 and its matching OL update, then make the next
    // Bitcoin tip lookup fail fatally.
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.process_sau(&update);
    let completion_block = make_test_l1_commitment(1);
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(update.commit_txid(), completion_block),
        update.blob().clone(),
    )]);
    fixture.fail_next_bitcoin_tip(FetchBitcoinTipError::Rpc(ClientError::MissingUserPassword));

    // 3. Process another tick and show the fatal failure prevents verification.
    let error = state
        .handle_tick()
        .await
        .expect_err("a fatal Bitcoin failure must end the tick");

    assert!(matches!(
        error,
        DaVerifierError::FetchBitcoinTip(FetchBitcoinTipError::Rpc(
            ClientError::MissingUserPassword
        ))
    ));
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 0);
    assert_eq!(
        fixture.recovered_da_read_requests(),
        recovered_da_reads_before_failure
    );
    assert!(fixture.account_update_requests().is_empty());
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_recoverable_persistence_failure_blocks_verification() {
    // 1. Seed verifiable candidate 0 and publish update 1 on L1.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);
    let stored = fixture.produce_update(build_test_state_diff(1));
    fixture.process_sau(&stored);
    let stored_l1_block = make_test_l1_commitment(1);
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(stored.commit_txid(), stored_l1_block),
        stored.blob().clone(),
    )]);
    let unpersisted = fixture.produce_update(build_test_state_diff(2));
    fixture.publish_da(&unpersisted);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [unpersisted.commit_transaction()]);
    fixture.mine_block(
        TEST_TWO_BLOCK_DA_COMPLETION_HEIGHT,
        unpersisted.reveal_transactions(),
    );

    // 2. Make update 1's candidate-store write fail recoverably.
    fixture.fail_next_recovered_da_write(RecoveredDaDbError::WorkerCancelled);
    let mut state = fixture.build_verifier_service_state();

    // 3. Process a tick and show the store failure blocks verification at seqno 0.
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing fails when candidate persistence fails");

    assert!(matches!(
        error,
        DaVerifierError::DaRecovery(DaRecoveryError::Database(
            RecoveredDaDbError::WorkerCancelled
        ))
    ));
    // The write is worth retrying, but recovery has just reported the
    // candidate store failing, so this tick must not turn around and read it.
    assert!(error.is_recoverable());
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 0);
    assert!(fixture.recovered_da_read_requests().is_empty());
    assert!(fixture.account_update_requests().is_empty());
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_tick_replays_contiguous_recovered_da_prefix() {
    // 1. Seed candidates and matching OL updates 0 through 2.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let account_id = fixture.account_id();
    let updates = produce_test_updates(&mut fixture, 3);
    fixture.seed_ol_updates_and_recovered_da_candidates(&updates);
    fixture.set_bitcoin_tip_height(
        TEST_GENESIS_L1_HEIGHT
            + u32::try_from(updates.len() - 1).expect("test update count fits in L1 height")
            + TEST_L1_REORG_SAFE_DEPTH,
    );
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick, which reads the complete contiguous recovered prefix.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show one candidate read verifies all three updates and reaches seqno 3.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    // All three entries in `updates` verified, so verification next expects sequence 3.
    assert_eq!(status.next_update_seq_no, 3);
    assert_eq!(
        fixture.recovered_da_read_requests(),
        [(0, TEST_GENESIS_L1_HEIGHT + 2)]
    );
    assert_eq!(
        fixture.account_update_requests(),
        [
            (account_id, Seqno::new(0)),
            (account_id, Seqno::new(1)),
            (account_id, Seqno::new(2)),
        ]
    );
}

#[tokio::test]
async fn test_verification_waits_for_missing_da_sequence_then_resumes() {
    // 1. Seed candidates 0, 2 and 3 with matching OL updates 0 through 3.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let account_id = fixture.account_id();
    let updates = produce_test_updates(&mut fixture, 4);
    for update in &updates {
        fixture.process_sau(update);
    }
    // Sequence 1 is absent from the recovered-DA store. Each other candidate
    // completes in the block that confirms it.
    for &index in &[0, 2, 3] {
        fixture.mine_and_seed_recovered_da_candidate(
            &updates[index],
            TEST_GENESIS_L1_HEIGHT
                + u32::try_from(index).expect("test update index fits in L1 height"),
        );
    }
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + 3 + TEST_L1_REORG_SAFE_DEPTH);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process a tick and show verification stops at missing seqno 1.
    state.handle_tick().await.expect("tick processing succeeds");

    // The seeded candidates omit `updates[1]`, so only the prefix through `updates[0]` verifies.
    assert_eq!(
        fixture.account_update_requests(),
        [(account_id, Seqno::new(0))]
    );

    // 3. Insert candidate 1, process another tick, and show it verifies through 3.
    fixture.mine_and_seed_recovered_da_candidate(
        &updates[1],
        TEST_GENESIS_L1_HEIGHT
            + u32::try_from(updates.len()).expect("test length fits in L1 height"),
    );
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + 4 + TEST_L1_REORG_SAFE_DEPTH);
    state.handle_tick().await.expect("tick processing succeeds");

    assert_eq!(
        fixture.account_update_requests(),
        [
            (account_id, Seqno::new(0)),
            (account_id, Seqno::new(1)),
            (account_id, Seqno::new(2)),
            (account_id, Seqno::new(3)),
        ]
    );
    // Tick one verifies sequence 0, then stalls at missing sequence 1. After
    // sequence 1 is seeded, tick two retries from 1 and verifies through 3.
    assert_eq!(
        fixture.recovered_da_read_requests(),
        [
            (0, TEST_GENESIS_L1_HEIGHT + 3),
            (1, TEST_GENESIS_L1_HEIGHT + 4),
        ]
    );
}

#[tokio::test]
async fn test_verification_advances_past_verified_candidate_prefix() {
    // 1. Seed candidates and matching OL updates 0 and 1.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let updates = produce_test_updates(&mut fixture, 2);
    fixture.seed_ol_updates_and_recovered_da_candidates(&updates);
    let recovered_l1_frontier = TEST_GENESIS_L1_HEIGHT + 1;
    fixture.set_bitcoin_tip_height(recovered_l1_frontier + TEST_L1_REORG_SAFE_DEPTH);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process a tick over the complete contiguous candidate prefix.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show verification advances one past the two verified candidates.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 2);

    // 4. Process another tick without new data and show the next prefix read starts at 2.
    state.handle_tick().await.expect("tick processing succeeds");
    assert_eq!(
        fixture.recovered_da_read_requests(),
        [(0, recovered_l1_frontier), (2, recovered_l1_frontier)]
    );
    assert_eq!(
        fixture.account_update_requests(),
        [
            (fixture.account_id(), Seqno::new(0)),
            (fixture.account_id(), Seqno::new(1)),
        ]
    );
}

#[tokio::test]
async fn test_ol_lag_stops_at_anchor_then_resumes() {
    // 1. Seed candidate 0 without its matching OL update.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.mine_and_seed_recovered_da_candidate(&update, TEST_GENESIS_L1_HEIGHT);
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process a tick and show finalized OL state bounds the available
    // prefix before seqno 0, so no manifest is requested and the anchor stays put.
    state.handle_tick().await.expect("tick processing succeeds");
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 0);
    assert!(fixture.account_update_requests().is_empty());

    // 3. Finalize the OL update, process another tick, and show candidate 0 verifies.
    fixture.process_sau(&update);
    state.handle_tick().await.expect("tick processing succeeds");

    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 1);
    assert_eq!(
        fixture.account_update_requests(),
        [(fixture.account_id(), Seqno::zero())]
    );
}

#[tokio::test]
async fn test_finalized_ol_frontier_commits_available_prefix() {
    // 1. Seed recovered candidates 0 and 1, but finalize only OL update 0.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let updates = produce_test_updates(&mut fixture, 2);
    for (index, update) in updates.iter().enumerate() {
        fixture.mine_and_seed_recovered_da_candidate(
            update,
            TEST_GENESIS_L1_HEIGHT
                + u32::try_from(index).expect("test update index fits in L1 height"),
        );
    }
    fixture.process_sau(&updates[0]);
    let recovered_l1_frontier = TEST_GENESIS_L1_HEIGHT + 1;
    fixture.set_bitcoin_tip_height(recovered_l1_frontier + TEST_L1_REORG_SAFE_DEPTH);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick and show the finalized OL frontier admits only
    // candidate 0, which verifies and advances the anchor to seqno 1.
    state.handle_tick().await.expect("tick processing succeeds");

    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 1);
    assert_eq!(
        fixture.account_update_requests(),
        [(fixture.account_id(), Seqno::zero())]
    );

    // 3. Finalize OL update 1 and show the next tick resumes from seqno 1.
    fixture.process_sau(&updates[1]);
    state.handle_tick().await.expect("tick processing succeeds");

    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 2);
    assert_eq!(
        fixture.recovered_da_read_requests(),
        [(0, recovered_l1_frontier), (1, recovered_l1_frontier)]
    );
    assert_eq!(
        fixture.account_update_requests(),
        [
            (fixture.account_id(), Seqno::zero()),
            (fixture.account_id(), Seqno::new(1)),
        ]
    );
}

#[tokio::test]
async fn test_lagging_finalized_ol_frontier_is_recoverable() {
    // 1. Verify candidate 0 so the in-memory anchor next expects seqno 1.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let updates = produce_test_updates(&mut fixture, 2);
    fixture.process_sau(&updates[0]);
    fixture.mine_and_seed_recovered_da_candidate(&updates[0], TEST_GENESIS_L1_HEIGHT);
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);
    let mut state = fixture.build_verifier_service_state();
    state.handle_tick().await.expect("tick processing succeeds");

    // 2. Seed candidate 1, but make the next finalized-state read regress to
    // seqno 0, behind the already verified frontier at seqno 1.
    fixture.process_sau(&updates[1]);
    fixture.mine_and_seed_recovered_da_candidate(&updates[1], TEST_GENESIS_L1_HEIGHT + 1);
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + 1 + TEST_L1_REORG_SAFE_DEPTH);
    fixture.override_next_finalized_frontier(Ok(Seqno::zero()));

    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the lagging finalized OL frontier");

    // 3. Show the lagging OL view is recoverable and leaves the verified anchor at seqno 1.
    assert!(matches!(
        error,
        DaVerifierError::LaggingFinalizedOLFrontier {
            anchor_next_update_seq_no,
            finalized_next_update_seq_no,
        } if anchor_next_update_seq_no == Seqno::new(1)
            && finalized_next_update_seq_no == Seqno::zero()
    ));
    assert!(error.is_recoverable());
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 1);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_recoverable_ol_failure_retries_from_anchor() {
    // 1. Seed candidate 0 and its matching OL update.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.seed_ol_updates_and_recovered_da_candidates(&[update]);
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);

    // 2. Make the first OL read time out and show the anchor remains at seqno 0.
    fixture.fail_ol_update_at(
        Seqno::zero(),
        OLAccountUpdateError::new(
            io::Error::new(io::ErrorKind::TimedOut, "OL account update unavailable"),
            true,
        ),
    );
    let mut state = fixture.build_verifier_service_state();

    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the OL account update timeout");
    match error {
        DaVerifierError::VerifyAccountState(VerifyAccountStateError::FetchAccountUpdate {
            update_seq_no,
            source: OLAccountUpdateError::Transient(source),
        }) => {
            assert_eq!(update_seq_no, Seqno::zero());
            let source = source
                .downcast_ref::<io::Error>()
                .expect("mock OL timeout uses an IO error");
            assert_eq!(source.kind(), io::ErrorKind::TimedOut);
        }
        error => panic!("unexpected verifier error: {error:?}"),
    }
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 0);

    // 3. Process another tick and show candidate 0 retries and verifies.
    state.handle_tick().await.expect("tick processing succeeds");

    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 1);
    assert_eq!(
        fixture.account_update_requests(),
        [
            (fixture.account_id(), Seqno::zero()),
            (fixture.account_id(), Seqno::zero()),
        ]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_ol_transport_failure_after_available_update_leaves_anchor_unchanged() {
    // 1. Seed candidates and matching OL updates 0 and 1 in one L1 scan window.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    let updates = produce_test_updates(&mut fixture, 2);
    fixture.seed_ol_updates_and_recovered_da_candidates(&updates);
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + 1 + TEST_L1_REORG_SAFE_DEPTH);

    // 2. Fail the first tick's OL read at seqno 1 after it checks seqno 0.
    fixture.fail_ol_update_at(
        Seqno::new(1),
        OLAccountUpdateError::new(
            io::Error::new(io::ErrorKind::TimedOut, "second OL update unavailable"),
            true,
        ),
    );
    let mut state = fixture.build_verifier_service_state();

    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the second OL account update timeout");
    assert!(error.is_recoverable());
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 0);

    // 3. Process another tick and show both updates are requested again.
    state.handle_tick().await.expect("tick processing succeeds");

    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 2);
    assert_eq!(
        fixture.account_update_requests(),
        [
            (fixture.account_id(), Seqno::new(0)),
            (fixture.account_id(), Seqno::new(1)),
            (fixture.account_id(), Seqno::new(0)),
            (fixture.account_id(), Seqno::new(1)),
        ]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_duplicate_candidates_fail_without_advancing_anchor() {
    // 1. Complete no-snapshot initialization before retaining candidates in the store.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let mut state = fixture.build_verifier_service_state();
    state
        .handle_tick()
        .await
        .expect("genesis initialization succeeds");

    // 2. Seed two recovered candidates that both claim seqno 0.
    let update = fixture.produce_update(build_test_state_diff(1));
    let completion_block = make_test_l1_commitment(1);
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(update.commit_txid(), completion_block),
        update.blob().clone(),
    )]);
    // The competing commit txid is deliberately the only difference between
    // the candidates, so transaction ordering cannot select a winner.
    let competing_commit_txid = Txid::from_byte_array([0x42; 32]);
    let duplicate = RecoveredDaBlob::new(
        DaL1Ref::new(competing_commit_txid, completion_block),
        update.blob().clone(),
    );
    fixture.seed_recovered_da(&[duplicate]);
    fixture.process_sau(&update);
    fixture.set_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);

    // 3. Process a tick and show reconstruction rejects both candidates after
    // fetching their one shared OL update.
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports duplicate candidates");
    assert!(!error.is_recoverable());
    assert!(matches!(
        error,
        DaVerifierError::Reconstruct(L1ReconstructionError::BatchSequence(
            BatchSequenceError::DuplicateUpdateSeqNo { update_seq_no }
        )) if update_seq_no == Seqno::zero()
    ));
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 0);
    assert_eq!(
        fixture.account_update_requests(),
        [(fixture.account_id(), Seqno::zero())]
    );
}

#[tokio::test]
async fn test_state_root_divergence_is_fatal_and_leaves_anchor_unchanged() {
    // 1. Complete no-snapshot initialization before retaining a candidate in the store.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let mut state = fixture.build_verifier_service_state();
    state
        .handle_tick()
        .await
        .expect("genesis initialization succeeds");

    // 2. Process the OL update for one state diff and seed candidate 0 with another.
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.process_sau(&update);
    let mut divergent_blob = update.blob().clone();
    divergent_blob.state_diff = build_test_state_diff(2);
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(update.commit_txid(), make_test_l1_commitment(1)),
        divergent_blob,
    )]);
    fixture.set_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);

    // 3. Process a tick and show local verification detects the root mismatch.
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the reconstructed state-root mismatch");
    assert!(!error.is_recoverable());
    assert!(matches!(
        error,
        DaVerifierError::VerifyAccountState(VerifyAccountStateError::VerifyUpdate(
            AccountStateVerificationError::ApplyUpdate {
                update_seq_no,
                source: EeAccountReconstructionError::InnerStateRootMismatch { .. },
            }
        )) if update_seq_no == Seqno::zero()
    ));

    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 0);
    assert_eq!(
        fixture.account_update_requests(),
        [(fixture.account_id(), Seqno::zero())]
    );
}

#[tokio::test]
async fn test_idle_tick_does_not_save_snapshot() {
    // 1. Run the multi-tick scenario through tick 1, which verifies updates 0..=8 and saves them.
    let (fixture, _, mut state) = run_multi_tick_da_scenario_first_tick().await;
    let saved_snapshot = fixture
        .saved_snapshot()
        .expect("tick 1 saves the verified state");
    let commit_lookups_after_setup = fixture.commit_block_lookups();
    let save_attempts_after_setup = fixture.snapshot_save_attempts();

    // 2. Without adding any new Bitcoin or OL data, process another tick and show the
    // unchanged verified anchor is not saved again.
    state.handle_tick().await.expect("tick processing succeeds");

    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 9);
    let commit_block_lookups = fixture.commit_block_lookups();
    let new_commit_block_lookups = &commit_block_lookups[commit_lookups_after_setup.len()..];
    assert!(new_commit_block_lookups.is_empty());
    let save_attempts = fixture.snapshot_save_attempts();
    let new_save_attempts = &save_attempts[save_attempts_after_setup.len()..];
    assert!(new_save_attempts.is_empty());
    assert_eq!(
        fixture
            .saved_snapshot()
            .expect("idle tick preserves the saved snapshot"),
        saved_snapshot,
    );
}

#[tokio::test]
async fn test_finalized_ol_frontier_saves_only_attested_prefix() {
    // 1. Run the multi-tick scenario through tick 1 and retain its durable snapshot.
    let (mut fixture, scenario, mut state) = run_multi_tick_da_scenario_first_tick().await;
    let previous_snapshot = fixture
        .saved_snapshot()
        .expect("tick 1 saves the verified state");
    let commit_lookups_after_setup = fixture.commit_block_lookups();
    let save_attempts_after_setup = fixture.snapshot_save_attempts();

    // 2. Complete updates 9 and 10 on L1, but finalize only update 9 on OL.
    let update_9_completion_block = fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT,
        [&scenario.updates[9].reveal_transactions()[1]],
    );
    fixture.process_sau(&scenario.updates[9]);
    let update_10 = fixture.produce_update(build_test_state_diff(11));
    fixture.publish_da(&update_10);
    fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT + 1,
        [update_10.commit_transaction()],
    );
    let update_10_completion_block = fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT + 2,
        update_10.reveal_transactions(),
    );
    fixture.set_bitcoin_tip_height(MULTI_TICK_SECOND_TICK_TIP);
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show the snapshot advances only through OL-attested update 9. Pruning removes
    // absorbed history while retaining unverified update 10 at the boundary.
    assert_one_recovered_blob(
        &fixture.recovered_da_blobs(),
        &update_10,
        update_10_completion_block,
    );
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 10);
    let commit_block_lookups = fixture.commit_block_lookups();
    let new_commit_block_lookups = &commit_block_lookups[commit_lookups_after_setup.len()..];
    assert_eq!(
        new_commit_block_lookups,
        [scenario.updates[9].commit_txid()]
    );
    let save_attempts = fixture.snapshot_save_attempts();
    let new_save_attempts = &save_attempts[save_attempts_after_setup.len()..];
    assert_eq!(new_save_attempts.len(), 1);
    assert_eq!(
        new_save_attempts[0].outcome(),
        SnapshotSaveOutcome::Succeeded
    );
    let snapshot = new_save_attempts[0].snapshot();
    assert_eq!(
        snapshot.replay_snapshot.next_update_seq_no(),
        Seqno::new(10)
    );
    assert_ne!(snapshot, &previous_snapshot);
    assert_eq!(snapshot.completion_block, update_9_completion_block);
}

#[tokio::test]
async fn test_catch_up_saves_snapshot_before_scanning_next_l1_window() {
    // 1. Put two OL-attested updates in the first of two three-block L1 scan windows.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_max_l1_scan_window_size(TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE);
    let updates = produce_test_updates(&mut fixture, 3);
    for update in &updates[..2] {
        fixture.publish_da(update);
    }
    fixture.mine_block(
        TEST_GENESIS_L1_HEIGHT,
        [
            updates[0].commit_transaction(),
            updates[1].commit_transaction(),
        ],
    );
    fixture.mine_block(
        TEST_GENESIS_L1_HEIGHT + 1,
        updates[..2]
            .iter()
            .flat_map(TestEeUpdate::reveal_transactions),
    );
    // Keep OL one update ahead of recovered DA so catch-up continues after the first window.
    for update in &updates {
        fixture.process_sau(update);
    }
    let reorg_safe_tip = TEST_GENESIS_L1_HEIGHT + 5;
    fixture.set_bitcoin_tip_height(reorg_safe_tip + TEST_L1_REORG_SAFE_DEPTH);
    let mut state = fixture.build_verifier_service_state();

    // 2. Catch up through both scan windows in one tick.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show the first window performs one prefix read and one final snapshot save before
    // recovery requests the second L1 range. The empty second window saves nothing.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 2);
    assert_eq!(
        fixture.recovered_da_read_requests(),
        [(0, TEST_GENESIS_L1_HEIGHT + 2), (2, reorg_safe_tip),]
    );
    assert_eq!(fixture.commit_block_lookups(), [updates[1].commit_txid()]);
    let attempts = fixture.snapshot_save_attempts();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome(), SnapshotSaveOutcome::Succeeded);

    let expected_operations = [
        ContextOperation::LoadSnapshot,
        ContextOperation::ClearRecoveredDa,
        ContextOperation::FetchBitcoinTip,
        ContextOperation::FetchL1BlockRange {
            start_height: TEST_GENESIS_L1_HEIGHT,
            end_height: TEST_GENESIS_L1_HEIGHT + 2,
        },
        ContextOperation::PutRecoveredDa,
        ContextOperation::GetContiguousRecoveredDa {
            first_update_seq_no: 0,
            recovered_l1_frontier: TEST_GENESIS_L1_HEIGHT + 2,
        },
        ContextOperation::FetchFinalizedAccountState {
            account_id: fixture.account_id(),
        },
        ContextOperation::FetchAccountUpdate {
            account_id: fixture.account_id(),
            update_seq_no: Seqno::zero(),
        },
        ContextOperation::FetchAccountUpdate {
            account_id: fixture.account_id(),
            update_seq_no: Seqno::new(1),
        },
        ContextOperation::FetchCommitBlock {
            commit_txid: updates[1].commit_txid(),
        },
        ContextOperation::SaveSnapshot {
            resume_l1_block: attempts[0].snapshot().resume_l1_block,
            completion_block: attempts[0].snapshot().completion_block,
        },
        ContextOperation::PruneRecoveredDa { update_seq_no: 2 },
        ContextOperation::FetchL1BlockRange {
            start_height: TEST_GENESIS_L1_HEIGHT + 3,
            end_height: reorg_safe_tip,
        },
    ];
    assert_eq!(
        fixture
            .context_operations()
            .into_iter()
            .take(expected_operations.len())
            .collect::<Vec<_>>(),
        expected_operations
    );
}

#[tokio::test]
async fn test_snapshot_save_prunes_absorbed_candidates() {
    // 1. Recover candidates 0 through 2, but finalize only updates 0 and 1 on OL.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let updates = produce_test_updates(&mut fixture, 3);
    let completion_blocks = updates
        .iter()
        .enumerate()
        .map(|(index, update)| {
            fixture.mine_and_seed_recovered_da_candidate(
                update,
                TEST_GENESIS_L1_HEIGHT
                    + u32::try_from(index).expect("test update index fits in L1 height"),
            )
        })
        .collect::<Vec<_>>();
    for update in &updates[..2] {
        fixture.process_sau(update);
    }
    fixture.set_bitcoin_tip_height(
        completion_blocks[2].height() + L1Height::from(TEST_L1_REORG_SAFE_DEPTH),
    );
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick, which verifies and saves through update 1.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show pruning runs after the durable save, removes sequences below 2,
    // and retains candidate 2 at the first unapplied boundary.
    let snapshot = fixture
        .saved_snapshot()
        .expect("verified prefix saves a snapshot");
    assert_eq!(snapshot.replay_snapshot.next_update_seq_no(), Seqno::new(2));
    assert_one_recovered_blob(
        &fixture.recovered_da_blobs(),
        &updates[2],
        completion_blocks[2],
    );
    let persistence_operations = fixture
        .context_operations()
        .into_iter()
        .filter(|operation| {
            matches!(
                operation,
                ContextOperation::SaveSnapshot { .. } | ContextOperation::PruneRecoveredDa { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        persistence_operations,
        [
            ContextOperation::SaveSnapshot {
                resume_l1_block: snapshot.resume_l1_block,
                completion_block: snapshot.completion_block,
            },
            ContextOperation::PruneRecoveredDa { update_seq_no: 2 },
        ]
    );
}

#[tokio::test]
async fn test_unverified_window_does_not_prune() {
    // 1. Recover candidate 0 without finalizing its OL update.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let update = fixture.produce_update(build_test_state_diff(1));
    let completion_block =
        fixture.mine_and_seed_recovered_da_candidate(&update, TEST_GENESIS_L1_HEIGHT);
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one window without advancing the verified anchor.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show the candidate remains and no snapshot or prune operation occurs.
    assert_eq!(state.next_update_seq_no(), 0);
    assert!(fixture.saved_snapshot().is_none());
    assert_one_recovered_blob(&fixture.recovered_da_blobs(), &update, completion_block);
    assert!(fixture.context_operations().into_iter().all(|operation| {
        !matches!(
            operation,
            ContextOperation::SaveSnapshot { .. } | ContextOperation::PruneRecoveredDa { .. }
        )
    }));
}

#[tokio::test]
async fn test_failed_prune_does_not_fail_the_tick() {
    // 1. Make candidate 0 verifiable, then make its post-save prune fail.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let update = fixture.produce_update(build_test_state_diff(1));
    fixture.process_sau(&update);
    let completion_block =
        fixture.mine_and_seed_recovered_da_candidate(&update, TEST_GENESIS_L1_HEIGHT);
    fixture.set_bitcoin_tip_height(TEST_GENESIS_L1_HEIGHT + TEST_L1_REORG_SAFE_DEPTH);
    fixture.fail_next_recovered_da_prune(RecoveredDaDbError::WorkerCancelled);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process one tick and show the prune failure is non-fatal.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show the verified snapshot remains durable and the unpruned candidate remains stored.
    assert_eq!(state.next_update_seq_no(), 1);
    let snapshot = fixture
        .saved_snapshot()
        .expect("successful save remains durable after prune failure");
    assert_eq!(snapshot.replay_snapshot.next_update_seq_no(), Seqno::new(1));
    assert_one_recovered_blob(&fixture.recovered_da_blobs(), &update, completion_block);
    let persistence_operations = fixture
        .context_operations()
        .into_iter()
        .filter(|operation| {
            matches!(
                operation,
                ContextOperation::SaveSnapshot { .. } | ContextOperation::PruneRecoveredDa { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        persistence_operations,
        [
            ContextOperation::SaveSnapshot {
                resume_l1_block: snapshot.resume_l1_block,
                completion_block: snapshot.completion_block,
            },
            ContextOperation::PruneRecoveredDa { update_seq_no: 1 },
        ]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_snapshot_uses_final_update_commit_and_highest_completion() {
    // 1. Produce and publish matching updates 0 and 1 for one L1 scan window.
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    );
    let updates = produce_test_updates(&mut fixture, 2);
    for update in &updates {
        fixture.publish_da(update);
    }

    // 2. Mine update 1's commit first, but complete update 0 last. Update 1 is therefore
    // the final applied update, while update 0 has the highest completion block.
    let update_1_commit_block =
        fixture.mine_block(TEST_GENESIS_L1_HEIGHT, [updates[1].commit_transaction()]);
    fixture.mine_block(
        TEST_GENESIS_L1_HEIGHT + 1,
        [updates[0].commit_transaction()],
    );
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 2, updates[1].reveal_transactions());
    let update_0_completion_block =
        fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 3, updates[0].reveal_transactions());
    fixture.set_bitcoin_tip_height(update_0_completion_block.height() + TEST_L1_REORG_SAFE_DEPTH);

    // 3. After both updates confirm on L1, make their matching OL account updates available.
    for update in &updates {
        fixture.process_sau(update);
    }
    let mut state = fixture.build_verifier_service_state();

    // 4. Process both updates in one verifier tick.
    state.handle_tick().await.expect("tick processing succeeds");

    // 5. Show the snapshot resumes from update 1's commit but covers L1 through update 0's
    // later completion block, with both reconstructed state halves ending at update 1.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 2);
    assert_eq!(fixture.commit_block_lookups(), [updates[1].commit_txid()]);
    let attempts = fixture.snapshot_save_attempts();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome(), SnapshotSaveOutcome::Succeeded);
    let snapshot = attempts[0].snapshot();
    assert_eq!(snapshot.resume_l1_block, update_1_commit_block);
    assert_eq!(snapshot.completion_block, update_0_completion_block);
    assert_eq!(snapshot.replay_snapshot.next_update_seq_no(), Seqno::new(2));
    assert_eq!(
        snapshot.replay_snapshot.last_applied_block_num(),
        updates[1].blob().evm_header.block_num
    );
    assert_stored_snapshot_is_consistent(snapshot);
}

#[tokio::test]
async fn test_recoverable_commit_lookup_failure_retries_pending_snapshot() {
    // 1. Save updates 0..=8, then complete update 9's DA and make its matching OL account
    // update available.
    let (fixture, scenario, mut state) = run_multi_tick_da_scenario_first_tick().await;
    let previous_snapshot = fixture
        .saved_snapshot()
        .expect("tick 1 saves the verified state");
    let commit_lookups_after_setup = fixture.commit_block_lookups();
    let save_attempts_after_setup = fixture.snapshot_save_attempts();
    make_multi_tick_update_9_available_for_second_tick(&fixture, &scenario);
    fixture.fail_next_commit_block_lookup(FetchCommitBlockError::FetchTransaction {
        txid: scenario.updates[9].commit_txid(),
        source: ClientError::Timeout,
    });

    // 2. Process tick 2 and show verification advances while the prior snapshot remains durable.
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the commit-block lookup timeout");
    assert!(matches!(
        error,
        DaVerifierError::FetchCommitBlock(FetchCommitBlockError::FetchTransaction {
            txid,
            source: ClientError::Timeout,
        }) if txid == scenario.updates[9].commit_txid()
    ));
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 10);
    let save_attempts = fixture.snapshot_save_attempts();
    let new_save_attempts = &save_attempts[save_attempts_after_setup.len()..];
    assert!(new_save_attempts.is_empty());
    assert_eq!(
        fixture
            .saved_snapshot()
            .expect("failed lookup preserves the prior snapshot"),
        previous_snapshot,
    );
    let ol_requests_before_retry = fixture.account_update_requests();

    // 3. Without adding Bitcoin or OL data, process tick 3 and show it retries the same
    // lookup without repeating OL verification.
    state.handle_tick().await.expect("tick processing succeeds");

    let commit_block_lookups = fixture.commit_block_lookups();
    let new_commit_block_lookups = &commit_block_lookups[commit_lookups_after_setup.len()..];
    assert_eq!(
        new_commit_block_lookups,
        [
            scenario.updates[9].commit_txid(),
            scenario.updates[9].commit_txid(),
        ]
    );
    assert_eq!(fixture.account_update_requests(), ol_requests_before_retry);
    let save_attempts = fixture.snapshot_save_attempts();
    let new_save_attempts = &save_attempts[save_attempts_after_setup.len()..];
    assert_eq!(new_save_attempts.len(), 1);
    assert_eq!(
        new_save_attempts[0].outcome(),
        SnapshotSaveOutcome::Succeeded
    );
    assert_eq!(
        new_save_attempts[0]
            .snapshot()
            .replay_snapshot
            .next_update_seq_no(),
        Seqno::new(10)
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_pending_snapshot_is_retried_before_lagging_ol_frontier_error() {
    // 1. Save updates 0..=8, make update 9 verifiable, and recover unfinalized update 10.
    let (mut fixture, scenario, mut state) = run_multi_tick_da_scenario_first_tick().await;
    let previous_snapshot = fixture
        .saved_snapshot()
        .expect("tick 1 saves the verified state");
    let commit_lookups_after_setup = fixture.commit_block_lookups();
    let save_attempts_after_setup = fixture.snapshot_save_attempts();
    make_multi_tick_update_9_available_for_second_tick(&fixture, &scenario);
    let update_10 = fixture.produce_update(build_test_state_diff(11));
    fixture.publish_da(&update_10);
    fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT + 1,
        [update_10.commit_transaction()],
    );
    fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT + 2,
        update_10.reveal_transactions(),
    );
    fixture.fail_next_snapshot_save(SnapshotSaveError::Io {
        operation: "test snapshot operation",
        path: PathBuf::from("reconstruction.snapshot"),
        source: io::Error::other("disk unavailable"),
    });

    // 2. Process tick 2 and show the failed save does not replace the prior snapshot.
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the snapshot write failure");
    assert!(matches!(
        error,
        DaVerifierError::SaveSnapshot(SnapshotSaveError::Io { .. })
    ));
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 10);
    assert_eq!(
        fixture
            .saved_snapshot()
            .expect("failed save preserves the prior snapshot"),
        previous_snapshot,
    );
    let failed_snapshot = {
        let save_attempts = fixture.snapshot_save_attempts();
        let new_save_attempts = &save_attempts[save_attempts_after_setup.len()..];
        assert_eq!(new_save_attempts.len(), 1);
        assert_eq!(new_save_attempts[0].outcome(), SnapshotSaveOutcome::Failed);
        new_save_attempts[0].snapshot().clone()
    };
    let ol_requests_before_retry = fixture.account_update_requests();

    // 3. Make the next finalized OL view lag behind the verified anchor. Tick 3 must retry
    // the pending snapshot before reporting that recoverable frontier error.
    fixture.override_next_finalized_frontier(Ok(Seqno::new(9)));
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the lagging finalized OL frontier");
    assert!(matches!(
        error,
        DaVerifierError::LaggingFinalizedOLFrontier {
            anchor_next_update_seq_no,
            finalized_next_update_seq_no,
        } if anchor_next_update_seq_no == Seqno::new(10)
            && finalized_next_update_seq_no == Seqno::new(9)
    ));
    assert!(error.is_recoverable());

    let save_attempts = fixture.snapshot_save_attempts();
    let new_save_attempts = &save_attempts[save_attempts_after_setup.len()..];
    assert_eq!(new_save_attempts.len(), 2);
    assert_eq!(new_save_attempts[0].outcome(), SnapshotSaveOutcome::Failed);
    assert_eq!(
        new_save_attempts[1].outcome(),
        SnapshotSaveOutcome::Succeeded
    );
    assert_eq!(new_save_attempts[1].snapshot(), &failed_snapshot);
    assert_eq!(
        fixture
            .saved_snapshot()
            .expect("successful retry replaces the prior snapshot"),
        failed_snapshot,
    );
    assert_eq!(fixture.account_update_requests(), ol_requests_before_retry);
    // Retrying a pending save re-resolves its resume commitment before writing, even though
    // the previous lookup succeeded and only the snapshot write failed.
    let commit_block_lookups = fixture.commit_block_lookups();
    let new_commit_block_lookups = &commit_block_lookups[commit_lookups_after_setup.len()..];
    assert_eq!(
        new_commit_block_lookups,
        [
            scenario.updates[9].commit_txid(),
            scenario.updates[9].commit_txid(),
        ]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_fatal_snapshot_failure_does_not_replace_snapshot() {
    // 1. Save updates 0..=8, then complete update 9's DA and make its matching OL account
    // update available.
    let (fixture, scenario, mut state) = run_multi_tick_da_scenario_first_tick().await;
    let previous_snapshot = fixture
        .saved_snapshot()
        .expect("tick 1 saves the verified state");
    let save_attempts_after_setup = fixture.snapshot_save_attempts();
    make_multi_tick_update_9_available_for_second_tick(&fixture, &scenario);
    fixture.fail_next_snapshot_save(SnapshotSaveError::Validation(
        SnapshotValidationError::StateRootMismatch,
    ));

    // 2. Process tick 2 and show the fatal save leaves the prior snapshot durable.
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the invalid snapshot state");
    assert!(!error.is_recoverable());
    assert!(matches!(
        error,
        DaVerifierError::SaveSnapshot(SnapshotSaveError::Validation(
            SnapshotValidationError::StateRootMismatch
        ))
    ));
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_update_seq_no, 10);
    let save_attempts = fixture.snapshot_save_attempts();
    let new_save_attempts = &save_attempts[save_attempts_after_setup.len()..];
    assert_eq!(new_save_attempts.len(), 1);
    assert_eq!(new_save_attempts[0].outcome(), SnapshotSaveOutcome::Failed);
    assert_eq!(
        fixture
            .saved_snapshot()
            .expect("fatal save preserves the prior snapshot"),
        previous_snapshot,
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_missing_snapshot_initializes_without_canonicality_checks() {
    // 1. Leave the snapshot store empty and expose a reorg-safe range beginning at genesis.
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_REORG_SAFE_TIP + TEST_L1_REORG_SAFE_DEPTH);
    let mut state = fixture.build_verifier_service_state();

    // 2. Process the first tick without persisted verification progress.
    state.handle_tick().await.expect("tick processing succeeds");

    // 3. Show initialization loads once, performs no canonicality checks, and scans from genesis.
    assert_eq!(fixture.snapshot_load_count(), 1);
    assert!(fixture.l1_block_canonicality_checks().is_empty());
    assert_eq!(
        fixture.recovery_operations(),
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: TEST_GENESIS_L1_HEIGHT,
                end_height: TEST_REORG_SAFE_TIP,
            },
        ]
    );
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_l1_height, TEST_REORG_SAFE_TIP + 1);
    assert_eq!(status.reorg_safe_tip, Some(TEST_REORG_SAFE_TIP));
    assert_eq!(status.next_update_seq_no, 0);
}

#[tokio::test]
async fn test_restart_resumes_from_snapshot_boundary() {
    // 1. Run tick 1 to verify and save through update 8, record the observations so far, then
    // drop the service state. The fixture keeps Bitcoin, candidates, OL updates, and the
    // snapshot, so the restart below sees exactly what a restarted process would.
    let (fixture, _scenario, state) = run_multi_tick_da_scenario_first_tick().await;
    let snapshot = fixture
        .saved_snapshot()
        .expect("the first tick saves verified progress");
    let recovery_operations_after_setup = fixture.recovery_operations();
    let recovered_da_reads_after_setup = fixture.recovered_da_read_requests();
    let snapshot_loads_after_setup = fixture.snapshot_load_count();
    drop(state);

    // 2. Restart over the same stores and process the first resumed tick.
    let mut restarted_state = fixture.restart_verifier_service_state();
    restarted_state
        .handle_tick()
        .await
        .expect("resumed tick processing succeeds");

    // 3. Show the restored frontier drives an inclusive rescan and verification at seqno 9.
    assert_eq!(
        fixture.snapshot_load_count() - snapshot_loads_after_setup,
        1
    );
    let recovery_operations = fixture.recovery_operations();
    assert_eq!(
        &recovery_operations[recovery_operations_after_setup.len()..],
        [
            ContextOperation::FetchBitcoinTip,
            ContextOperation::FetchL1BlockRange {
                start_height: snapshot.resume_l1_block.height(),
                end_height: MULTI_TICK_FIRST_TICK_SAFE_TIP,
            },
            ContextOperation::PutRecoveredDa,
        ]
    );
    let recovered_da_reads = fixture.recovered_da_read_requests();
    assert_eq!(
        &recovered_da_reads[recovered_da_reads_after_setup.len()..],
        [(9, MULTI_TICK_FIRST_TICK_SAFE_TIP)]
    );
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&restarted_state);
    assert_eq!(status.next_l1_height, MULTI_TICK_FIRST_TICK_SAFE_TIP + 1);
    assert_eq!(status.next_update_seq_no, 9);
}

#[tokio::test]
async fn test_snapshot_resume_preserves_recovered_da() {
    // 1. Save through update 8, then retain completed update 9 above the saved L1 frontier.
    let (fixture, scenario, state) = run_multi_tick_da_scenario_first_tick().await;
    let completion_block = fixture.mine_block(
        MULTI_TICK_UPDATE_9_COMPLETION_HEIGHT,
        [&scenario.updates[9].reveal_transactions()[1]],
    );
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(scenario.updates[9].commit_txid(), completion_block),
        scenario.updates[9].blob().clone(),
    )]);
    let operations_after_setup = fixture.context_operations();
    drop(state);

    // 2. Restart from the valid snapshot without making update 9 available on OL.
    let mut restarted_state = fixture.restart_verifier_service_state();
    restarted_state
        .handle_tick()
        .await
        .expect("resumed tick processing succeeds");

    // 3. Show initialization did not clear the candidate retained above the snapshot boundary.
    assert!(fixture
        .context_operations()
        .iter()
        .skip(operations_after_setup.len())
        .all(|operation| !matches!(operation, ContextOperation::ClearRecoveredDa)));
    assert!(fixture.recovered_da_blobs().iter().any(|recovered_blob| {
        recovered_blob.blob().update_seq_no == scenario.updates[9].blob().update_seq_no
            && recovered_blob.l1_ref().completion_block() == completion_block
    }));
}

#[tokio::test]
async fn test_restart_after_interrupted_catch_up_resumes_from_latest_verified_commit() {
    // 1. Put update 0 in the first scan window, then fail recovery at the start of the second.
    let failed_height = TEST_GENESIS_L1_HEIGHT + 3;
    let mut fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_max_l1_scan_window_size(TEST_THREE_BLOCK_L1_SCAN_WINDOW_SIZE);
    let updates = produce_test_updates(&mut fixture, 2);
    let update = &updates[0];
    fixture.publish_da(update);
    let commit_block =
        fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 1, [update.commit_transaction()]);
    fixture.mine_block(TEST_GENESIS_L1_HEIGHT + 2, update.reveal_transactions());
    // Keep OL one update ahead so catch-up reaches the injected second-window failure.
    for update in &updates {
        fixture.process_sau(update);
    }
    let reorg_safe_tip = TEST_GENESIS_L1_HEIGHT + 5;
    fixture.set_bitcoin_tip_height(reorg_safe_tip + TEST_L1_REORG_SAFE_DEPTH);
    fixture.fail_block_fetch_at(
        failed_height,
        FetchBlockError::RetriesExhausted {
            height: failed_height,
            max_retries: 3,
            source: ClientError::Timeout,
        },
    );
    let mut state = fixture.build_verifier_service_state();

    // 2. Catch up until the failure and retain the snapshot saved after the first window.
    state
        .handle_tick()
        .await
        .expect("recoverable second-window failure ends the tick successfully");
    let snapshot = fixture
        .saved_snapshot()
        .expect("the verified first window saves a snapshot");
    assert_eq!(snapshot.resume_l1_block, commit_block);
    assert_eq!(state.next_l1_height(), failed_height);
    fixture.assert_injected_behavior_consumed();
    let operations_after_interrupted_tick = fixture.context_operations();
    drop(state);

    // 3. Restart and show recovery begins inclusively at the durable commit boundary.
    let mut restarted_state = fixture.restart_verifier_service_state();
    restarted_state
        .handle_tick()
        .await
        .expect("resumed catch-up succeeds");
    let operations = fixture.context_operations();
    let first_resumed_range = operations[operations_after_interrupted_tick.len()..]
        .iter()
        .find_map(|operation| match operation {
            ContextOperation::FetchL1BlockRange {
                start_height,
                end_height,
            } => Some((*start_height, *end_height)),
            _ => None,
        })
        .expect("resumed tick requests an L1 range");
    assert_eq!(first_resumed_range.0, snapshot.resume_l1_block.height());
    assert_ne!(first_resumed_range.0, TEST_GENESIS_L1_HEIGHT);
    assert_ne!(first_resumed_range.0, failed_height);
}

#[tokio::test]
async fn test_snapshot_initialization_checks_both_l1_boundaries_before_bitcoin_da_scan() {
    // 1. Save a real snapshot whose resume and highest completion blocks differ.
    let fixture = build_fixture_with_distinct_snapshot_boundaries().await;
    let snapshot = fixture
        .saved_snapshot()
        .expect("the setup saves a distinct-boundary snapshot");
    assert_ne!(snapshot.resume_l1_block, snapshot.completion_block);
    let operations_after_setup = fixture.context_operations();

    // 2. Restart over the same world and process the first resumed tick.
    let mut state = fixture.restart_verifier_service_state();
    state
        .handle_tick()
        .await
        .expect("resumed tick processing succeeds");

    // 3. Show both exact boundaries are checked before the Bitcoin DA scan fetches the tip.
    // Boundary-check order is deliberately irrelevant; both checks must precede the tip fetch.
    let operations = fixture.context_operations();
    let resumed_operations = &operations[operations_after_setup.len()..];
    let initialization_operations = resumed_operations
        .iter()
        .take(4)
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(
        initialization_operations.len(),
        4,
        "initialization operations: {initialization_operations:?}"
    );
    assert_eq!(initialization_operations[0], ContextOperation::LoadSnapshot);
    assert_eq!(
        initialization_operations[3],
        ContextOperation::FetchBitcoinTip
    );
    let mut checked_boundaries = initialization_operations[1..3]
        .iter()
        .filter_map(|operation| match operation {
            ContextOperation::CheckL1BlockCanonicality { commitment } => Some(*commitment),
            _ => None,
        })
        .collect::<Vec<_>>();
    checked_boundaries.sort_by_key(L1BlockCommitment::height);
    let mut expected_boundaries = vec![snapshot.resume_l1_block, snapshot.completion_block];
    expected_boundaries.sort_by_key(L1BlockCommitment::height);
    assert_eq!(checked_boundaries, expected_boundaries);
}

#[tokio::test]
async fn test_recoverable_canonicality_failure_retries_snapshot_initialization() {
    // 1. Save updates 0..=8, restart, and fail the first resume-block canonicality check.
    let (fixture, _scenario, state) = run_multi_tick_da_scenario_first_tick().await;
    let snapshot = fixture
        .saved_snapshot()
        .expect("the first tick saves verified progress");
    let snapshot_loads_after_setup = fixture.snapshot_load_count();
    let canonicality_checks_after_setup = fixture.l1_block_canonicality_checks();
    let operations_after_setup = fixture.context_operations();
    drop(state);
    fixture.fail_next_l1_block_canonicality_check(CheckL1BlockError::FetchBlockHash {
        expected: snapshot.resume_l1_block,
        source: ClientError::Timeout,
    });
    let mut restarted_state = fixture.restart_verifier_service_state();

    // 2. Process one tick and show the recoverable failure installs no snapshot state.
    let error = restarted_state
        .handle_tick()
        .await
        .expect_err("tick processing reports the canonicality lookup failure");
    assert!(error.is_recoverable());
    assert!(matches!(
        error,
        DaVerifierError::CheckL1Block(CheckL1BlockError::FetchBlockHash {
            expected,
            source: ClientError::Timeout,
        }) if expected == snapshot.resume_l1_block
    ));
    assert_service_uninitialized(&restarted_state, MULTI_TICK_GENESIS_L1_HEIGHT);
    let operations = fixture.context_operations();
    assert_eq!(
        &operations[operations_after_setup.len()..],
        [
            ContextOperation::LoadSnapshot,
            ContextOperation::CheckL1BlockCanonicality {
                commitment: snapshot.resume_l1_block,
            },
        ]
    );

    // 3. Process the next tick and show initialization reloads the snapshot and repeats boundary
    // validation from the resume block.
    restarted_state
        .handle_tick()
        .await
        .expect("retried tick processing succeeds");
    assert_eq!(
        fixture.snapshot_load_count() - snapshot_loads_after_setup,
        2
    );
    let canonicality_checks = fixture.l1_block_canonicality_checks();
    assert_eq!(
        &canonicality_checks[canonicality_checks_after_setup.len()..],
        [
            snapshot.resume_l1_block,
            snapshot.resume_l1_block,
            snapshot.completion_block,
        ]
    );
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&restarted_state);
    assert_eq!(status.next_l1_height, MULTI_TICK_FIRST_TICK_SAFE_TIP + 1);
    assert_eq!(status.next_update_seq_no, 9);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_snapshot_initialization_runs_once() {
    // 1. Save updates 0..=8, discard service state, and capture initialization observations.
    let (fixture, _scenario, state) = run_multi_tick_da_scenario_first_tick().await;
    let snapshot = fixture
        .saved_snapshot()
        .expect("the first tick saves verified progress");
    let snapshot_loads_after_setup = fixture.snapshot_load_count();
    let canonicality_checks_after_setup = fixture.l1_block_canonicality_checks();
    let operations_after_setup = fixture.context_operations();
    drop(state);

    // 2. Restart and process two ticks without adding Bitcoin or OL data between them.
    let mut restarted_state = fixture.restart_verifier_service_state();
    restarted_state
        .handle_tick()
        .await
        .expect("first resumed tick processing succeeds");
    restarted_state
        .handle_tick()
        .await
        .expect("second resumed tick processing succeeds");

    // 3. Show snapshot loading and boundary validation happened only on the first tick.
    assert_eq!(
        fixture.snapshot_load_count() - snapshot_loads_after_setup,
        1
    );
    let canonicality_checks = fixture.l1_block_canonicality_checks();
    // The resume and completion roles happen to reference block 140 in this scenario; retaining
    // both entries proves initialization checked each role once.
    assert_eq!(
        &canonicality_checks[canonicality_checks_after_setup.len()..],
        [snapshot.resume_l1_block, snapshot.completion_block]
    );
    let operations = fixture.context_operations();
    let resumed_operations = &operations[operations_after_setup.len()..];
    assert_eq!(
        resumed_operations
            .iter()
            .filter(|operation| matches!(operation, ContextOperation::FetchBitcoinTip))
            .count(),
        2,
        "both resumed ticks must reach Bitcoin tip retrieval"
    );
}

#[tokio::test]
async fn test_snapshot_decode_failure_leaves_service_uninitialized() {
    // 1. Process a fresh service state's first tick with a typed snapshot decode failure.
    let (fixture, state, error) =
        run_first_tick_with_snapshot_load_failure(SnapshotLoadError::Decode {
            path: PathBuf::from("reconstruction.snapshot"),
            source: eyre::eyre!("invalid snapshot bytes"),
        })
        .await;

    // 2. Confirm initialization reports the exact fatal load error.
    assert!(!error.is_recoverable());
    assert!(matches!(
        error,
        DaVerifierError::LoadSnapshot(SnapshotLoadError::Decode { .. })
    ));

    // 3. Show the service remains uninitialized and performs no work after snapshot loading.
    assert_service_uninitialized(&state, TEST_GENESIS_L1_HEIGHT);
    assert_eq!(
        fixture.context_operations(),
        [ContextOperation::LoadSnapshot]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_snapshot_validation_failure_leaves_service_uninitialized() {
    // 1. Process a fresh service state's first tick with a typed snapshot validation failure.
    let (fixture, state, error) = run_first_tick_with_snapshot_load_failure(
        SnapshotLoadError::Validation(SnapshotValidationError::InnerStateRootMismatch),
    )
    .await;

    // 2. Confirm initialization reports the exact fatal load error.
    assert!(!error.is_recoverable());
    assert!(matches!(
        error,
        DaVerifierError::LoadSnapshot(SnapshotLoadError::Validation(
            SnapshotValidationError::InnerStateRootMismatch
        ))
    ));

    // 3. Show the service remains uninitialized and performs no work after snapshot loading.
    assert_service_uninitialized(&state, TEST_GENESIS_L1_HEIGHT);
    assert_eq!(
        fixture.context_operations(),
        [ContextOperation::LoadSnapshot]
    );
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_snapshot_before_genesis_restarts_from_genesis() {
    // 1. Save valid progress, retain stale DA, then move the snapshot resume below genesis.
    let (mut fixture, _scenario, state) = run_multi_tick_da_scenario_first_tick().await;
    let mut snapshot = fixture
        .saved_snapshot()
        .expect("the first tick saves verified progress");
    snapshot.resume_l1_block = L1BlockCommitment::new(
        MULTI_TICK_GENESIS_L1_HEIGHT - 1,
        L1BlockId::from(Buf32::from([0xA5; 32])),
    );
    fixture.store_snapshot_for_restart(snapshot.clone());
    let retained_update = fixture.produce_update(build_test_state_diff(3));
    let retained_completion_block = L1BlockCommitment::new(
        snapshot.completion_block.height() + 1,
        L1BlockId::from(Buf32::from([0xA6; 32])),
    );
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(retained_update.commit_txid(), retained_completion_block),
        retained_update.blob().clone(),
    )]);
    let operations_after_setup = fixture.context_operations();
    fixture.fail_next_bitcoin_tip(FetchBitcoinTipError::Rpc(ClientError::Timeout));
    drop(state);

    // 2. Restart and let initialization discard the incompatible state before recovery continues.
    let mut state = fixture.restart_verifier_service_state();
    state
        .handle_tick()
        .await
        .expect("snapshot below genesis restarts verification from genesis");

    // 3. Show no canonicality check runs and stale state is removed before the Bitcoin tip read.
    let operations = fixture.context_operations();
    assert_eq!(
        &operations[operations_after_setup.len()..],
        [
            ContextOperation::LoadSnapshot,
            ContextOperation::DeleteSnapshot,
            ContextOperation::ClearRecoveredDa,
            ContextOperation::FetchBitcoinTip,
        ]
    );
    assert!(fixture.recovered_da_blobs().is_empty());
    assert_eq!(fixture.saved_snapshot(), None);
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_l1_height, MULTI_TICK_GENESIS_L1_HEIGHT);
    assert_eq!(status.next_update_seq_no, 0);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_snapshot_delete_failure_stops_reorg_reset() {
    // 1. Save a snapshot, reorg its resume block, and make stale-snapshot deletion fail.
    let fixture = build_fixture_with_distinct_snapshot_boundaries().await;
    let snapshot = fixture
        .saved_snapshot()
        .expect("the setup saves a distinct-boundary snapshot");
    let operations_after_setup = fixture.context_operations();
    fixture.replace_bitcoin_block(snapshot.resume_l1_block.height(), 1);
    fixture.fail_next_snapshot_delete(SnapshotDeleteError::Io {
        operation: "remove reconstruction snapshot",
        path: PathBuf::from("snapshot"),
        source: io::Error::other("test snapshot delete failure"),
    });

    // 2. Restart and show deletion fails before recovered DA is cleared.
    let mut state = fixture.restart_verifier_service_state();
    let error = state
        .handle_tick()
        .await
        .expect_err("tick processing reports the snapshot delete failure");
    assert!(error.is_recoverable());
    assert!(matches!(error, DaVerifierError::DeleteSnapshot(_)));

    // 3. Show initialization remains pending and the stale snapshot remains available to retry.
    assert_service_uninitialized(&state, TEST_GENESIS_L1_HEIGHT);
    let operations = fixture.context_operations();
    assert_eq!(
        &operations[operations_after_setup.len()..],
        [
            ContextOperation::LoadSnapshot,
            ContextOperation::CheckL1BlockCanonicality {
                commitment: snapshot.resume_l1_block,
            },
            ContextOperation::DeleteSnapshot,
        ]
    );
    assert_eq!(fixture.saved_snapshot(), Some(snapshot));
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_noncanonical_resume_block_restarts_from_genesis() {
    // 1. Save a distinct-boundary snapshot, retain stale DA, then reorg its resume block.
    let mut fixture = build_fixture_with_distinct_snapshot_boundaries().await;
    let snapshot = fixture
        .saved_snapshot()
        .expect("the setup saves a distinct-boundary snapshot");
    let retained_update = fixture.produce_update(build_test_state_diff(3));
    let retained_completion_block = L1BlockCommitment::new(
        snapshot.completion_block.height() + 1,
        L1BlockId::from(Buf32::from([0xA6; 32])),
    );
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(retained_update.commit_txid(), retained_completion_block),
        retained_update.blob().clone(),
    )]);
    let operations_after_setup = fixture.context_operations();
    fixture.replace_bitcoin_block(snapshot.resume_l1_block.height(), 1);
    fixture.fail_next_bitcoin_tip(FetchBitcoinTipError::Rpc(ClientError::Timeout));

    // 2. Restart and let initialization discard the stale state before recovery continues.
    let mut state = fixture.restart_verifier_service_state();
    state
        .handle_tick()
        .await
        .expect("confirmed snapshot reorg restarts from genesis");

    // 3. Show the snapshot is deleted and candidates are cleared before the Bitcoin tip read.
    let operations = fixture.context_operations();
    assert_eq!(
        &operations[operations_after_setup.len()..],
        [
            ContextOperation::LoadSnapshot,
            ContextOperation::CheckL1BlockCanonicality {
                commitment: snapshot.resume_l1_block,
            },
            ContextOperation::DeleteSnapshot,
            ContextOperation::ClearRecoveredDa,
            ContextOperation::FetchBitcoinTip,
        ]
    );
    assert!(fixture.recovered_da_blobs().is_empty());
    assert_eq!(fixture.saved_snapshot(), None);
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_l1_height, TEST_GENESIS_L1_HEIGHT);
    assert_eq!(status.next_update_seq_no, 0);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_noncanonical_completion_block_restarts_from_genesis() {
    // 1. Save a distinct-boundary snapshot, retain stale DA, then reorg its completion block.
    let mut fixture = build_fixture_with_distinct_snapshot_boundaries().await;
    let snapshot = fixture
        .saved_snapshot()
        .expect("the setup saves a distinct-boundary snapshot");
    let retained_update = fixture.produce_update(build_test_state_diff(3));
    let retained_completion_block = L1BlockCommitment::new(
        snapshot.completion_block.height() + 1,
        L1BlockId::from(Buf32::from([0xA6; 32])),
    );
    fixture.seed_recovered_da(&[RecoveredDaBlob::new(
        DaL1Ref::new(retained_update.commit_txid(), retained_completion_block),
        retained_update.blob().clone(),
    )]);
    let operations_after_setup = fixture.context_operations();
    fixture.replace_bitcoin_block(snapshot.completion_block.height(), 1);
    fixture.fail_next_bitcoin_tip(FetchBitcoinTipError::Rpc(ClientError::Timeout));

    // 2. Restart and let initialization discard the stale state before recovery continues.
    let mut state = fixture.restart_verifier_service_state();
    state
        .handle_tick()
        .await
        .expect("confirmed snapshot reorg restarts from genesis");

    // 3. Show both boundaries are checked before the stale state is deleted and cleared.
    let operations = fixture.context_operations();
    assert_eq!(
        &operations[operations_after_setup.len()..],
        [
            ContextOperation::LoadSnapshot,
            ContextOperation::CheckL1BlockCanonicality {
                commitment: snapshot.resume_l1_block,
            },
            ContextOperation::CheckL1BlockCanonicality {
                commitment: snapshot.completion_block,
            },
            ContextOperation::DeleteSnapshot,
            ContextOperation::ClearRecoveredDa,
            ContextOperation::FetchBitcoinTip,
        ]
    );
    assert!(fixture.recovered_da_blobs().is_empty());
    assert_eq!(fixture.saved_snapshot(), None);
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&state);
    assert_eq!(status.next_l1_height, TEST_GENESIS_L1_HEIGHT);
    assert_eq!(status.next_update_seq_no, 0);
    fixture.assert_injected_behavior_consumed();
}

#[tokio::test]
async fn test_restart_reprocesses_resume_block_idempotently() {
    // 1. Save updates 0..=8, which prunes their candidates, then restart from update 8's block.
    let (fixture, scenario, state) = run_multi_tick_da_scenario_first_tick().await;
    assert!(fixture.recovered_da_blobs().is_empty());
    let writes_after_setup = fixture.recovered_da_write_attempts();
    let snapshot_before_restart = fixture
        .saved_snapshot()
        .expect("the first tick saves verified progress");
    drop(state);
    let mut restarted_state = fixture.restart_verifier_service_state();

    // 2. Process the first resumed tick. The inclusive scan covers update 8's completion and
    // update 9's commit, but the incomplete update 9 envelope emits no blob, so only update 8 is
    // written again.
    restarted_state
        .handle_tick()
        .await
        .expect("resumed tick processing succeeds");

    // 3. Show inclusive recovery emits and restores update 8's pruned candidate.
    let writes = fixture.recovered_da_write_attempts();
    let writes_after_restart = &writes[writes_after_setup.len()..];
    assert_eq!(writes_after_restart.len(), 1);
    assert_eq!(
        writes_after_restart[0].outcome(),
        RecoveredDaWriteOutcome::Succeeded
    );
    assert_one_recovered_blob(
        writes_after_restart[0].blobs(),
        &scenario.updates[8],
        scenario.first_tick_completion_blocks.update_8,
    );
    assert_one_recovered_blob(
        &fixture.recovered_da_blobs(),
        &scenario.updates[8],
        scenario.first_tick_completion_blocks.update_8,
    );
    assert_eq!(fixture.saved_snapshot(), Some(snapshot_before_restart));
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&restarted_state);
    assert_eq!(status.next_update_seq_no, 9);
}

#[tokio::test]
async fn test_restart_rebuilds_cross_block_da_extraction() {
    // 1. Save updates 0..=8 while update 9's commit and first reveal are reorg-safe, then restart.
    let (fixture, scenario, state) = run_multi_tick_da_scenario_first_tick().await;
    let previous_snapshot = fixture
        .saved_snapshot()
        .expect("the first tick saves verified progress");
    let saves_after_setup = fixture.snapshot_save_attempts();
    drop(state);
    let mut restarted_state = fixture.restart_verifier_service_state();

    // 2. Process the first resumed tick so inclusive recovery rebuilds update 9's partial envelope.
    restarted_state
        .handle_tick()
        .await
        .expect("resumed tick processing succeeds");
    assert_one_recovered_blob(
        &fixture.recovered_da_blobs(),
        &scenario.updates[8],
        scenario.first_tick_completion_blocks.update_8,
    );
    assert_eq!(fixture.saved_snapshot(), Some(previous_snapshot.clone()));

    let writes_before_final_reveal = fixture.recovered_da_write_attempts();

    // 3. Mine update 9's final reveal, make its OL account update available, and process again.
    let update_9_completion =
        make_multi_tick_update_9_available_for_second_tick(&fixture, &scenario);
    restarted_state
        .handle_tick()
        .await
        .expect("tick processing succeeds after the final reveal");

    // 4. Show the final reveal completes update 9 from extractor state rebuilt before this scan,
    // then verify and snapshot it from the restored account-state anchor.
    let writes = fixture.recovered_da_write_attempts();
    let writes_after_final_reveal = &writes[writes_before_final_reveal.len()..];
    assert_eq!(writes_after_final_reveal.len(), 1);
    // This tick scans from height 146 and sees only the final reveal, so producing the blob
    // requires the commit and first reveal reconstructed by the preceding resumed tick.
    assert_one_recovered_blob(
        writes_after_final_reveal[0].blobs(),
        &scenario.updates[9],
        update_9_completion,
    );
    assert!(fixture.recovered_da_blobs().is_empty());
    // Advancing from saved seqno 9 to 10 proves update 9 was applied to the account state restored
    // through update 8, rather than to a fresh account state.
    let status = DaVerifierService::<MockDaVerifierContext>::get_status(&restarted_state);
    assert_eq!(status.next_update_seq_no, 10);
    let saves = fixture.snapshot_save_attempts();
    let saves_after_restart = &saves[saves_after_setup.len()..];
    assert_eq!(saves_after_restart.len(), 1);
    assert_eq!(
        saves_after_restart[0].outcome(),
        SnapshotSaveOutcome::Succeeded
    );
    assert_eq!(
        saves_after_restart[0]
            .snapshot()
            .replay_snapshot
            .next_update_seq_no(),
        Seqno::new(10)
    );
    assert_eq!(
        saves_after_restart[0].snapshot().resume_l1_block.height(),
        MULTI_TICK_UPDATE_9_COMMIT_HEIGHT
    );
    assert_eq!(
        saves_after_restart[0].snapshot().completion_block,
        update_9_completion
    );
    assert_ne!(saves_after_restart[0].snapshot(), &previous_snapshot);
}

#[tokio::test]
async fn test_service_continues_on_recoverable_error() {
    let fixture = DaVerifierFixture::new(
        AlpenParams::default(),
        TEST_GENESIS_L1_HEIGHT,
        TEST_L1_REORG_SAFE_DEPTH,
    )
    .with_bitcoin_tip_height(TEST_ONE_BLOCK_SCAN_BITCOIN_TIP);
    fixture.fail_next_recovered_da_read(RecoveredDaDbError::WorkerCancelled);
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
