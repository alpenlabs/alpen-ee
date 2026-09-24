//! Persists the state required to resume EE DA verification.

use std::{
    fs::{self, File},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
};

use alpen_acct_state::compute_ee_account_inner_root;
use alpen_acct_types::EeAccountState;
use alpen_batch_replay::BatchReplaySnapshot;
use alpen_evm_ee::{decode_ethereum_state, encode_ethereum_state};
use eyre::Context;
use ssz::{Decode, Encode};
use strata_acct_types::Hash;
use strata_codec::{BufDecoder, Codec, Decoder, Encoder};
use strata_identifiers::{Buf32, L1BlockCommitment, L1Height};
use strata_snark_acct_types::Seqno;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::account_state::VerifiedAccountState;

const SNAPSHOT_VERSION: u16 = 1;
const INNER_STATE_ROOT_LEN: usize = 32;

/// EVM replay and verified EE account state loaded from one snapshot.
pub(crate) struct ReconstructionSnapshot {
    replay: BatchReplaySnapshot,
    verified_account_state: VerifiedAccountState,
    resume_l1_block: L1BlockCommitment,
    completion_block: L1BlockCommitment,
}

impl ReconstructionSnapshot {
    /// Creates a reconstruction snapshot after validating both state anchors.
    pub(crate) fn try_new(
        replay: BatchReplaySnapshot,
        verified_account_state: VerifiedAccountState,
        resume_l1_block: L1BlockCommitment,
        completion_block: L1BlockCommitment,
    ) -> Result<Self, SnapshotValidationError> {
        validate_snapshot(replay.state_root(), &verified_account_state)?;
        validate_snapshot_boundaries(resume_l1_block, completion_block)?;
        Ok(Self {
            replay,
            verified_account_state,
            resume_l1_block,
            completion_block,
        })
    }

    /// Returns the inclusive L1 block from which DA recovery resumes.
    pub(crate) fn resume_l1_block(&self) -> L1BlockCommitment {
        self.resume_l1_block
    }

    /// Returns the highest DA completion block represented by this snapshot.
    pub(crate) fn completion_block(&self) -> L1BlockCommitment {
        self.completion_block
    }

    /// Consumes the snapshot and returns its replay and account-state anchors.
    pub(crate) fn into_state_parts(self) -> (BatchReplaySnapshot, VerifiedAccountState) {
        (self.replay, self.verified_account_state)
    }
}

/// Snapshot data is unsupported, malformed, or internally inconsistent.
#[derive(Debug, Error)]
pub(crate) enum SnapshotValidationError {
    /// The snapshot uses a format version unsupported by this binary.
    #[error("unsupported snapshot version (expected {expected}, got {actual})")]
    UnsupportedVersion { expected: u16, actual: u16 },

    /// The encoded account state ends before its declared length.
    #[error("snapshot account state is truncated (expected {expected} bytes, {remaining} remain)")]
    TruncatedAccountState { expected: usize, remaining: usize },

    /// The snapshot contains data after its account state.
    #[error("snapshot contains {count} trailing bytes")]
    TrailingBytes { count: usize },

    /// The replayed EVM state does not match the state recorded by the EE account.
    #[error("replayed EVM and EE account state roots do not match")]
    StateRootMismatch,

    /// The EE account state does not match the root previously verified against OL.
    #[error("EE account state does not match the OL-published inner state root")]
    InnerStateRootMismatch,

    /// The recovery point follows data the snapshot claims to cover.
    #[error(
        "snapshot resume L1 height {resume_l1_height} exceeds completion L1 height {completion_l1_height}"
    )]
    ResumeAfterCompletion {
        resume_l1_height: L1Height,
        completion_l1_height: L1Height,
    },
}

/// Loading a reconstruction snapshot failed.
#[derive(Debug, Error)]
pub(crate) enum SnapshotLoadError {
    /// Reading the snapshot file failed.
    #[error("failed to read reconstruction snapshot from {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Decoding the snapshot file failed.
    #[error("failed to decode reconstruction snapshot from {path}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: eyre::Report,
    },

    /// The decoded snapshot violates an internal invariant.
    #[error(transparent)]
    Validation(#[from] SnapshotValidationError),
}

