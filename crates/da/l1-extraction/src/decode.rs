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

    use super::*;
    use crate::test_utils::make_da_blob;

    const TEST_HEIGHT: u32 = 42;

    fn make_l1_ref() -> DaL1Ref {
        let completion_block = L1BlockCommitment {
            height: TEST_HEIGHT,
            blkid: BlockHash::all_zeros().to_l1_block_id(),
        };
        DaL1Ref::new(Txid::all_zeros(), completion_block)
    }

    fn build_observation(spec_version: AlpenSpecId, payload: Vec<u8>) -> DaL1Observation {
        DaL1Observation::new(make_l1_ref(), spec_version, payload)
    }

    #[test]
    fn test_payload_decodes_to_blob() {
        for spec_version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            let blob = make_da_blob(spec_version);
            let encoded = blob.encode_to_vec().expect("encode DA blob");

            let observation = build_observation(blob.spec_version, encoded.clone());
            let decoded =
                decode_observed_da_payload(&observation).expect("decode observation payload");

            assert_eq!(decoded.l1_ref(), make_l1_ref());
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
        let observation = build_observation(AlpenSpecId::V1, Vec::new());
        let err =
            decode_observed_da_payload(&observation).expect_err("invalid payload must be rejected");

        assert_eq!(err.l1_ref(), make_l1_ref());
    }
}
