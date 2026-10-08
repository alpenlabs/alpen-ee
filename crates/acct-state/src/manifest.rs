use alpen_acct_types::UpdateExtraData;
use strata_acct_types::Hash;
use strata_snark_acct_types::Seqno;
use thiserror::Error;

/// Failure to construct an EE account update manifest.
#[derive(Debug, Error)]
pub enum EeAccountUpdateManifestError {
    /// The update manifest moves the inbox cursor backward.
    #[error(
        "inbox cursor regression at update sequence number {} (previous {previous}, got {current})",
        .update_seq_no.inner()
    )]
    InboxCursorRegression {
        update_seq_no: Seqno,
        previous: u64,
        current: u64,
    },
}

/// OL-published data for one accepted EE account update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EeAccountUpdateManifest {
    update_seq_no: Seqno,
    expected_inner_state_root: Hash,
    prev_next_msg_idx: u64,
    new_next_msg_idx: u64,
    extra_data: UpdateExtraData,
}

impl EeAccountUpdateManifest {
    /// Creates a decoded manifest for an OL-accepted EE account update.
    ///
    /// # Errors
    ///
    /// Returns an error when the update moves the inbox cursor backward.
    pub fn try_new(
        update_seq_no: Seqno,
        expected_inner_state_root: Hash,
        prev_next_msg_idx: u64,
        new_next_msg_idx: u64,
        extra_data: UpdateExtraData,
    ) -> Result<Self, EeAccountUpdateManifestError> {
        if new_next_msg_idx < prev_next_msg_idx {
            return Err(EeAccountUpdateManifestError::InboxCursorRegression {
                update_seq_no,
                previous: prev_next_msg_idx,
                current: new_next_msg_idx,
            });
        }

        Ok(Self {
            update_seq_no,
            expected_inner_state_root,
            prev_next_msg_idx,
            new_next_msg_idx,
            extra_data,
        })
    }

    /// Returns the accepted account update sequence number.
    pub fn update_seq_no(&self) -> Seqno {
        self.update_seq_no
    }

    /// Returns the OL-published EE account inner-state root.
    pub fn expected_inner_state_root(&self) -> Hash {
        self.expected_inner_state_root
    }

    /// Returns the inbox cursor before this update.
    pub fn prev_next_msg_idx(&self) -> u64 {
        self.prev_next_msg_idx
    }

    /// Returns the inbox cursor after this update.
    pub fn new_next_msg_idx(&self) -> u64 {
        self.new_next_msg_idx
    }

    /// Returns the number of inbox messages consumed by the update.
    pub(crate) fn inbox_message_count(&self) -> u64 {
        self.new_next_msg_idx - self.prev_next_msg_idx
    }

    /// Returns the EE-specific update data.
    pub fn extra_data(&self) -> &UpdateExtraData {
        &self.extra_data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hash(seed: u8) -> Hash {
        Hash::new([seed; 32])
    }

    #[test]
    fn test_inbox_cursor_regression_rejected() {
        let err = EeAccountUpdateManifest::try_new(
            Seqno::new(4),
            make_hash(1),
            11,
            10,
            UpdateExtraData::new(make_hash(2), make_hash(3), 0, 0),
        )
        .expect_err("inbox cursor regression must fail");

        assert!(matches!(
            err,
            EeAccountUpdateManifestError::InboxCursorRegression {
                update_seq_no,
                previous: 11,
                current: 10,
            } if update_seq_no == Seqno::new(4)
        ));
    }
}
