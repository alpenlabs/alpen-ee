use alpen_acct_types::EnvError;
use strata_acct_types::Hash;
use strata_snark_acct_types::Seqno;
use thiserror::Error;

/// Failure to reconstruct an EE account update.
#[derive(Debug, Error)]
pub enum EeAccountReconstructionError {
    /// The reconstructed inner-state root differs from the root published by OL.
    #[error(
        "EE account inner-state root mismatch at update sequence number {} (expected {expected}, got {actual})",
        .update_seq_no.inner()
    )]
    InnerStateRootMismatch {
        update_seq_no: Seqno,
        expected: Hash,
        actual: Hash,
    },

    /// The number of supplied messages differs from the manifest's inbox range length.
    #[error(
        "inbox message count mismatch at update sequence number {} (expected {expected}, got {actual})",
        .update_seq_no.inner()
    )]
    InboxMessageCountMismatch {
        update_seq_no: Seqno,
        expected: u64,
        actual: u64,
    },

    /// Applying the fetched inbox messages to the candidate account state failed.
    #[error(
        "inbox message application failed at update sequence number {}: {source}",
        .update_seq_no.inner()
    )]
    ApplyInboxMessages {
        update_seq_no: Seqno,
        #[source]
        source: EnvError,
    },

    /// The manifest consumes more pending inputs than the account state contains.
    #[error(
        "pending input underflow at update sequence number {} (requested {requested}, available {available})",
        .update_seq_no.inner()
    )]
    PendingInputUnderflow {
        update_seq_no: Seqno,
        requested: u32,
        available: usize,
    },

    /// The manifest consumes more pending forced inclusions than the account state contains.
    #[error(
        "pending forced-inclusion underflow at update sequence number {} (requested {requested}, available {available})",
        .update_seq_no.inner()
    )]
    PendingFinclUnderflow {
        update_seq_no: Seqno,
        requested: u32,
        available: usize,
    },
}
