//! Decodes authenticated EE DA payloads into typed blobs.

use alpen_ee_da_types::{decode_da_blob, DaBlob};
use strata_codec::CodecError;
use thiserror::Error;

use crate::scan::{EeDaL1Observation, EeDaL1Ref};

/// EE DA blob decoded from a payload recovered on L1.
#[derive(Debug, Clone)]
pub struct RecoveredDaBlob {
    l1_ref: EeDaL1Ref,
    blob: DaBlob,
}

impl RecoveredDaBlob {
    pub fn new(l1_ref: EeDaL1Ref, blob: DaBlob) -> Self {
        Self { l1_ref, blob }
    }

    /// Returns the L1 provenance for this blob.
    pub const fn l1_ref(&self) -> EeDaL1Ref {
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
    l1_ref: EeDaL1Ref,
    #[source]
    source: CodecError,
}

impl DaDecodeError {
    /// Returns the L1 provenance of the payload that failed to decode.
    pub const fn l1_ref(&self) -> EeDaL1Ref {
        self.l1_ref
    }
}

/// Decodes a [`DaBlob`] from an authenticated L1 observation's payload.
pub fn decode_observed_da_payload(
    observation: &EeDaL1Observation,
) -> Result<RecoveredDaBlob, DaDecodeError> {
    let l1_ref = observation.l1_ref();
    let blob =
        decode_da_blob(observation.payload()).map_err(|source| DaDecodeError { l1_ref, source })?;
    Ok(RecoveredDaBlob::new(l1_ref, blob))
}

#[cfg(test)]
mod tests {
    use alpen_ee_da_types::EvmHeaderSummary;
    use bitcoin::{hashes::Hash, BlockHash, Txid};
    use strata_btc_types::BlockHashExt;
    use strata_codec::encode_to_vec;
    use strata_identifiers::L1BlockCommitment;

    use super::*;

    const TEST_HEIGHT: u32 = 42;

    fn make_l1_ref() -> EeDaL1Ref {
        let completion_block = L1BlockCommitment {
            height: TEST_HEIGHT,
            blkid: BlockHash::all_zeros().to_l1_block_id(),
        };
        EeDaL1Ref::new(Txid::all_zeros(), completion_block)
    }

    fn make_da_blob() -> DaBlob {
        DaBlob {
            update_seq_no: 3,
            evm_header: EvmHeaderSummary {
                block_num: 9,
                timestamp: 1_700_000_000,
                base_fee: 100,
                gas_used: 21_000,
                gas_limit: 36_000_000,
            },
            state_diff: Default::default(),
        }
    }

    fn build_observation(payload: Vec<u8>) -> EeDaL1Observation {
        EeDaL1Observation::new(make_l1_ref(), payload)
    }

    #[test]
    fn test_payload_decodes_to_blob() {
        let blob = make_da_blob();
        let encoded = encode_to_vec(&blob).expect("encode DA blob");

        let observation = build_observation(encoded.clone());
        let decoded = decode_observed_da_payload(&observation).expect("decode observation payload");

        assert_eq!(decoded.l1_ref(), make_l1_ref());
        // DaBlob is not Eq, so compare via re-encoding.
        assert_eq!(
            encode_to_vec(decoded.blob()).expect("encode decoded DA blob"),
            encoded
        );
    }

    #[test]
    fn test_decode_failure_preserves_l1_ref() {
        let observation = build_observation(Vec::new());
        let err =
            decode_observed_da_payload(&observation).expect_err("invalid payload must be rejected");

        assert_eq!(err.l1_ref(), make_l1_ref());
    }
}
