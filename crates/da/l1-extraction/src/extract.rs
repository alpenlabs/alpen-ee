//! Composes EE DA scanning and decoding for individual L1 blocks.

use bitcoin::secp256k1::XOnlyPublicKey;
use strata_l1_txfmt::MagicBytes;
use tracing::warn;

use crate::{
    decode::{decode_observed_da_payload, RecoveredDaBlob},
    fetch::L1BlockData,
    scan::{DaScanner, DaScannerConfig},
};

/// Stateful extractor for EE DA blobs published on L1.
///
/// Callers must supply blocks in L1 order and handle fetching, continuity,
/// reorg safety, and persistence outside the extractor.
#[derive(Debug)]
pub struct DaExtractor {
    scanner: DaScanner,
}

impl DaExtractor {
    /// Creates an empty extractor.
    pub fn new(magic_bytes: MagicBytes, sequencer_pubkey: XOnlyPublicKey) -> Self {
        let config = DaScannerConfig::new(magic_bytes, sequencer_pubkey);
        Self {
            scanner: DaScanner::new(config),
        }
    }

    /// Processes one L1 block and returns EE DA blobs completed while processing it.
    ///
    /// A payload that fails to decode is logged and skipped. Its update sequence
    /// number lives inside the payload, so a rejected payload cannot be placed in
    /// the replay sequence and nothing downstream can act on it; the absent
    /// sequence surfaces as an ordinary gap. Rejecting one payload never
    /// discards the others completed in the same block.
    pub fn process_block(&mut self, block: &L1BlockData) -> Vec<RecoveredDaBlob> {
        self.scanner
            .scan_block(block)
            .iter()
            .filter_map(
                |observation| match decode_observed_da_payload(observation) {
                    Ok(recovered) => Some(recovered),
                    Err(error) => {
                        warn!(
                            commit_txid = %error.l1_ref().commit_txid(),
                            %error,
                            "ignoring EE DA payload that failed to decode"
                        );
                        None
                    }
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use alpen_da_types::da_blob_version;
    use alpen_params::AlpenSpecId;
    use strata_l1_envelope_fmt::test_utils as commit_reveal_fixtures;

    use super::*;
    use crate::test_utils::{
        build_l1_block_data, make_alpen_magic_bytes, make_da_blob, make_sequencer_pubkey,
        SEQUENCER_KEY_SEED,
    };

    #[test]
    fn test_blob_extracted_across_blocks() {
        let magic_bytes = make_alpen_magic_bytes();
        let da_blob = make_da_blob(AlpenSpecId::V1);
        let encoded_blob = da_blob.encode_to_vec().expect("encode DA blob");
        let set = commit_reveal_fixtures::build_commit_reveal_set(
            &magic_bytes,
            &da_blob_version(da_blob.spec_version).to_be_bytes(),
            &[encoded_blob.as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let commit_txid = set.commit.compute_txid();
        let commit_block = build_l1_block_data(10, vec![set.commit]);
        let reveal_block = build_l1_block_data(11, set.reveals);
        let mut extractor = DaExtractor::new(magic_bytes, make_sequencer_pubkey());

        assert!(extractor.process_block(&commit_block).is_empty());
        let recovered_blobs = extractor.process_block(&reveal_block);

        assert_eq!(recovered_blobs.len(), 1);
        let recovered_blob = recovered_blobs.first().expect("one recovered blob");
        assert_eq!(recovered_blob.l1_ref().commit_txid(), commit_txid);
        assert_eq!(recovered_blob.l1_ref().completion_block().height(), 11);
        assert_eq!(
            recovered_blob
                .blob()
                .encode_to_vec()
                .expect("encode recovered DA blob"),
            encoded_blob
        );
    }

    #[test]
    fn test_undecodable_payload_does_not_block_decodable_blob() {
        let magic_bytes = make_alpen_magic_bytes();
        let da_blob = make_da_blob(AlpenSpecId::V0);
        let encoded_blob = da_blob.encode_to_vec().expect("encode DA blob");
        // The fixture's commit tx varies with the chunk count, not the chunk
        // bytes, so the two sets must differ in how many chunks they carry.
        let (head, tail) = encoded_blob.split_at(encoded_blob.len() / 2);
        let undecodable_set = commit_reveal_fixtures::build_commit_reveal_set(
            &magic_bytes,
            &da_blob_version(da_blob.spec_version).to_be_bytes(),
            &[b"not a DA blob".as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let decodable_set = commit_reveal_fixtures::build_commit_reveal_set(
            &magic_bytes,
            &da_blob_version(da_blob.spec_version).to_be_bytes(),
            &[head, tail],
            SEQUENCER_KEY_SEED,
        );
        let commit_txid = decodable_set.commit.compute_txid();
        let mut reveals = undecodable_set.reveals;
        reveals.extend(decodable_set.reveals);
        let commit_block =
            build_l1_block_data(10, vec![undecodable_set.commit, decodable_set.commit]);
        let reveal_block = build_l1_block_data(11, reveals);
        let mut extractor = DaExtractor::new(magic_bytes, make_sequencer_pubkey());

        assert!(extractor.process_block(&commit_block).is_empty());
        let recovered = extractor.process_block(&reveal_block);

        assert_eq!(recovered.len(), 1);
        let recovered_blob = recovered.first().expect("one recovered blob");
        assert_eq!(recovered_blob.l1_ref().commit_txid(), commit_txid);
        assert_eq!(
            recovered_blob
                .blob()
                .encode_to_vec()
                .expect("encode recovered DA blob"),
            encoded_blob
        );
    }
}
