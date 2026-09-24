use alpen_ee_da_types::{DaBlob, DA_BLOB_VERSION, EE_DA_MAGIC_BYTES};
use bitcoin::Transaction;
use strata_codec::{Codec, CodecError};
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

/// Errors raised while recovering one EE DA blob from ordered L1 blocks.
#[derive(Debug, thiserror::Error)]
pub enum DaBlobRecoveryError {
    #[error("no complete EE DA payload found")]
    MissingPayload,
    #[error("multiple complete EE DA payloads found")]
    MultiplePayloads,
    #[error("non-increasing L1 block height (previous {previous}, got {current})")]
    NonIncreasingBlockHeight {
        previous: L1Height,
        current: L1Height,
    },
    #[error("invalid EE DA marker tail ({source})")]
    MarkerTail {
        #[source]
        source: CommitRevealParseError,
    },
    #[error("unsupported EE DA version {actual} (expected {expected})")]
    UnsupportedVersion { expected: u32, actual: u32 },
    #[error("EE DA blob decode failed ({source})")]
    Codec {
        #[source]
        source: CodecError,
    },
}

/// Recovers exactly one supported EE DA blob from ordered L1 block transactions.
pub(crate) fn recover_ee_da_blob(
    blocks: impl IntoIterator<Item = L1DaTransactions>,
) -> Result<DaBlob, DaBlobRecoveryError> {
    let config = PayloadParserConfig::chunked_reveals(EE_DA_MAGIC_BYTES.into());
    let mut parser = PayloadParser::new(config);
    let mut recovered_payload = None;
    let mut last_block_height = None;

    for block in blocks {
        let block_height = block.height;
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
            .parse(&block.transactions, block_height)
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
    if version != DA_BLOB_VERSION {
        return Err(DaBlobRecoveryError::UnsupportedVersion {
            expected: DA_BLOB_VERSION,
            actual: version,
        });
    }

    let mut decoder = ChunkIterDecoder::new(payload.chunks().iter().map(Vec::as_slice));
    let blob =
        DaBlob::decode(&mut decoder).map_err(|source| DaBlobRecoveryError::Codec { source })?;
    if !decoder.is_exhausted() {
        return Err(DaBlobRecoveryError::Codec {
            source: CodecError::ExtraInput,
        });
    }

    Ok(blob)
}

#[cfg(test)]
mod tests {
    use alpen_ee_da_types::EvmHeaderSummary;
    use alpen_reth_statediff::BatchStateDiff;
    use strata_codec::encode_to_vec;
    use strata_l1_envelope_fmt::test_utils::build_commit_reveal_set;

    use super::*;

    fn make_blob(update_seq_no: u64) -> DaBlob {
        DaBlob {
            update_seq_no,
            evm_header: EvmHeaderSummary {
                block_num: 10,
                timestamp: 1_700_000_000,
                base_fee: 100,
                gas_used: 21_000,
                gas_limit: 36_000_000,
            },
            state_diff: BatchStateDiff::new(),
        }
    }

    #[test]
    fn test_cross_block_payload_recovers_blob() {
        let expected = make_blob(7);
        let encoded = encode_to_vec(&expected).expect("blob encodes");
        let txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &DA_BLOB_VERSION.to_be_bytes(),
            &[encoded],
            7,
        );
        let blocks = [
            L1DaTransactions::new(42, vec![txs.commit]),
            L1DaTransactions::new(43, txs.reveals),
        ];

        let actual = recover_ee_da_blob(blocks).expect("blob recovers");

        assert_eq!(
            encode_to_vec(&actual).expect("decoded blob encodes"),
            encode_to_vec(&expected).expect("expected blob encodes")
        );
    }

    #[test]
    fn test_incomplete_payload_rejected() {
        let encoded = encode_to_vec(&make_blob(7)).expect("blob encodes");
        let txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &DA_BLOB_VERSION.to_be_bytes(),
            &[encoded],
            7,
        );
        assert!(matches!(
            recover_ee_da_blob([L1DaTransactions::new(42, vec![txs.commit])]),
            Err(DaBlobRecoveryError::MissingPayload)
        ));
    }

    #[test]
    fn test_unsupported_version_rejected() {
        let encoded = encode_to_vec(&make_blob(7)).expect("blob encodes");
        let txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &(DA_BLOB_VERSION + 1).to_be_bytes(),
            &[encoded],
            7,
        );
        let mut transactions = vec![txs.commit];
        transactions.extend(txs.reveals);
        assert!(matches!(
            recover_ee_da_blob([L1DaTransactions::new(42, transactions)]),
            Err(DaBlobRecoveryError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn test_multiple_payloads_rejected() {
        let first = encode_to_vec(&make_blob(7)).expect("first blob encodes");
        let second = encode_to_vec(&make_blob(8)).expect("second blob encodes");
        let first_txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &DA_BLOB_VERSION.to_be_bytes(),
            &[first],
            7,
        );
        let second_txs = build_commit_reveal_set(
            &EE_DA_MAGIC_BYTES.into(),
            &DA_BLOB_VERSION.to_be_bytes(),
            &[&second[..1], &second[1..]],
            8,
        );
        let mut transactions = vec![first_txs.commit, second_txs.commit];
        transactions.extend(first_txs.reveals);
        transactions.extend(second_txs.reveals);
        assert!(matches!(
            recover_ee_da_blob([L1DaTransactions::new(42, transactions)]),
            Err(DaBlobRecoveryError::MultiplePayloads)
        ));
    }

    #[test]
    fn test_non_increasing_block_height_rejected() {
        assert!(matches!(
            recover_ee_da_blob([
                L1DaTransactions::new(42, Vec::new()),
                L1DaTransactions::new(42, Vec::new()),
            ]),
            Err(DaBlobRecoveryError::NonIncreasingBlockHeight {
                previous: 42,
                current: 42
            })
        ));
    }
}
