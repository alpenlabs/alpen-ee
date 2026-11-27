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

/// Transition data for one OL-accepted EE account update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EeAccountUpdateData {
    update_seq_no: Seqno,
    prev_next_msg_idx: u64,
    new_next_msg_idx: u64,
    extra_data: UpdateExtraData,
}

impl EeAccountUpdateData {
    /// Creates decoded transition data for an OL-accepted EE account update.
    ///
    /// # Errors
    ///
    /// Returns an error when the update moves the inbox cursor backward.
    pub fn try_new(
        update_seq_no: Seqno,
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
            prev_next_msg_idx,
            new_next_msg_idx,
            extra_data,
        })
    }

    /// Returns the accepted account update sequence number.
    pub fn update_seq_no(&self) -> Seqno {
        self.update_seq_no
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

/// An EE account update with its OL-published inner-state root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EeAccountUpdateManifest {
    update: EeAccountUpdateData,
    expected_inner_state_root: Hash,
}

impl EeAccountUpdateManifest {
    /// Creates a decoded manifest for an OL-attested EE account update.
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
        let update = EeAccountUpdateData::try_new(
            update_seq_no,
            prev_next_msg_idx,
            new_next_msg_idx,
            extra_data,
        )?;
        Ok(Self {
            update,
            expected_inner_state_root,
        })
    }

    /// Returns the account transition data.
    pub fn update(&self) -> &EeAccountUpdateData {
        &self.update
    }

    /// Returns the OL-published EE account inner-state root.
    pub fn expected_inner_state_root(&self) -> Hash {
        self.expected_inner_state_root
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
        let err = EeAccountUpdateData::try_new(
            Seqno::new(4),
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
