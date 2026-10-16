use alpen_da_types::{da_blob_version, DaBlob, EE_DA_MAGIC_BYTES};
use alpen_params::AlpenSpecId;
use bitcoin::Transaction;
use strata_codec::CodecError;
use strata_codec_utils::ChunkIterDecoder;
use strata_identifiers::L1Height;
use strata_l1_envelope_fmt::{CommitRevealParseError, PayloadParser, PayloadParserConfig};

/// EE DA transactions included in one L1 block.
pub(crate) struct L1DaTransactions {
    height: L1Height,
    transactions: Vec<Transaction>,
}

impl L1DaTransactions {
    /// Creates an L1 DA transaction set.
    pub(crate) fn new(height: L1Height, transactions: Vec<Transaction>) -> Self {
        Self {
            height,
            transactions,
        }
    }
}

/// Errors raised while recovering one EE DA blob from the L1 blocks carrying a
/// batch's DA, supplied in ascending height order.
#[derive(Debug, thiserror::Error)]
pub enum DaBlobRecoveryError {
    #[error("no complete EE DA payload found")]
    MissingPayload,

    /// The supplied blocks must carry one batch's DA; a second payload means they do not.
    #[error("multiple complete EE DA payloads found")]
    MultiplePayloads,

    #[error("non-increasing L1 block height (previous {previous}, got {current})")]
    NonIncreasingBlockHeight {
        previous: L1Height,
        current: L1Height,
    },

    #[error("invalid EE DA marker tail: {source}")]
    MarkerTail { source: CommitRevealParseError },

    #[error("unsupported EE DA version {actual} (expected {expected})")]
    UnsupportedVersion { expected: u32, actual: u32 },

    #[error("EE DA blob decode failed: {source}")]
    Codec { source: CodecError },
}

/// Recovers exactly one supported EE DA blob from ordered L1 block transactions.
pub(crate) fn recover_ee_da_blob(
    da_txs_by_block: impl IntoIterator<Item = L1DaTransactions>,
    spec_version: AlpenSpecId,
) -> Result<DaBlob, DaBlobRecoveryError> {
    let config = PayloadParserConfig::chunked_reveals(EE_DA_MAGIC_BYTES.into());
    let mut parser = PayloadParser::new(config);
    let mut recovered_payload = None;
    let mut last_block_height = None;

    for block_da_txs in da_txs_by_block {
        let block_height = block_da_txs.height;
        if let Some(previous) = last_block_height {
            if block_height <= previous {
                return Err(DaBlobRecoveryError::NonIncreasingBlockHeight {
                    previous,
                    current: block_height,
                });
            }
        }
        last_block_height = Some(block_height);

        for payload in parser
            .parse(&block_da_txs.transactions, block_height)
            .into_payloads()
        {
            if recovered_payload.replace(payload).is_some() {
                return Err(DaBlobRecoveryError::MultiplePayloads);
            }
        }
    }

    let payload = recovered_payload.ok_or(DaBlobRecoveryError::MissingPayload)?;
    let version = payload
        .tail_array::<4>()
        .map(|bytes| u32::from_be_bytes(*bytes))
        .map_err(|source| DaBlobRecoveryError::MarkerTail { source })?;
    let expected_version = da_blob_version(spec_version);
    if version != expected_version {
        return Err(DaBlobRecoveryError::UnsupportedVersion {
            expected: expected_version,
            actual: version,
        });
    }

    let mut decoder = ChunkIterDecoder::new(payload.chunks().iter().map(Vec::as_slice));
    let blob = DaBlob::decode(spec_version, &mut decoder)
        .map_err(|source| DaBlobRecoveryError::Codec { source })?;
    if !decoder.is_exhausted() {
        return Err(DaBlobRecoveryError::Codec {
            source: CodecError::ExtraInput,
        });
    }

    Ok(blob)
}