impl SnapshotLoadError {
    /// Returns whether loading can be retried without changing verifier state.
    pub(crate) fn is_recoverable(&self) -> bool {
        matches!(self, Self::Io { .. })
    }
}

/// Deleting a reconstruction snapshot failed.
#[derive(Debug, Error)]
pub(crate) enum SnapshotDeleteError {
    /// Removing the snapshot file or synchronizing its directory failed.
    #[error("failed to {operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl SnapshotDeleteError {
    /// Returns whether deletion can be retried without changing verifier state.
    pub(crate) fn is_recoverable(&self) -> bool {
        true
    }
}

/// Saving a reconstruction snapshot failed.
#[derive(Debug, Error)]
pub(crate) enum SnapshotSaveError {
    /// The replay and account-state anchors are inconsistent.
    #[error(transparent)]
    Validation(#[from] SnapshotValidationError),

    /// Encoding the in-memory snapshot failed.
    #[error("failed to encode reconstruction snapshot: {0}")]
    Encoding(#[source] eyre::Report),

    /// Persisting the encoded snapshot failed.
    #[error("failed to {operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl SnapshotSaveError {
    /// Returns whether saving can be retried without changing verifier state.
    pub(crate) fn is_recoverable(&self) -> bool {
        matches!(self, Self::Io { .. })
    }
}

/// Loads a reconstruction snapshot, returning [`None`] when the path does not exist.
pub(crate) fn load_reconstruction_snapshot(
    path: &Path,
) -> Result<Option<ReconstructionSnapshot>, SnapshotLoadError> {
    let encoded = match fs::read(path) {
        Ok(encoded) => encoded,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SnapshotLoadError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let mut decoder = BufDecoder::new(encoded);
    let version =
        u16::decode(&mut decoder).map_err(|source| snapshot_decode_error(path, source))?;
    if version != SNAPSHOT_VERSION {
        return Err(SnapshotValidationError::UnsupportedVersion {
            expected: SNAPSHOT_VERSION,
            actual: version,
        }
        .into());
    }

    let resume_l1_block = L1BlockCommitment::decode(&mut decoder)
        .map_err(|source| snapshot_decode_error(path, source))?;
    let completion_block = L1BlockCommitment::decode(&mut decoder)
        .map_err(|source| snapshot_decode_error(path, source))?;
    let next_update_seq_no =
        u64::decode(&mut decoder).map_err(|source| snapshot_decode_error(path, source))?;
    let last_applied_block_num =
        u64::decode(&mut decoder).map_err(|source| snapshot_decode_error(path, source))?;
    let ethereum_state = decode_ethereum_state(&mut decoder)
        .map_err(|source| snapshot_decode_error(path, source))?;
    let next_inbox_msg_idx =
        u64::decode(&mut decoder).map_err(|source| snapshot_decode_error(path, source))?;

    let mut expected_inner_state_root_bytes = [0; INNER_STATE_ROOT_LEN];
    decoder
        .read_buf(&mut expected_inner_state_root_bytes)
        .map_err(|source| snapshot_decode_error(path, source))?;
    let expected_inner_state_root = Hash::from_ssz_bytes(&expected_inner_state_root_bytes)
        .map_err(|source| snapshot_decode_error(path, source))?;

    let account_state_len =
        u32::decode(&mut decoder).map_err(|source| snapshot_decode_error(path, source))?;
    let account_state_len =
        usize::try_from(account_state_len).map_err(|source| snapshot_decode_error(path, source))?;
    // Validate the declared length before allocating, so a corrupt snapshot cannot request
    // more account-state memory than its remaining payload can supply.
    if account_state_len > decoder.remaining() {
        return Err(SnapshotValidationError::TruncatedAccountState {
            expected: account_state_len,
            remaining: decoder.remaining(),
        }
        .into());
    }
    let mut account_state_bytes = vec![0; account_state_len];
    decoder
        .read_buf(&mut account_state_bytes)
        .map_err(|source| snapshot_decode_error(path, source))?;
    if decoder.remaining() != 0 {
        return Err(SnapshotValidationError::TrailingBytes {
            count: decoder.remaining(),
        }
        .into());
    }
    let account_state = EeAccountState::from_ssz_bytes(&account_state_bytes)
        .map_err(|source| snapshot_decode_error(path, source))?;
    let verified_account_state =
        VerifiedAccountState::new(account_state, next_inbox_msg_idx, expected_inner_state_root);
    let replay = BatchReplaySnapshot::new(
        Seqno::new(next_update_seq_no),
        last_applied_block_num,
        ethereum_state,
    );
    Ok(Some(ReconstructionSnapshot::try_new(
        replay,
        verified_account_state,
        resume_l1_block,
        completion_block,
    )?))
}

/// Removes a reconstruction snapshot and durably records its absence.
pub(crate) fn delete_reconstruction_snapshot(path: &Path) -> Result<(), SnapshotDeleteError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(snapshot_delete_io_error(
                "remove reconstruction snapshot",
                path,
                source,
            ))
        }
    }

