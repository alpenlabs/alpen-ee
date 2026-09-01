//! Versioned value families for the EE store.
//!
//! Every structured record the store persists is declared here, so the set of
//! formats this binary can read is one file rather than a hunt through the table
//! definitions. Each family names its shipped versions ascending, the last one
//! current; [`schema`](super::schema) binds each table to a family with
//! `value as Family`, and the build fails if a table still names a superseded
//! version.
//!
//! # Adding a version
//!
//! 1. Add the new struct (usually a copy of the current one plus the change) and give it an
//!    `impl_schema_version_*!` line with the next tag.
//! 2. Add one [`UpConvert`](alpen_db_store_mdbx::UpConvert) impl from the previous version to it.
//! 3. Add the `tag => Type` entry to the family below.
//!
//! Nothing else moves: the already-shipped structs and converters are never
//! edited, because values carrying their tags are still on disk. A missing
//! `N -> N+1` converter fails the build.
//!
//! # What is not versioned
//!
//! Tables whose value is a bare identifier, an index, a counter, a presence
//! marker, or a homogeneous collection of identifiers carry no tag: there is no
//! field to add, and changing how such a table is laid out is a key-layout
//! change rather than a value re-encode. The same goes for tables storing an
//! opaque blob — an engine payload, EVM bytecode, an encoded witness — whose
//! framing is owned by whatever produced it, not by this store.

use alpen_db_store_mdbx::{impl_schema_version_borsh, impl_schema_version_cbor, versioned_value};
use alpen_ee_common::AccessedStateRecord;
use strata_db_types::{
    chunked_envelope::ChunkedEnvelopeEntry, fee_bump::TxNodeRecord, l1_broadcast::L1TxEntry,
};
use strata_paas::TaskRecordData;
use zkaleido::ProofReceiptWithMetadata;

use crate::serialization_types::{
    DBAccountStateAtEpoch, DBBatchWithStatus, DBChunkWithStatus, DBExecBlockRecord,
};

// --- Node tables ---------------------------------------------------------

impl_schema_version_borsh!(StoredAccountStateAtEpoch, DBAccountStateAtEpoch, 1);

versioned_value! {
    /// EE account state as of an OL epoch.
    pub(crate) StoredAccountStateAtEpoch {
        1 => DBAccountStateAtEpoch,
    }
}

impl_schema_version_borsh!(StoredExecBlockRecord, DBExecBlockRecord, 1);

versioned_value! {
    /// An execution block's record.
    pub(crate) StoredExecBlockRecord {
        1 => DBExecBlockRecord,
    }
}

impl_schema_version_borsh!(StoredBatch, DBBatchWithStatus, 1);

versioned_value! {
    /// A batch together with its status.
    pub(crate) StoredBatch {
        1 => DBBatchWithStatus,
    }
}

impl_schema_version_borsh!(StoredChunk, DBChunkWithStatus, 1);

versioned_value! {
    /// A chunk together with its status.
    pub(crate) StoredChunk {
        1 => DBChunkWithStatus,
    }
}

impl_schema_version_borsh!(StoredAccessedState, AccessedStateRecord, 1);

versioned_value! {
    /// The state a block's execution touched.
    pub(crate) StoredAccessedState {
        1 => AccessedStateRecord,
    }
}

// --- Prover tables -------------------------------------------------------

impl_schema_version_cbor!(StoredProverTask, TaskRecordData, 1);

versioned_value! {
    /// A prover task record.
    pub(crate) StoredProverTask {
        1 => TaskRecordData,
    }
}

impl_schema_version_borsh!(StoredProofReceipt, ProofReceiptWithMetadata, 1);

versioned_value! {
    /// A proof receipt, for both the chunk and account proof tables.
    pub(crate) StoredProofReceipt {
        1 => ProofReceiptWithMetadata,
    }
}

// --- DA-pipeline tables --------------------------------------------------

impl_schema_version_cbor!(StoredL1TxEntry, L1TxEntry, 1);

versioned_value! {
    /// An L1 broadcast transaction entry.
    pub(crate) StoredL1TxEntry {
        1 => L1TxEntry,
    }
}

impl_schema_version_cbor!(StoredTxNodeRecord, TxNodeRecord, 1);

versioned_value! {
    /// One node of an L1 transaction replacement chain.
    pub(crate) StoredTxNodeRecord {
        1 => TxNodeRecord,
    }
}

impl_schema_version_cbor!(StoredChunkedEnvelopeEntry, ChunkedEnvelopeEntry, 1);

versioned_value! {
    /// A chunked-envelope entry awaiting or tracking L1 inclusion.
    pub(crate) StoredChunkedEnvelopeEntry {
        1 => ChunkedEnvelopeEntry,
    }
}

#[cfg(test)]
mod tests {
    use alpen_db_store_mdbx::{CodecError, UpgradeCtx, VersionedValue};

    use super::*;

    /// Runs the family invariants over every family declared in this module, so
    /// a new one is covered by adding it to this list.
    macro_rules! for_each_family {
        ($check:ident) => {
            $check::<StoredAccountStateAtEpoch>();
            $check::<StoredExecBlockRecord>();
            $check::<StoredBatch>();
            $check::<StoredChunk>();
            $check::<StoredAccessedState>();
            $check::<StoredProverTask>();
            $check::<StoredProofReceipt>();
            $check::<StoredL1TxEntry>();
            $check::<StoredTxNodeRecord>();
            $check::<StoredChunkedEnvelopeEntry>();
        };
    }

    /// The declared chain must start at 1, ascend by one, and end at the version
    /// this binary writes — a gap would leave stored bytes undecodable.
    fn check_chain<F: VersionedValue>() {
        let versions = F::VERSIONS;
        assert_eq!(
            versions.first(),
            Some(&1),
            "`{}`: version chain must start at 1",
            F::FAMILY
        );
        for (position, version) in versions.iter().enumerate() {
            assert_eq!(
                *version,
                position as u8 + 1,
                "`{}`: version chain must ascend without gaps, got {versions:?}",
                F::FAMILY
            );
        }
        assert_eq!(
            versions.last(),
            Some(&F::CURRENT_VERSION),
            "`{}`: the last declared version must be the one written",
            F::FAMILY
        );
    }

    /// A value written by a newer binary must be refused, not misread.
    fn check_refuses_newer<F: VersionedValue>() {
        let newer = [F::CURRENT_VERSION + 1, 0, 0, 0, 0];
        let err = F::decode_tagged(&newer, &UpgradeCtx::detached())
            .err()
            .unwrap_or_else(|| panic!("`{}`: a newer tag decoded", F::FAMILY));
        assert!(
            matches!(err, CodecError::NewerVersion { .. }),
            "`{}`: expected a newer-version refusal, got {err:?}",
            F::FAMILY
        );
    }

    /// An empty value carries no tag and must be reported as such.
    fn check_reports_missing_tag<F: VersionedValue>() {
        let err = F::decode_tagged(&[], &UpgradeCtx::detached())
            .err()
            .unwrap_or_else(|| panic!("`{}`: empty bytes decoded", F::FAMILY));
        assert!(
            matches!(err, CodecError::MissingVersionTag { .. }),
            "`{}`: expected a missing-tag error, got {err:?}",
            F::FAMILY
        );
    }

    #[test]
    fn every_family_declares_a_gapless_chain() {
        for_each_family!(check_chain);
    }

    #[test]
    fn every_family_refuses_a_newer_version() {
        for_each_family!(check_refuses_newer);
    }

    #[test]
    fn every_family_reports_a_missing_tag() {
        for_each_family!(check_reports_missing_tag);
    }
}