#[cfg(test)]
mod tests {
    use alpen_da_types::EvmHeaderSummary;
    use alpen_params::HeaderExtra;
    use alpen_reth_statediff::BatchStateDiff;
    use strata_l1_envelope_fmt::test_utils::build_commit_reveal_set;

    use super::*;

    const SPEC_VERSION: AlpenSpecId = AlpenSpecId::V1;

    fn make_blob(update_seq_no: u64) -> DaBlob {
        DaBlob {
            spec_version: SPEC_VERSION,
            update_seq_no,
            evm_header: EvmHeaderSummary {
                block_num: 10,
                timestamp: 1_700_000_000,
                base_fee: 100,
                gas_used: 21_000,
                gas_limit: 36_000_000,
                da_rate: HeaderExtra::new(SPEC_VERSION, 2_500_000_000).da_rate(),
            },
            state_diff: BatchStateDiff::new(),
        }
    }

    #[test]
    fn test_cross_block_payload_recovers_blob() {
        let expected = make_blob(7);
        let encoded = expected.encode_to_vec().expect("blob encodes");
        let txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &da_blob_version(SPEC_VERSION).to_be_bytes(),
            &[encoded],
            7,
        );
        let blocks = [
            L1DaTransactions::new(42, vec![txs.commit]),
            L1DaTransactions::new(43, txs.reveals),
        ];

        let actual = recover_ee_da_blob(blocks, SPEC_VERSION).expect("blob recovers");

        assert_eq!(
            actual.encode_to_vec().expect("decoded blob encodes"),
            expected.encode_to_vec().expect("expected blob encodes")
        );
    }

    #[test]
    fn test_incomplete_payload_rejected() {
        let encoded = make_blob(7).encode_to_vec().expect("blob encodes");
        let txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &da_blob_version(SPEC_VERSION).to_be_bytes(),
            &[encoded],
            7,
        );
        assert!(matches!(
            recover_ee_da_blob([L1DaTransactions::new(42, vec![txs.commit])], SPEC_VERSION,),
            Err(DaBlobRecoveryError::MissingPayload)
        ));
    }

    #[test]
    fn test_unsupported_version_rejected() {
        let encoded = make_blob(7).encode_to_vec().expect("blob encodes");
        let unsupported_version = da_blob_version(SPEC_VERSION) + 1;
        let txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &unsupported_version.to_be_bytes(),
            &[encoded],
            7,
        );
        let mut transactions = vec![txs.commit];
        transactions.extend(txs.reveals);
        assert!(matches!(
            recover_ee_da_blob([L1DaTransactions::new(42, transactions)], SPEC_VERSION,),
            Err(DaBlobRecoveryError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn test_multiple_payloads_rejected() {
        let first = make_blob(7).encode_to_vec().expect("first blob encodes");
        let second = make_blob(8).encode_to_vec().expect("second blob encodes");
        let first_txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &da_blob_version(SPEC_VERSION).to_be_bytes(),
            &[first],
            7,
        );
        let second_txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &da_blob_version(SPEC_VERSION).to_be_bytes(),
            &[&second[..1], &second[1..]],
            8,
        );
        let mut transactions = vec![first_txs.commit, second_txs.commit];
        transactions.extend(first_txs.reveals);
        transactions.extend(second_txs.reveals);
        assert!(matches!(
            recover_ee_da_blob([L1DaTransactions::new(42, transactions)], SPEC_VERSION,),
            Err(DaBlobRecoveryError::MultiplePayloads)
        ));
    }

    #[test]
    fn test_non_increasing_block_height_rejected() {
        assert!(matches!(
            recover_ee_da_blob(
                [
                    L1DaTransactions::new(42, Vec::new()),
                    L1DaTransactions::new(42, Vec::new()),
                ],
                SPEC_VERSION
            ),
            Err(DaBlobRecoveryError::NonIncreasingBlockHeight {
                previous: 42,
                current: 42
            })
        ));
    }
}
