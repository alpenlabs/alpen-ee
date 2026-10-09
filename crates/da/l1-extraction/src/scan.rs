//! Incrementally extracts EE DA chunked envelopes from L1 blocks.
//!
//! Entry point: [`DaScanner`]. The scanner delegates commit/reveal parsing
//! to `strata-l1-envelope-fmt`, then returns only payloads produced by the
//! configured EE sequencer with the supported DA blob marker version.

use alpen_params::AlpenSpecId;
use bitcoin::{secp256k1::XOnlyPublicKey, Txid};
use strata_identifiers::L1BlockCommitment;
use strata_l1_envelope_fmt::{
    CommitRevealParseError, PayloadParser, PayloadParserConfig, RecoveredPayload,
};
use strata_l1_txfmt::MagicBytes;
use tracing::warn;

use crate::fetch::L1BlockData;

/// L1 provenance for a recovered EE DA payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaL1Ref {
    commit_txid: Txid,
    completion_block: L1BlockCommitment,
}

impl DaL1Ref {
    /// Constructs an EE DA L1 reference.
    pub const fn new(commit_txid: Txid, completion_block: L1BlockCommitment) -> Self {
        Self {
            commit_txid,
            completion_block,
        }
    }

    /// Returns the marker-bearing commit transaction id.
    pub const fn commit_txid(&self) -> Txid {
        self.commit_txid
    }

    /// Returns the L1 block that completed payload recovery.
    pub const fn completion_block(&self) -> L1BlockCommitment {
        self.completion_block
    }
}

/// EE DA payload recovered from L1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaL1Observation {
    l1_ref: DaL1Ref,
    spec_version: AlpenSpecId,
    payload: Vec<u8>,
}

impl DaL1Observation {
    /// Constructs an EE DA L1 observation.
    pub(crate) fn new(l1_ref: DaL1Ref, spec_version: AlpenSpecId, payload: Vec<u8>) -> Self {
        Self {
            l1_ref,
            spec_version,
            payload,
        }
    }

    /// Returns the L1 provenance for this observation.
    pub const fn l1_ref(&self) -> DaL1Ref {
        self.l1_ref
    }

    /// Returns the marker-bearing commit transaction id.
    pub const fn commit_txid(&self) -> Txid {
        self.l1_ref.commit_txid()
    }

    /// Returns the spec version that defines the DA blob layout.
    pub const fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }

    /// Returns the recovered DA payload bytes.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Returns the L1 block that completed payload recovery.
    pub const fn completion_block(&self) -> L1BlockCommitment {
        self.l1_ref.completion_block()
    }
}

/// Configuration for scanning EE DA in L1 blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaScannerConfig {
    magic_bytes: MagicBytes,
    sequencer_pubkey: XOnlyPublicKey,
}

impl DaScannerConfig {
    /// Creates scanner configuration.
    pub fn new(magic_bytes: MagicBytes, sequencer_pubkey: XOnlyPublicKey) -> Self {
        Self {
            magic_bytes,
            sequencer_pubkey,
        }
    }

    /// Returns the L1 marker magic bytes.
    pub fn magic_bytes(&self) -> MagicBytes {
        self.magic_bytes
    }
}

/// Incrementally scans L1 blocks for EE DA payloads.
///
/// Full blocks are not retained. The shared parser retains incomplete commit
/// state across scanned blocks and returns payloads once their reveal set is
/// complete. The scanner authenticates recovered payloads against the configured
/// sequencer key and validates the EE DA marker-tail version before returning
/// observations completed at the scanned block. Callers must provide blocks in
/// L1 order and handle continuity and reorg validation outside the scanner.
#[derive(Debug)]
pub struct DaScanner {
    config: DaScannerConfig,
    payload_parser: PayloadParser,
}

impl DaScanner {
    /// Creates an empty EE DA scanner.
    pub fn new(config: DaScannerConfig) -> Self {
        let payload_parser_config = PayloadParserConfig::chunked_reveals(config.magic_bytes());
        let payload_parser = PayloadParser::new(payload_parser_config);
        Self {
            config,
            payload_parser,
        }
    }

