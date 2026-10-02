//! Composes EE DA scanning and decoding for individual L1 blocks.

use crate::{
    decode::{decode_observed_da_payload, DaDecodeError, RecoveredDaBlob},
    fetch::L1BlockData,
    scan::{EeDaScanner, EeDaScannerConfig},
};

/// Stateful extractor for EE DA blobs published on L1.
///
/// Callers must supply blocks in L1 order and handle fetching, continuity,
/// reorg safety, and persistence outside the extractor.
#[derive(Debug)]
pub struct EeDaExtractor {
    scanner: EeDaScanner,
}

impl EeDaExtractor {
    /// Creates an empty extractor.
    pub fn new(config: EeDaScannerConfig) -> Self {
        Self {
            scanner: EeDaScanner::new(config),
        }
    }

    /// Processes one L1 block and returns EE DA blobs completed while processing it.
    pub fn process_block(
        &mut self,
        block: &L1BlockData,
    ) -> Result<Vec<RecoveredDaBlob>, DaDecodeError> {
        self.scanner
            .scan_block(block)
            .iter()
            .map(decode_observed_da_payload)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use alpen_ee_da_types::DA_BLOB_VERSION;
    use strata_codec::encode_to_vec;
    use strata_l1_envelope_fmt::test_utils as commit_reveal_fixtures;

    use super::*;
    use crate::test_utils::{
        build_l1_block_data, make_alpen_magic_bytes, make_da_blob, make_sequencer_pubkey,
        SEQUENCER_KEY_SEED,
    };

    #[test]
    fn test_blob_extracted_across_blocks() {
        let blob = make_da_blob();
        let encoded = encode_to_vec(&blob).expect("encode DA blob");
        let set = commit_reveal_fixtures::build_commit_reveal_set(
            &make_alpen_magic_bytes(),
            &DA_BLOB_VERSION.to_be_bytes(),
            &[encoded.as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let commit_txid = set.commit.compute_txid();
        let commit_block = build_l1_block_data(10, vec![set.commit]);
        let reveal_block = build_l1_block_data(11, set.reveals);
        let config = EeDaScannerConfig::new(make_alpen_magic_bytes(), make_sequencer_pubkey());
        let mut extractor = EeDaExtractor::new(config);

        assert!(extractor
            .process_block(&commit_block)
            .expect("process commit block")
            .is_empty());
        let recovered = extractor
            .process_block(&reveal_block)
            .expect("process reveal block");

        assert_eq!(recovered.len(), 1);
        let recovered = recovered.first().expect("one recovered blob");
        assert_eq!(recovered.l1_ref().commit_txid(), commit_txid);
        assert_eq!(recovered.l1_ref().completion_block().height(), 11);
        assert_eq!(
            encode_to_vec(recovered.blob()).expect("encode recovered DA blob"),
            encoded
        );
    }
}
