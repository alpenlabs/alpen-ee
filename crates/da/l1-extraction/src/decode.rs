//! Decodes authenticated EE DA payloads into typed blobs.

use alpen_da_types::{decode_da_blob, DaBlob};
use strata_codec::CodecError;
use thiserror::Error;

use crate::scan::{DaL1Observation, DaL1Ref};

/// EE DA blob decoded from a payload recovered on L1.
#[derive(Debug, Clone)]
pub struct RecoveredDaBlob {
    l1_ref: DaL1Ref,
    blob: DaBlob,
}

impl RecoveredDaBlob {
    pub fn new(l1_ref: DaL1Ref, blob: DaBlob) -> Self {
        Self { l1_ref, blob }
    }

    /// Returns the L1 provenance for this blob.
    pub const fn l1_ref(&self) -> DaL1Ref {
        self.l1_ref
    }

    /// Returns the decoded DA blob.
    pub const fn blob(&self) -> &DaBlob {
        &self.blob
    }

    /// Consumes this value, returning the decoded DA blob.
    pub fn into_blob(self) -> DaBlob {
        self.blob
    }
}

/// Failure to decode an authenticated EE DA payload.
#[derive(Debug, Error)]
#[error(
    "failed to decode EE DA blob from commit {}: {source}",
    .l1_ref.commit_txid()
)]
pub struct DaDecodeError {
    l1_ref: DaL1Ref,
    #[source]
    source: CodecError,
}

impl DaDecodeError {
    /// Returns the L1 provenance of the payload that failed to decode.
    pub const fn l1_ref(&self) -> DaL1Ref {
        self.l1_ref
    }
}

/// Decodes a [`DaBlob`] from an authenticated L1 observation's payload.
pub fn decode_observed_da_payload(
    observation: &DaL1Observation,
) -> Result<RecoveredDaBlob, DaDecodeError> {
    let l1_ref = observation.l1_ref();
    let blob = decode_da_blob(observation.payload(), observation.spec_version())
        .map_err(|source| DaDecodeError { l1_ref, source })?;
    Ok(RecoveredDaBlob::new(l1_ref, blob))
}

#[cfg(test)]
mod tests {
    use alpen_params::AlpenSpecId;
    use bitcoin::{hashes::Hash, BlockHash, Txid};
    use strata_btc_types::BlockHashExt;
    use strata_identifiers::L1BlockCommitment;
    use strata_l1_envelope_fmt::L1TxRef;

    use super::*;
    use crate::test_utils::build_da_blob;

    const TEST_HEIGHT: u32 = 42;
    const TEST_L1_REF_SEED: u8 = 1;
    const TEST_UPDATE_SEQ_NO: u64 = 3;
    const TEST_BLOCK_NUM: u64 = 9;
    const TEST_TIMESTAMP: u64 = 1_700_000_000;

    /// Builds an L1 reference whose every identifier derives from `seed`.
    fn build_l1_ref(seed: u8) -> DaL1Ref {
        // A payload's commit is confirmed before the reveals that complete it.
        let commit_txref = L1TxRef::new(
            Txid::from_byte_array([seed; 32]),
            L1BlockCommitment {
                height: TEST_HEIGHT - 1,
                blkid: BlockHash::from_byte_array([seed.wrapping_add(1); 32]).to_l1_block_id(),
            },
        );
        let completion_block = L1BlockCommitment {
            height: TEST_HEIGHT,
            blkid: BlockHash::from_byte_array([seed.wrapping_add(2); 32]).to_l1_block_id(),
        };
        DaL1Ref::new(commit_txref, completion_block)
    }

    #[test]
    fn test_payload_decodes_to_blob() {
        for spec_version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            let blob = build_da_blob(
                spec_version,
                TEST_UPDATE_SEQ_NO,
                TEST_BLOCK_NUM,
                TEST_TIMESTAMP,
            );
            let encoded = blob.encode_to_vec().expect("encode DA blob");

            let l1_ref = build_l1_ref(TEST_L1_REF_SEED);
            let observation = DaL1Observation::new(l1_ref, blob.spec_version, encoded.clone());
            let decoded =
                decode_observed_da_payload(&observation).expect("decode observation payload");

            assert_eq!(decoded.l1_ref(), l1_ref);
            // DaBlob is not Eq, so compare via re-encoding.
            assert_eq!(
                decoded
                    .blob()
                    .encode_to_vec()
                    .expect("encode decoded DA blob"),
                encoded
            );
        }
    }

    #[test]
    fn test_decode_failure_preserves_l1_ref() {
        let l1_ref = build_l1_ref(TEST_L1_REF_SEED);
        let observation = DaL1Observation::new(l1_ref, AlpenSpecId::V1, Vec::new());
        let err =
            decode_observed_da_payload(&observation).expect_err("invalid payload must be rejected");

        assert_eq!(err.l1_ref(), l1_ref);
    }
}