    /// Scans one L1 block and returns L1 observations with completed payloads.
    pub fn scan_block(&mut self, block: &L1BlockData) -> Vec<DaL1Observation> {
        let expected_producer_pubkey = self.config.sequencer_pubkey.serialize();
        let completion_block = L1BlockCommitment {
            height: block.height(),
            blkid: block.block_id(),
        };
        let recovered_payloads = self
            .payload_parser
            .parse(block.block().txdata.iter(), block.height())
            .into_payloads();

        let mut da_observations = Vec::new();
        for recovered_payload in recovered_payloads {
            let commit_txid = recovered_payload.anchor_txid();
            let recovered_payload = match recovered_payload
                .require_producer(&expected_producer_pubkey)
            {
                Ok(recovered_payload) => recovered_payload,
                Err(err) => {
                    warn!(%commit_txid, %err, "ignoring EE DA payload from unexpected producer");
                    continue;
                }
            };

            let version = match read_da_blob_version(&recovered_payload) {
                Ok(version) => version,
                Err(err) => {
                    warn!(%commit_txid, %err, "ignoring EE DA payload with invalid marker tail");
                    continue;
                }
            };

            let spec_version = match u16::try_from(version)
                .ok()
                .and_then(|version| AlpenSpecId::try_from(version).ok())
            {
                Some(spec_version) => spec_version,
                None => {
                    warn!(
                        %commit_txid,
                        version,
                        "ignoring EE DA payload with unsupported version"
                    );
                    continue;
                }
            };

            let l1_ref = DaL1Ref::new(commit_txid, completion_block);
            let observation =
                DaL1Observation::new(l1_ref, spec_version, recovered_payload.into_payload());
            da_observations.push(observation);
        }

        da_observations
    }
}

fn read_da_blob_version(
    recovered_payload: &RecoveredPayload,
) -> Result<u32, CommitRevealParseError> {
    let version = recovered_payload.tail_array::<4>()?;
    Ok(u32::from_be_bytes(*version))
}

#[cfg(test)]
mod tests {
    use alpen_da_types::da_blob_version;
    use strata_l1_envelope_fmt::test_utils as commit_reveal_fixtures;

    use super::*;
    use crate::test_utils::{
        build_l1_block_data, make_alpen_magic_bytes, make_sequencer_pubkey, SEQUENCER_KEY_SEED,
    };

    const NON_SEQUENCER_KEY_SEED: u8 = 8;

    fn v0_blob_version() -> u32 {
        da_blob_version(AlpenSpecId::V0)
    }

    fn scan_preloaded_l1_blocks(
        blocks: &[L1BlockData],
        magic_bytes: MagicBytes,
        sequencer_pubkey: XOnlyPublicKey,
    ) -> Vec<DaL1Observation> {
        let config = DaScannerConfig::new(magic_bytes, sequencer_pubkey);
        let mut scanner = DaScanner::new(config);
        let mut da_observations = Vec::new();
        for block in blocks {
            let new_observations = scanner.scan_block(block);
            da_observations.extend(new_observations);
        }
        da_observations
    }

    fn make_other_magic_bytes() -> MagicBytes {
        "OTHR".parse().expect("valid ASCII magic")
    }