    let parent = snapshot_parent(path);
    let directory = File::open(parent)
        .map_err(|source| snapshot_delete_io_error("open snapshot directory", parent, source))?;
    directory
        .sync_all()
        .map_err(|source| snapshot_delete_io_error("sync snapshot directory", parent, source))?;
    Ok(())
}

/// Atomically writes one verified replay and account-state anchor.
pub(crate) fn save_reconstruction_snapshot(
    path: &Path,
    replay_snapshot: &BatchReplaySnapshot,
    verified_account_state: &VerifiedAccountState,
    resume_l1_block: L1BlockCommitment,
    completion_block: L1BlockCommitment,
) -> Result<(), SnapshotSaveError> {
    validate_snapshot(replay_snapshot.state_root(), verified_account_state)?;
    validate_snapshot_boundaries(resume_l1_block, completion_block)?;

    let encoded = encode_snapshot(
        replay_snapshot,
        verified_account_state,
        resume_l1_block,
        completion_block,
    )
    .map_err(SnapshotSaveError::Encoding)?;
    replace_file(path, &encoded)
}

fn encode_snapshot(
    replay_snapshot: &BatchReplaySnapshot,
    verified_account_state: &VerifiedAccountState,
    resume_l1_block: L1BlockCommitment,
    completion_block: L1BlockCommitment,
) -> eyre::Result<Vec<u8>> {
    let encoded_account_state = verified_account_state.state().as_ssz_bytes();
    let account_state_len = u32::try_from(encoded_account_state.len())
        .wrap_err("encoded EE account state exceeds snapshot length limit")?;

    let mut encoded = Vec::new();
    SNAPSHOT_VERSION
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot version")?;
    resume_l1_block
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot L1 resume block")?;
    completion_block
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot L1 completion block")?;
    replay_snapshot
        .next_update_seq_no()
        .inner()
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot update sequence number")?;
    replay_snapshot
        .last_applied_block_num()
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot block number")?;
    encode_ethereum_state(replay_snapshot.ethereum_state(), &mut encoded)
        .wrap_err("failed to encode snapshot Ethereum state")?;
    verified_account_state
        .next_inbox_msg_idx()
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot inbox cursor")?;
    // `Hash` has a fixed 32-byte SSZ encoding, so this field needs no length prefix.
    encoded
        .write_buf(
            &verified_account_state
                .expected_inner_state_root()
                .as_ssz_bytes(),
        )
        .wrap_err("failed to encode snapshot expected inner state root")?;
    account_state_len
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot account state length")?;
    encoded
        .write_buf(&encoded_account_state)
        .wrap_err("failed to encode snapshot account state")?;

    Ok(encoded)
}

fn validate_snapshot(
    replayed_state_root: Buf32,
    verified_account_state: &VerifiedAccountState,
) -> Result<(), SnapshotValidationError> {
    if verified_account_state.state().last_exec_state_root().0 != replayed_state_root.0 {
        return Err(SnapshotValidationError::StateRootMismatch);
    }

    let reconstructed_inner_state_root =
        compute_ee_account_inner_root(verified_account_state.state());
    if reconstructed_inner_state_root != verified_account_state.expected_inner_state_root() {
        return Err(SnapshotValidationError::InnerStateRootMismatch);
    }

    Ok(())
}

