//! Converts EE DA blobs into ordered replay batches.

use alpen_batch_replay::EvmReplayBatch;
use alpen_da_types::DaBlob;
use strata_snark_acct_types::Seqno;

/// Error returned when EE DA blobs do not form a valid batch sequence.
#[derive(Debug, thiserror::Error)]
pub enum BatchSequenceError {
    /// Two EE DA blobs claim the same update sequence number.
    #[error(
        "two EE DA blobs have the same update sequence number {}",
        .update_seq_no.inner()
    )]
    DuplicateUpdateSeqNo {
        /// Duplicated update sequence number.
        update_seq_no: Seqno,
    },

    /// Two consecutive EE DA blobs skip an update sequence number.
    #[error(
        "EE DA update sequence number gap (expected {}, got {})",
        .expected.inner(),
        .actual.inner()
    )]
    UpdateSeqNoGap {
        /// Expected next update sequence number.
        expected: Seqno,

        /// Actual next update sequence number.
        actual: Seqno,
    },
}

/// Converts EE DA blobs into replay batches ordered by update sequence number.
///
/// The first sequence number is not anchored here. Genesis and snapshot replay
/// validate it against their respective starting state.
pub fn build_ordered_replay_batches<I>(blobs: I) -> Result<Vec<EvmReplayBatch>, BatchSequenceError>
where
    I: IntoIterator<Item = DaBlob>,
{
    let mut blobs = blobs.into_iter().collect::<Vec<_>>();
    blobs.sort_by_key(|blob| blob.update_seq_no);

    validate_update_sequence(&blobs)?;

    Ok(blobs
        .into_iter()
        .map(|blob| {
            EvmReplayBatch::new(
                Seqno::new(blob.update_seq_no),
                blob.evm_header,
                blob.state_diff,
            )
        })
        .collect())
}

fn validate_update_sequence(blobs: &[DaBlob]) -> Result<(), BatchSequenceError> {
    for pair in blobs.windows(2) {
        let previous = &pair[0];
        let next = &pair[1];
        let previous_seq_no = previous.update_seq_no;
        let next_seq_no = next.update_seq_no;

        if next_seq_no == previous_seq_no {
            return Err(BatchSequenceError::DuplicateUpdateSeqNo {
                update_seq_no: Seqno::new(next_seq_no),
            });
        }

        let expected_seq_no = previous_seq_no
            .checked_add(1)
            .expect("a distinct sorted successor implies a non-terminal sequence number");
        if next_seq_no != expected_seq_no {
            return Err(BatchSequenceError::UpdateSeqNoGap {
                expected: Seqno::new(expected_seq_no),
                actual: Seqno::new(next_seq_no),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::build_da_blob;

    #[test]
    fn test_unordered_blobs_sorted() {
        let batches = build_ordered_replay_batches([
            build_da_blob(7, 70),
            build_da_blob(5, 50),
            build_da_blob(6, 60),
        ])
        .expect("contiguous blobs order");

        let sequence = batches
            .iter()
            .map(EvmReplayBatch::update_seq_no)
            .collect::<Vec<_>>();
        assert_eq!(sequence, vec![Seqno::new(5), Seqno::new(6), Seqno::new(7)]);
        assert_eq!(batches[0].evm_header().block_num, 50);
        assert_eq!(batches[2].evm_header().block_num, 70);
    }

    #[test]
    fn test_duplicate_sequence_number_rejected() {
        let err = build_ordered_replay_batches([build_da_blob(8, 80), build_da_blob(8, 81)])
            .expect_err("duplicate sequence rejects");

        assert!(matches!(
            err,
            BatchSequenceError::DuplicateUpdateSeqNo { update_seq_no }
                if update_seq_no == Seqno::new(8)
        ));
    }

    #[test]
    fn test_sequence_gap_rejected() {
        let err = build_ordered_replay_batches([build_da_blob(5, 50), build_da_blob(3, 30)])
            .expect_err("sequence gap rejects");

        assert!(matches!(
            err,
            BatchSequenceError::UpdateSeqNoGap {
                expected,
                actual,
            } if expected == Seqno::new(4)
                && actual == Seqno::new(5)
        ));
    }

    #[test]
    fn test_empty_input() {
        let batches = build_ordered_replay_batches(Vec::new()).expect("empty input orders");

        assert!(batches.is_empty());
    }
}