    #[test]
    fn test_wrong_magic_rejected() {
        let set = commit_reveal_fixtures::build_commit_reveal_set(
            &make_other_magic_bytes(),
            &v0_blob_version().to_be_bytes(),
            &[b"chunk".as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let blocks = vec![
            build_l1_block_data(10, vec![set.commit]),
            build_l1_block_data(11, set.reveals),
        ];

        let observations =
            scan_preloaded_l1_blocks(&blocks, make_alpen_magic_bytes(), make_sequencer_pubkey());

        assert!(observations.is_empty());
    }

    #[test]
    fn test_wrong_producer_rejected() {
        let set = commit_reveal_fixtures::build_commit_reveal_set(
            &make_alpen_magic_bytes(),
            &v0_blob_version().to_be_bytes(),
            &[b"chunk".as_slice()],
            NON_SEQUENCER_KEY_SEED,
        );
        let blocks = vec![
            build_l1_block_data(10, vec![set.commit]),
            build_l1_block_data(11, set.reveals),
        ];

        let observations =
            scan_preloaded_l1_blocks(&blocks, make_alpen_magic_bytes(), make_sequencer_pubkey());

        assert!(observations.is_empty());
    }

    #[test]
    fn test_malformed_version_rejected() {
        let set = commit_reveal_fixtures::build_commit_reveal_set(
            &make_alpen_magic_bytes(),
            b"too-long",
            &[b"chunk".as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let blocks = vec![
            build_l1_block_data(10, vec![set.commit]),
            build_l1_block_data(11, set.reveals),
        ];

        let observations =
            scan_preloaded_l1_blocks(&blocks, make_alpen_magic_bytes(), make_sequencer_pubkey());

        assert!(observations.is_empty());
    }

    #[test]
    fn test_unsupported_version_rejected() {
        let unsupported_version = 2u32;
        let set = commit_reveal_fixtures::build_commit_reveal_set(
            &make_alpen_magic_bytes(),
            &unsupported_version.to_be_bytes(),
            &[unsupported_version.to_be_bytes().as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let blocks = vec![
            build_l1_block_data(10, vec![set.commit]),
            build_l1_block_data(11, set.reveals),
        ];

        let observations =
            scan_preloaded_l1_blocks(&blocks, make_alpen_magic_bytes(), make_sequencer_pubkey());

        assert!(observations.is_empty());
    }

    #[test]
    fn test_rejected_payload_does_not_block_valid_payload() {
        let unsupported_version = 2u32;
        let rejected = commit_reveal_fixtures::build_commit_reveal_set(
            &make_alpen_magic_bytes(),
            &unsupported_version.to_be_bytes(),
            &[unsupported_version.to_be_bytes().as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let valid = commit_reveal_fixtures::build_commit_reveal_set(
            &make_alpen_magic_bytes(),
            &v0_blob_version().to_be_bytes(),
            &[b"chunk".as_slice(), b"tail".as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let valid_commit_txid = valid.commit.compute_txid();
        let mut reveals = rejected.reveals;
        reveals.extend(valid.reveals);
        let blocks = vec![
            build_l1_block_data(10, vec![rejected.commit, valid.commit]),
            build_l1_block_data(11, reveals),
        ];

        let observations =
            scan_preloaded_l1_blocks(&blocks, make_alpen_magic_bytes(), make_sequencer_pubkey());

        assert_eq!(observations.len(), 1);
        let observation = observations.first().expect("one valid observation");
        assert_eq!(observation.commit_txid(), valid_commit_txid);
        assert_eq!(observation.payload(), b"chunktail");
    }

    #[test]
    fn test_payload_recovered_across_blocks() {
        let set = commit_reveal_fixtures::build_commit_reveal_set(
            &make_alpen_magic_bytes(),
            &v0_blob_version().to_be_bytes(),
            &[b"chunk".as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let commit_txid = set.commit.compute_txid();
        let blocks = vec![
            build_l1_block_data(10, vec![set.commit]),
            build_l1_block_data(11, set.reveals),
        ];

        let observations =
            scan_preloaded_l1_blocks(&blocks, make_alpen_magic_bytes(), make_sequencer_pubkey());

        assert_eq!(observations.len(), 1);
        let observation = observations.first().expect("one observation");
        assert_eq!(observation.commit_txid(), commit_txid);
        assert_eq!(observation.payload(), b"chunk");
        assert_eq!(observation.completion_block().height(), 11);
    }

    #[test]
    fn test_multiple_payloads_recovered() {
        let set0 = commit_reveal_fixtures::build_commit_reveal_set(
            &make_alpen_magic_bytes(),
            &v0_blob_version().to_be_bytes(),
            &[b"chunk-0".as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let set1 = commit_reveal_fixtures::build_commit_reveal_set(
            &make_alpen_magic_bytes(),
            &da_blob_version(AlpenSpecId::V1).to_be_bytes(),
            &[b"chunk-1a".as_slice(), b"chunk-1b".as_slice()],
            SEQUENCER_KEY_SEED,
        );
        let commit0_txid = set0.commit.compute_txid();
        let commit1_txid = set1.commit.compute_txid();
        let mut reveals = set0.reveals;
        reveals.extend(set1.reveals);
        let blocks = vec![
            build_l1_block_data(10, vec![set0.commit, set1.commit]),
            build_l1_block_data(11, reveals),
        ];

        let observations =
            scan_preloaded_l1_blocks(&blocks, make_alpen_magic_bytes(), make_sequencer_pubkey());

        assert_eq!(observations.len(), 2);
        assert!(observations.iter().any(|observation| {
            observation.commit_txid() == commit0_txid
                && observation.spec_version() == AlpenSpecId::V0
                && observation.payload() == b"chunk-0"
        }));
        assert!(observations.iter().any(|observation| {
            observation.commit_txid() == commit1_txid
                && observation.spec_version() == AlpenSpecId::V1
                && observation.payload() == b"chunk-1achunk-1b"
        }));
    }
}