fn validate_snapshot_boundaries(
    resume_l1_block: L1BlockCommitment,
    completion_block: L1BlockCommitment,
) -> Result<(), SnapshotValidationError> {
    if resume_l1_block.height() > completion_block.height() {
        return Err(SnapshotValidationError::ResumeAfterCompletion {
            resume_l1_height: resume_l1_block.height(),
            completion_l1_height: completion_block.height(),
        });
    }
    Ok(())
}

fn snapshot_decode_error(path: &Path, source: impl Into<eyre::Report>) -> SnapshotLoadError {
    SnapshotLoadError::Decode {
        path: path.to_path_buf(),
        source: source.into(),
    }
}

fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), SnapshotSaveError> {
    let parent = snapshot_parent(path);
    let mut temp_file = NamedTempFile::new_in(parent)
        .map_err(|source| snapshot_io_error("create temporary snapshot", parent, source))?;
    temp_file
        .write_all(bytes)
        .map_err(|source| snapshot_io_error("write reconstruction snapshot", path, source))?;
    temp_file
        .as_file_mut()
        .sync_all()
        .map_err(|source| snapshot_io_error("sync reconstruction snapshot", path, source))?;
    temp_file
        .persist(path)
        .map_err(|error| snapshot_io_error("replace reconstruction snapshot", path, error.error))?;
    let directory = File::open(parent)
        .map_err(|source| snapshot_io_error("open snapshot directory", parent, source))?;
    directory
        .sync_all()
        .map_err(|source| snapshot_io_error("sync snapshot directory", parent, source))?;
    Ok(())
}

