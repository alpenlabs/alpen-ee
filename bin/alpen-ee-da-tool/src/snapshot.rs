//! Persists reconstruction snapshots for the standalone tool.

use std::{
    fs::{self, File},
    io::{ErrorKind, Write},
    path::Path,
};

use alpen_ee_batch_replay::{BatchReplayOutcome, BatchReplaySnapshot};
use alpen_reth_statediff::EthereumStateExt;
use eyre::{eyre, Context};
use ssz::{Decode, Encode};
use strata_codec::{BufDecoder, Codec, Decoder, Encoder};
use strata_ee_acct_types::EeAccountState;
use strata_evm_ee::{decode_ethereum_state, encode_ethereum_state};
use strata_snark_acct_types::Seqno;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::account_state::VerifiedAccountState;

/// EVM replay and verified EE account state loaded from one snapshot.
#[derive(Debug)]
pub(crate) struct ReconstructionSnapshot {
    replay: BatchReplaySnapshot,
    verified_account_state: VerifiedAccountState,
}

impl ReconstructionSnapshot {
    fn new(replay: BatchReplaySnapshot, verified_account_state: VerifiedAccountState) -> Self {
        Self {
            replay,
            verified_account_state,
        }
    }

    pub(crate) fn into_parts(self) -> (BatchReplaySnapshot, VerifiedAccountState) {
        (self.replay, self.verified_account_state)
    }
}

#[derive(Debug, Error)]
enum SnapshotValidationError {
    /// The encoded account state ends before its declared length.
    #[error("snapshot account state is truncated (expected {expected} bytes, {remaining} remain)")]
    TruncatedAccountState { expected: usize, remaining: usize },

    /// The snapshot contains data after its account state.
    #[error("snapshot contains {count} trailing bytes")]
    TrailingBytes { count: usize },

    /// The replayed EVM state does not match the state recorded by the EE account.
    #[error("replayed EVM and EE account state roots do not match")]
    StateRootMismatch,
}

/// Loads a reconstruction snapshot, returning [`None`] when the path does not exist.
pub(crate) fn load_reconstruction_snapshot(
    path: &Path,
) -> eyre::Result<Option<ReconstructionSnapshot>> {
    let encoded = match fs::read(path) {
        Ok(encoded) => encoded,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).wrap_err_with(|| {
                format!(
                    "failed to read reconstruction snapshot from {}",
                    path.display()
                )
            })
        }
    };
    let mut decoder = BufDecoder::new(encoded);
    let next_update_seq_no =
        u64::decode(&mut decoder).wrap_err_with(|| snapshot_decode_error(path))?;
    let last_applied_block_num =
        u64::decode(&mut decoder).wrap_err_with(|| snapshot_decode_error(path))?;
    let ethereum_state =
        decode_ethereum_state(&mut decoder).wrap_err_with(|| snapshot_decode_error(path))?;
    let next_inbox_msg_idx =
        u64::decode(&mut decoder).wrap_err_with(|| snapshot_decode_error(path))?;
    let account_state_len =
        u32::decode(&mut decoder).wrap_err_with(|| snapshot_decode_error(path))?;
    let account_state_len =
        usize::try_from(account_state_len).wrap_err_with(|| snapshot_decode_error(path))?;
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
        .wrap_err_with(|| snapshot_decode_error(path))?;
    if decoder.remaining() != 0 {
        return Err(SnapshotValidationError::TrailingBytes {
            count: decoder.remaining(),
        }
        .into());
    }
    let account_state = EeAccountState::from_ssz_bytes(&account_state_bytes)
        .wrap_err_with(|| snapshot_decode_error(path))?;

    let replayed_state_root = ethereum_state.state_root_buf32();
    let account_state_root = account_state.last_exec_state_root();
    if account_state_root.0 != replayed_state_root.0 {
        return Err(SnapshotValidationError::StateRootMismatch.into());
    }

    let replay = BatchReplaySnapshot::try_new(
        Seqno::new(next_update_seq_no),
        last_applied_block_num,
        replayed_state_root,
        ethereum_state,
    )
    .wrap_err_with(|| snapshot_decode_error(path))?;

    Ok(Some(ReconstructionSnapshot::new(
        replay,
        VerifiedAccountState::new(account_state, next_inbox_msg_idx),
    )))
}

/// Atomically writes the EVM and EE account state needed to continue reconstruction.
pub(crate) fn save_reconstruction_snapshot(
    path: &Path,
    batch_replay_outcome: &BatchReplayOutcome,
    verified_account_state: &VerifiedAccountState,
) -> eyre::Result<()> {
    let replayed_state_root = batch_replay_outcome.final_state_root();
    let account_state_root = verified_account_state.state().last_exec_state_root();
    if account_state_root.0 != replayed_state_root.0 {
        return Err(SnapshotValidationError::StateRootMismatch.into());
    }

    let applied_range = batch_replay_outcome.applied_range();
    let next_update_seq_no = applied_range
        .last_update_seq_no()
        .inner()
        .checked_add(1)
        .ok_or_else(|| eyre!("cannot save snapshot after terminal update sequence number"))?;
    let encoded_account_state = verified_account_state.state().as_ssz_bytes();
    let account_state_len = u32::try_from(encoded_account_state.len())
        .wrap_err("encoded EE account state exceeds snapshot length limit")?;

    let mut encoded = Vec::new();
    next_update_seq_no
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot update sequence number")?;
    applied_range
        .last_block_num()
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot block number")?;
    encode_ethereum_state(batch_replay_outcome.final_state(), &mut encoded)
        .wrap_err("failed to encode snapshot Ethereum state")?;
    verified_account_state
        .next_inbox_msg_idx()
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot inbox cursor")?;
    account_state_len
        .encode(&mut encoded)
        .wrap_err("failed to encode snapshot account state length")?;
    encoded
        .write_buf(&encoded_account_state)
        .wrap_err("failed to encode snapshot account state")?;

    replace_file(path, &encoded)
}