fn snapshot_io_error(operation: &'static str, path: &Path, source: io::Error) -> SnapshotSaveError {
    SnapshotSaveError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn snapshot_delete_io_error(
    operation: &'static str,
    path: &Path,
    source: io::Error,
) -> SnapshotDeleteError {
    SnapshotDeleteError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn snapshot_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use alpen_acct_types::{EeAccountState, PendingFinclEntry, PendingInputEntry};
    use alpen_batch_replay::{replay_from_genesis, BatchReplayOutcome, EvmReplayBatch};
    use alpen_chain_types::SubjectDepositData;
    use alpen_da_types::EvmHeaderSummary;
    use alpen_reth_statediff::test_utils::{
        account_change, addr, batch_diff, block_diff, hash, slot, snapshot as account_snapshot,
        storage_change, value,
    };
    use bitcoin::hashes::{sha256, Hash as _};
    use eyre::eyre;
    use strata_acct_types::{BitcoinAmount, Hash, SubjectId};
    use strata_identifiers::{L1BlockId, L1Height};
    use strata_predicate::PredicateKey;
    use strata_snark_acct_types::Seqno;
    use tempfile::tempdir;

    use super::*;

    const EXISTING_SNAPSHOT_BYTES: &[u8] = b"existing snapshot";

    fn replay_batch_with_populated_state() -> BatchReplayOutcome {
        let mut block = block_diff();
        for seed in [1, 2] {
            let address = addr(seed);
            account_change(
                &mut block,
                address,
                None,
                Some(account_snapshot(
                    1_000 + u64::from(seed),
                    u64::from(seed),
                    hash(seed),
                )),
            );
            storage_change(
                &mut block,
                address,
                slot(u64::from(seed)),
                value(0),
                value(10 + u64::from(seed)),
            );
        }

        let outcome = replay_from_genesis(
            [],
            [EvmReplayBatch::new(
                Seqno::zero(),
                EvmHeaderSummary {
                    block_num: 1,
                    timestamp: 1,
                    base_fee: 1,
                    gas_used: 0,
                    gas_limit: 1,
                    da_rate: None,
                },
                batch_diff(&[block]),
            )],
        )
        .expect("batch replays");
        assert_eq!(
            outcome.final_state().storage_tries.len(),
            2,
            "snapshot fixture must retain both account storage tries"
        );
        outcome
    }

    fn build_snapshot_state() -> (BatchReplaySnapshot, VerifiedAccountState) {
        let outcome = replay_batch_with_populated_state();
        let state_root = outcome.final_state_root();
        let pending_inputs = vec![
            PendingInputEntry::Deposit(SubjectDepositData::new(
                SubjectId::new([3; 32]),
                BitcoinAmount::try_from(50_000).expect("test deposit amount is valid"),
            )),
            PendingInputEntry::PredicateRotation(PredicateKey::always_accept()),
        ];
        let pending_fincls = vec![
            PendingFinclEntry::new(7, Hash::new([4; 32])),
            PendingFinclEntry::new(8, Hash::new([5; 32])),
        ];
        let account_state = EeAccountState::new(
            Hash::new([7; 32]),
            Hash::new(state_root.0),
            pending_inputs,
            pending_fincls,
        );
        let verified_account_state = VerifiedAccountState::new(
            account_state.clone(),
            9,
            compute_ee_account_inner_root(&account_state),
        );
        let replay_snapshot =
            BatchReplaySnapshot::try_new(Seqno::new(1), 1, state_root, outcome.into_final_state())
                .expect("replay snapshot builds");
        (replay_snapshot, verified_account_state)
    }

    // This fixture is frozen exclusively for the V1 encoding contract test. Do not update it as
    // part of unrelated test-fixture maintenance.
    fn encode_frozen_snapshot_v1() -> Vec<u8> {
        let mut block = block_diff();
        for seed in [11, 22] {
            let address = addr(seed);
            account_change(
                &mut block,
                address,
                None,
                Some(account_snapshot(
                    10_000 + u64::from(seed),
                    u64::from(seed),
                    hash(seed),
                )),
            );
            storage_change(
                &mut block,
                address,
                slot(u64::from(seed)),
                value(0),
                value(100 + u64::from(seed)),
            );
        }
        let outcome = replay_from_genesis(
            [],
            [EvmReplayBatch::new(
                Seqno::zero(),
                EvmHeaderSummary {
                    block_num: 17,
                    timestamp: 1_700_000,
                    base_fee: 2,
                    gas_used: 3,
                    gas_limit: 4,
                    da_rate: None,
                },
                batch_diff(&[block]),
            )],
        )
        .expect("frozen batch replays");
        let state_root = outcome.final_state_root();
        let replay_snapshot =
            BatchReplaySnapshot::try_new(Seqno::new(1), 17, state_root, outcome.into_final_state())
                .expect("frozen replay snapshot builds");
        let account_state = EeAccountState::new(
            Hash::new([0x31; 32]),
            Hash::new(state_root.0),
            vec![
                PendingInputEntry::Deposit(SubjectDepositData::new(
                    SubjectId::new([0x32; 32]),
                    BitcoinAmount::try_from(12_345).expect("frozen deposit amount is valid"),
                )),
                PendingInputEntry::PredicateRotation(PredicateKey::always_accept()),
            ],
            vec![
                PendingFinclEntry::new(18, Hash::new([0x33; 32])),
                PendingFinclEntry::new(19, Hash::new([0x34; 32])),
            ],
        );
        let verified_account_state = VerifiedAccountState::new(
            account_state.clone(),
            13,
            compute_ee_account_inner_root(&account_state),
        );

        encode_snapshot(
            &replay_snapshot,
            &verified_account_state,
            build_l1_commitment(101, 0xaa),
            build_l1_commitment(102, 0xbb),
        )
        .expect("frozen V1 snapshot encodes")
    }

    fn build_l1_commitment(height: L1Height, block_id_byte: u8) -> L1BlockCommitment {
        L1BlockCommitment::new(height, L1BlockId::from(Buf32::from([block_id_byte; 32])))
    }

    fn encode_valid_snapshot() -> Vec<u8> {
        let (replay_snapshot, verified_account_state) = build_snapshot_state();
        encode_snapshot(
            &replay_snapshot,
            &verified_account_state,
            build_l1_commitment(42, 1),
            build_l1_commitment(43, 2),
        )
        .expect("test snapshot encodes")
    }

    fn load_snapshot_error(path: &Path) -> SnapshotLoadError {
        load_reconstruction_snapshot(path)
            .err()
            .expect("snapshot load must fail")
    }

    #[test]
    fn test_snapshot_save_io_failure_is_recoverable() {
        let error = SnapshotSaveError::Io {
            operation: "test snapshot operation",
            path: PathBuf::from("snapshot"),
            source: io::Error::other("test IO failure"),
        };

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_snapshot_save_validation_failure_is_fatal() {
        let error = SnapshotSaveError::Validation(SnapshotValidationError::StateRootMismatch);

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_snapshot_save_encoding_failure_is_fatal() {
        let error = SnapshotSaveError::Encoding(eyre!("test encoding failure"));

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_snapshot_delete_io_failure_is_recoverable() {
        let error = SnapshotDeleteError::Io {
            operation: "test snapshot operation",
            path: PathBuf::from("snapshot"),
            source: io::Error::other("test IO failure"),
        };

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_snapshot_load_io_failure_is_recoverable() {
        let error = SnapshotLoadError::Io {
            path: PathBuf::from("snapshot"),
            source: io::Error::from_raw_os_error(5),
        };

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_snapshot_load_decode_failure_is_fatal() {
        let error = SnapshotLoadError::Decode {
            path: PathBuf::from("snapshot"),
            source: eyre!("test decoding failure"),
        };

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_snapshot_load_validation_failure_is_fatal() {
        let error = SnapshotLoadError::Validation(SnapshotValidationError::InnerStateRootMismatch);

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_snapshot_save_and_load() {
        // 1. Put unrelated bytes at the snapshot path and build the state to save.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        fs::write(&path, EXISTING_SNAPSHOT_BYTES).expect("existing snapshot writes");
        let (replay_snapshot, verified_account_state) = build_snapshot_state();
        let resume_l1_block = build_l1_commitment(42, 1);
        let completion_block = build_l1_commitment(43, 2);

        // 2. Save the snapshot over the existing file.
        save_reconstruction_snapshot(
            &path,
            &replay_snapshot,
            &verified_account_state,
            resume_l1_block,
            completion_block,
        )
        .expect("snapshot saves");

        // 3. Load the replacement and verify every stored state component.
        let loaded = load_reconstruction_snapshot(&path)
            .expect("snapshot loads")
            .expect("snapshot exists");
        assert_eq!(loaded.resume_l1_block(), resume_l1_block);
        assert_eq!(loaded.completion_block(), completion_block);
        let (loaded_replay_snapshot, loaded_account_state) = loaded.into_state_parts();
        assert_eq!(loaded_replay_snapshot, replay_snapshot);
        assert_eq!(loaded_account_state, verified_account_state);
    }

    #[test]
    fn test_resume_after_completion_is_rejected_on_save() {
        // 1. Keep an existing file and build otherwise valid snapshot state whose resume block
        // follows its claimed highest completion block.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        fs::write(&path, EXISTING_SNAPSHOT_BYTES).expect("existing snapshot writes");
        let (replay_snapshot, verified_account_state) = build_snapshot_state();
        let resume_l1_block = build_l1_commitment(43, 1);
        let completion_block = build_l1_commitment(42, 2);

        // 2. Attempt to save the incoherent L1 boundaries.
        let error = save_reconstruction_snapshot(
            &path,
            &replay_snapshot,
            &verified_account_state,
            resume_l1_block,
            completion_block,
        )
        .expect_err("resume after completion must fail");

        // 3. Confirm validation reports both heights without replacing the existing file.
        assert!(matches!(
            error,
            SnapshotSaveError::Validation(SnapshotValidationError::ResumeAfterCompletion {
                resume_l1_height: 43,
                completion_l1_height: 42,
            })
        ));
        assert_eq!(
            fs::read(&path).expect("existing snapshot remains readable"),
            EXISTING_SNAPSHOT_BYTES
        );
    }

    #[test]
    fn test_mismatched_state_roots_are_rejected() {
        // 1. Keep an existing file and build internally consistent account state whose execution
        // root differs from the replayed state root.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        fs::write(&path, EXISTING_SNAPSHOT_BYTES).expect("existing snapshot writes");
        let (replay_snapshot, _) = build_snapshot_state();
        let account_state = EeAccountState::new(
            Hash::new([7; 32]),
            Hash::new([8; 32]),
            Vec::new(),
            Vec::new(),
        );
        let verified_account_state = VerifiedAccountState::new(
            account_state.clone(),
            9,
            compute_ee_account_inner_root(&account_state),
        );

        // 2. Attempt to save the inconsistent replay and account-state anchors.
        let error = save_reconstruction_snapshot(
            &path,
            &replay_snapshot,
            &verified_account_state,
            L1BlockCommitment::default(),
            L1BlockCommitment::default(),
        )
        .expect_err("mismatched roots must fail");

        // 3. Confirm validation reports the root mismatch without replacing the existing file.
        assert!(matches!(
            error,
            SnapshotSaveError::Validation(SnapshotValidationError::StateRootMismatch)
        ));
        assert_eq!(
            fs::read(&path).expect("existing snapshot remains readable"),
            EXISTING_SNAPSHOT_BYTES
        );
    }

    #[test]
    fn test_mismatched_inner_state_root_is_rejected() {
        // 1. Keep an existing file and replace an otherwise valid account anchor's expected inner
        // state root with an incorrect value.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        fs::write(&path, EXISTING_SNAPSHOT_BYTES).expect("existing snapshot writes");
        let (replay_snapshot, verified_account_state) = build_snapshot_state();
        let invalid_account_state = VerifiedAccountState::new(
            verified_account_state.state().clone(),
            verified_account_state.next_inbox_msg_idx(),
            Hash::zero(),
        );

        // 2. Attempt to save the replay state with the invalid account anchor.
        let error = save_reconstruction_snapshot(
            &path,
            &replay_snapshot,
            &invalid_account_state,
            L1BlockCommitment::default(),
            L1BlockCommitment::default(),
        )
        .expect_err("mismatched inner state root must fail");

        // 3. Confirm validation reports the inner-root mismatch without replacing the file.
        assert!(matches!(
            error,
            SnapshotSaveError::Validation(SnapshotValidationError::InnerStateRootMismatch)
        ));
        assert_eq!(
            fs::read(&path).expect("existing snapshot remains readable"),
            EXISTING_SNAPSHOT_BYTES
        );
    }

    #[test]
    fn test_snapshot_encoding_matches_versioned_digest() {
        // 1. Encode the frozen populated V1 fixture.
        let encoded = encode_frozen_snapshot_v1();

        // 2. Hash the complete versioned encoding, including both replay and account state.
        let digest = sha256::Hash::hash(&encoded);

        // 3. A mismatch means the on-disk format changed. Revert unintended drift; for an
        // intentional change, bump `SNAPSHOT_VERSION` and update these values deliberately.
        assert_eq!(SNAPSHOT_VERSION, 1, "snapshot format version changed");
        assert_eq!(encoded.len(), 781, "snapshot V1 encoded length changed");
        assert_eq!(
            digest.to_string(),
            "b2b7374cbeedec3adf013d2207e9cd27a54a97f44171e8283b119cdb8ffe906f",
            "snapshot V1 encoding changed"
        );
    }

    #[test]
    fn test_snapshot_save_io_error_is_propagated() {
        // 1. Build a valid snapshot and target a path under a directory that does not exist.
        let directory = tempdir().expect("temporary directory builds");
        let missing_parent = directory.path().join("missing");
        let path = missing_parent.join("reconstruction.snapshot");
        let (replay_snapshot, verified_account_state) = build_snapshot_state();

        // 2. Attempt the save and capture the first filesystem failure.
        let error = save_reconstruction_snapshot(
            &path,
            &replay_snapshot,
            &verified_account_state,
            build_l1_commitment(42, 1),
            build_l1_commitment(43, 2),
        )
        .expect_err("saving below a missing directory must fail");

        // 3. Confirm the IO variant retains the failing path and filesystem error kind.
        assert!(matches!(
            error,
            SnapshotSaveError::Io {
                operation: _,
                path,
                source,
            } if path == missing_parent && source.kind() == ErrorKind::NotFound
        ));
    }

    #[test]
    fn test_snapshot_delete_removes_existing_file() {
        // 1. Write a snapshot file at the configured path.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        fs::write(&path, encode_valid_snapshot()).expect("snapshot writes");

        // 2. Delete the snapshot and confirm subsequent loading observes its absence.
        delete_reconstruction_snapshot(&path).expect("snapshot deletion succeeds");
        let loaded =
            load_reconstruction_snapshot(&path).expect("deleted snapshot path remains readable");
        assert!(loaded.is_none());
    }

    #[test]
    fn test_missing_snapshot_delete_succeeds() {
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("missing.snapshot");

        delete_reconstruction_snapshot(&path).expect("missing snapshot is already deleted");
    }

    #[test]
    fn test_snapshot_delete_io_error_is_propagated() {
        let directory = tempdir().expect("temporary directory builds");

        let error = delete_reconstruction_snapshot(directory.path())
            .expect_err("deleting a directory as a snapshot must fail");

        assert!(matches!(error, SnapshotDeleteError::Io { .. }));
    }

    #[test]
    fn test_missing_snapshot_returns_none() {
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");

        let loaded =
            load_reconstruction_snapshot(&path).expect("missing snapshot returns no saved state");

        assert!(loaded.is_none());
    }

    #[test]
    fn test_snapshot_read_error_is_propagated() {
        let directory = tempdir().expect("temporary directory builds");

        let error = load_snapshot_error(directory.path());

        assert!(matches!(
            error,
            SnapshotLoadError::Io { source, .. } if source.kind() != ErrorKind::NotFound
        ));
    }

    #[test]
    fn test_truncated_snapshot_version_is_decode_error() {
        // 1. Write only one byte of the two-byte snapshot version field.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        fs::write(&path, [0]).expect("truncated snapshot version writes");

        // 2. Confirm loading fails at the codec boundary rather than validation.
        let error = load_snapshot_error(&path);
        assert!(matches!(error, SnapshotLoadError::Decode { .. }));
    }

    #[test]
    fn test_unsupported_snapshot_version_is_rejected() {
        // 1. Encode only an unsupported version; it must be rejected before later fields are read.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        let mut encoded = Vec::new();
        let unsupported_version = SNAPSHOT_VERSION + 1;
        unsupported_version
            .encode(&mut encoded)
            .expect("test version encodes");
        fs::write(&path, encoded).expect("unsupported snapshot writes");

        // 2. Load the version-only snapshot and capture the validation failure.
        let error = load_snapshot_error(&path);

        // 3. Confirm the error reports both the supported and encoded versions.
        assert!(matches!(
            error,
            SnapshotLoadError::Validation(SnapshotValidationError::UnsupportedVersion {
                expected: SNAPSHOT_VERSION,
                actual,
            }) if actual == unsupported_version
        ));
    }

    #[test]
    fn test_truncated_account_state_is_rejected() {
        // 1. Account state is the final encoded field; remove its final byte while retaining its
        // declared length.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        let mut encoded = encode_valid_snapshot();
        encoded.pop().expect("encoded snapshot is non-empty");
        fs::write(&path, encoded).expect("truncated snapshot writes");

        // 2. Load the truncated snapshot and capture the validation failure.
        let error = load_snapshot_error(&path);

        // 3. Confirm the payload is exactly one byte shorter than its declared length.
        assert!(matches!(
            error,
            SnapshotLoadError::Validation(SnapshotValidationError::TruncatedAccountState {
                expected,
                remaining,
            }) if remaining + 1 == expected
        ));
    }

    #[test]
    fn test_snapshot_trailing_bytes_are_rejected() {
        // 1. Append one byte after the final field of an otherwise valid snapshot.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        let mut encoded = encode_valid_snapshot();
        encoded.push(0xff);
        fs::write(&path, encoded).expect("snapshot with trailing byte writes");

        // 2. Load the snapshot and capture the validation failure.
        let error = load_snapshot_error(&path);

        // 3. Confirm the decoder reports exactly the appended byte.
        assert!(matches!(
            error,
            SnapshotLoadError::Validation(SnapshotValidationError::TrailingBytes { count: 1 })
        ));
    }

    #[test]
    fn test_resume_after_completion_is_rejected_on_load() {
        // 1. Encode otherwise valid state with a resume block after its claimed completion block.
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        let (replay_snapshot, verified_account_state) = build_snapshot_state();
        let encoded = encode_snapshot(
            &replay_snapshot,
            &verified_account_state,
            build_l1_commitment(43, 1),
            build_l1_commitment(42, 2),
        )
        .expect("incoherent boundaries still have a valid encoding");
        fs::write(&path, encoded).expect("snapshot writes");

        // 2. Load the encoded snapshot through the production reader.
        let error = load_snapshot_error(&path);

        // 3. Confirm construction rejects the incoherent boundary heights.
        assert!(matches!(
            error,
            SnapshotLoadError::Validation(SnapshotValidationError::ResumeAfterCompletion {
                resume_l1_height: 43,
                completion_l1_height: 42,
            })
        ));
    }
}