fn snapshot_decode_error(path: &Path) -> String {
    format!(
        "failed to decode reconstruction snapshot from {}",
        path.display()
    )
}

fn replace_file(path: &Path, bytes: &[u8]) -> eyre::Result<()> {
    let parent = snapshot_parent(path);
    let mut temp_file = NamedTempFile::new_in(parent).wrap_err_with(|| {
        format!(
            "failed to create temporary snapshot in {}",
            parent.display()
        )
    })?;
    temp_file.write_all(bytes).wrap_err_with(|| {
        format!(
            "failed to write reconstruction snapshot for {}",
            path.display()
        )
    })?;
    temp_file.as_file_mut().sync_all().wrap_err_with(|| {
        format!(
            "failed to sync reconstruction snapshot for {}",
            path.display()
        )
    })?;
    temp_file.persist(path).map_err(|error| {
        eyre!(error.error).wrap_err(format!(
            "failed to replace reconstruction snapshot at {}",
            path.display()
        ))
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .wrap_err_with(|| format!("failed to sync snapshot directory {}", parent.display()))?;
    Ok(())
}

fn snapshot_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use strata_acct_types::Hash;
    use strata_ee_acct_types::EeAccountState;
    use tempfile::tempdir;

    use super::*;
    use crate::test_utils::replay_empty_batch;

    fn build_verified_account_state(
        batch_replay_outcome: &BatchReplayOutcome,
    ) -> VerifiedAccountState {
        VerifiedAccountState::new(
            EeAccountState::new(
                Hash::new([7; 32]),
                Hash::new(batch_replay_outcome.final_state_root().0),
                Vec::new(),
                Vec::new(),
            ),
            9,
        )
    }

    #[test]
    fn test_snapshot_contains_reconstructed_state() {
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        fs::write(&path, b"old snapshot").expect("old snapshot writes");
        let outcome = replay_empty_batch();
        let expected_account_state = build_verified_account_state(&outcome);

        save_reconstruction_snapshot(&path, &outcome, &expected_account_state)
            .expect("snapshot saves");

        let snapshot = load_reconstruction_snapshot(&path)
            .expect("snapshot loads")
            .expect("snapshot exists");
        let (replay, verified_account_state) = snapshot.into_parts();
        assert_eq!(replay.next_update_seq_no(), Seqno::new(1));
        assert_eq!(replay.last_applied_block_num(), 1);
        assert_eq!(
            replay.ethereum_state().state_root_buf32(),
            outcome.final_state_root()
        );
        assert_eq!(
            verified_account_state.next_inbox_msg_idx(),
            expected_account_state.next_inbox_msg_idx()
        );
        assert_eq!(
            verified_account_state.state(),
            expected_account_state.state()
        );
    }

    #[test]
    fn test_mismatched_state_roots_are_rejected() {
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        let outcome = replay_empty_batch();
        let verified_account_state = VerifiedAccountState::new(
            EeAccountState::new(
                Hash::new([7; 32]),
                Hash::new([8; 32]),
                Vec::new(),
                Vec::new(),
            ),
            9,
        );

        let error = save_reconstruction_snapshot(&path, &outcome, &verified_account_state)
            .expect_err("mismatched roots must fail");

        assert!(matches!(
            error.downcast_ref::<SnapshotValidationError>(),
            Some(SnapshotValidationError::StateRootMismatch)
        ));
    }

    #[test]
    fn test_trailing_bytes_are_rejected() {
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("reconstruction.snapshot");
        let outcome = replay_empty_batch();
        let verified_account_state = build_verified_account_state(&outcome);
        save_reconstruction_snapshot(&path, &outcome, &verified_account_state)
            .expect("snapshot saves");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("snapshot opens")
            .write_all(&[0xff])
            .expect("trailing byte writes");

        let error = load_reconstruction_snapshot(&path).expect_err("trailing bytes reject");
        assert!(matches!(
            error.downcast_ref::<SnapshotValidationError>(),
            Some(SnapshotValidationError::TrailingBytes { count: 1 })
        ));
    }

    #[test]
    fn test_missing_snapshot_returns_none() {
        let directory = tempdir().expect("temporary directory builds");
        let path = directory.path().join("missing.snapshot");

        assert!(load_reconstruction_snapshot(&path)
            .expect("absence is valid")
            .is_none());
    }
}
