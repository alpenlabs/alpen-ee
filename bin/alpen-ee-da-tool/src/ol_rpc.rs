//! Provides access to the OL RPC methods used by the tool.

use std::ops::Range;

use alpen_ee_acct_state::{EeAccountUpdateManifest, EeAccountUpdateManifestError};
use async_trait::async_trait;
use eyre::Context;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use strata_acct_types::{AccountId, MessageEntry, MsgPayloadError};
use strata_codec::{decode_buf_exact, CodecError};
use strata_ee_acct_types::UpdateExtraData;
use strata_ol_rpc_api::OLClientRpcClient;
use strata_ol_rpc_types::{RpcIndexedEntry, RpcMessageEntry, RpcSnarkAcctUpdateManifest};
use strata_snark_acct_types::{Seqno, MAX_PROCESSED_MESSAGES};
use thiserror::Error;

use crate::account_state::{OLAccountUpdate, OLAccountUpdateSource};

const OL_RPC_INBOX_MESSAGE_LIMIT: u64 = 1_000;

#[derive(Debug, Error)]
enum OLAccountUpdateConversionError {
    #[error("OL update sequence number mismatch (expected {expected}, got {actual})")]
    UpdateSeqNoMismatch { expected: u64, actual: u64 },

    #[error("OL update sequence number {update_seq_no} has no inner-state root")]
    MissingInnerStateRoot { update_seq_no: u64 },

    #[error("OL update sequence number {update_seq_no} has no EE extra data")]
    MissingExtraData { update_seq_no: u64 },

    #[error("failed to decode EE extra data for update sequence number {update_seq_no}: {source}")]
    DecodeExtraData {
        update_seq_no: u64,
        #[source]
        source: CodecError,
    },

    #[error(
        "invalid EE account update manifest at update sequence number {update_seq_no}: {source}"
    )]
    InvalidManifest {
        update_seq_no: u64,
        #[source]
        source: EeAccountUpdateManifestError,
    },

    #[error(
        "OL inbox message count at update sequence number {update_seq_no} exceeds the protocol limit (count {count}, maximum {maximum})"
    )]
    InboxMessageCountExceedsProtocolLimit {
        update_seq_no: u64,
        count: u64,
        maximum: u64,
    },

    #[error("OL inbox message offset {offset} exceeds u64")]
    InboxMessageOffsetTooLarge { offset: usize },

    #[error("OL inbox message index overflow at update sequence number {update_seq_no}")]
    InboxMessageIndexOverflow { update_seq_no: u64 },

    #[error(
        "OL inbox index mismatch at update sequence number {update_seq_no} (expected {expected}, got {actual})"
    )]
    InboxIndexMismatch {
        update_seq_no: u64,
        expected: u64,
        actual: u64,
    },

    #[error(
        "failed to decode inbox message {message_index} for OL update sequence number {update_seq_no}: {source}"
    )]
    DecodeInboxMessage {
        update_seq_no: u64,
        message_index: u64,
        #[source]
        source: MsgPayloadError,
    },

    #[error("OL inbox message count {count} exceeds u64")]
    InboxMessageCountTooLarge { count: usize },

    #[error(
        "OL inbox message count mismatch at update sequence number {update_seq_no} (expected {expected}, got {actual})"
    )]
    InboxMessageCountMismatch {
        update_seq_no: u64,
        expected: u64,
        actual: u64,
    },
}

#[derive(Debug)]
pub(crate) struct RpcOLAccountUpdateSource {
    client: HttpClient,
}

impl RpcOLAccountUpdateSource {
    pub(crate) fn try_new(rpc_url: &str) -> eyre::Result<Self> {
        let client = HttpClientBuilder::default()
            .build(rpc_url)
            .wrap_err_with(|| format!("failed to create OL RPC client for {rpc_url}"))?;
        Ok(Self { client })
    }

    async fn fetch_update_manifest(
        &self,
        account_id: AccountId,
        update_seq_no: u64,
    ) -> eyre::Result<RpcSnarkAcctUpdateManifest> {
        self.client
            .get_snark_acct_update_manifest(account_id, update_seq_no)
            .await
            .wrap_err_with(|| format!("failed to fetch OL update manifest {update_seq_no}"))
    }

    async fn fetch_inbox_message_range(
        &self,
        account_id: AccountId,
        start: u64,
        end: u64,
    ) -> eyre::Result<Vec<RpcIndexedEntry<RpcMessageEntry>>> {
        self.client
            .get_snark_acct_inbox_msg_range(account_id, start, end)
            .await
            .wrap_err_with(|| format!("failed to fetch OL inbox range {start}..{end}"))
    }
}

#[async_trait]
impl OLAccountUpdateSource for RpcOLAccountUpdateSource {
    async fn fetch_account_update(
        &self,
        account_id: AccountId,
        update_seq_no: Seqno,
    ) -> eyre::Result<OLAccountUpdate> {
        let sequence = *update_seq_no.inner();
        let rpc_manifest = self.fetch_update_manifest(account_id, sequence).await?;
        let manifest = convert_update_manifest(update_seq_no, &rpc_manifest)?;
        let mut rpc_messages = Vec::new();
        for range in inbox_message_ranges(manifest.prev_next_msg_idx(), manifest.new_next_msg_idx())
        {
            rpc_messages.extend(
                self.fetch_inbox_message_range(account_id, range.start, range.end)
                    .await?,
            );
        }

        convert_account_update(manifest, &rpc_messages).map_err(Into::into)
    }
}

fn inbox_message_ranges(start: u64, end: u64) -> impl Iterator<Item = Range<u64>> {
    (start..end)
        .step_by(OL_RPC_INBOX_MESSAGE_LIMIT as usize)
        .map(move |page_start| {
            let page_end = page_start
                .saturating_add(OL_RPC_INBOX_MESSAGE_LIMIT)
                .min(end);
            page_start..page_end
        })
}

fn convert_update_manifest(
    update_seq_no: Seqno,
    rpc_manifest: &RpcSnarkAcctUpdateManifest,
) -> Result<EeAccountUpdateManifest, OLAccountUpdateConversionError> {
    let sequence = *update_seq_no.inner();
    if rpc_manifest.seq_no() != sequence {
        return Err(OLAccountUpdateConversionError::UpdateSeqNoMismatch {
            expected: sequence,
            actual: rpc_manifest.seq_no(),
        });
    }

    let expected_inner_state_root = rpc_manifest
        .new_inner_state_root()
        .ok_or(OLAccountUpdateConversionError::MissingInnerStateRoot {
            update_seq_no: sequence,
        })?
        .0
        .into();
    let encoded_extra_data =
        rpc_manifest
            .extra_data()
            .ok_or(OLAccountUpdateConversionError::MissingExtraData {
                update_seq_no: sequence,
            })?;
    let extra_data: UpdateExtraData =
        decode_buf_exact(&encoded_extra_data.0).map_err(|source| {
            OLAccountUpdateConversionError::DecodeExtraData {
                update_seq_no: sequence,
                source,
            }
        })?;

    let manifest = EeAccountUpdateManifest::try_new(
        update_seq_no,
        expected_inner_state_root,
        rpc_manifest.prev_next_msg_idx(),
        rpc_manifest.new_next_msg_idx(),
        extra_data,
    )
    .map_err(|source| OLAccountUpdateConversionError::InvalidManifest {
        update_seq_no: sequence,
        source,
    })?;

    let message_count = manifest.new_next_msg_idx() - manifest.prev_next_msg_idx();
    if message_count > MAX_PROCESSED_MESSAGES {
        return Err(
            OLAccountUpdateConversionError::InboxMessageCountExceedsProtocolLimit {
                update_seq_no: sequence,
                count: message_count,
                maximum: MAX_PROCESSED_MESSAGES,
            },
        );
    }

    Ok(manifest)
}

fn convert_account_update(
    manifest: EeAccountUpdateManifest,
    rpc_messages: &[RpcIndexedEntry<RpcMessageEntry>],
) -> Result<OLAccountUpdate, OLAccountUpdateConversionError> {
    let sequence = *manifest.update_seq_no().inner();
    let mut inbox_messages = Vec::with_capacity(rpc_messages.len());

    for (offset, rpc_message) in rpc_messages.iter().enumerate() {
        let offset = u64::try_from(offset)
            .map_err(|_| OLAccountUpdateConversionError::InboxMessageOffsetTooLarge { offset })?;
        let expected_index = manifest.prev_next_msg_idx().checked_add(offset).ok_or(
            OLAccountUpdateConversionError::InboxMessageIndexOverflow {
                update_seq_no: sequence,
            },
        )?;
        if rpc_message.index() != expected_index {
            return Err(OLAccountUpdateConversionError::InboxIndexMismatch {
                update_seq_no: sequence,
                expected: expected_index,
                actual: rpc_message.index(),
            });
        }

        let message = MessageEntry::try_from(rpc_message.value().clone()).map_err(|source| {
            OLAccountUpdateConversionError::DecodeInboxMessage {
                update_seq_no: sequence,
                message_index: expected_index,
                source,
            }
        })?;
        inbox_messages.push(message);
    }

    let actual_message_count = u64::try_from(inbox_messages.len()).map_err(|_| {
        OLAccountUpdateConversionError::InboxMessageCountTooLarge {
            count: inbox_messages.len(),
        }
    })?;
    let expected_message_count = manifest.new_next_msg_idx() - manifest.prev_next_msg_idx();
    if actual_message_count != expected_message_count {
        return Err(OLAccountUpdateConversionError::InboxMessageCountMismatch {
            update_seq_no: sequence,
            expected: expected_message_count,
            actual: actual_message_count,
        });
    }

    Ok(OLAccountUpdate::new(manifest, inbox_messages))
}

#[cfg(test)]
mod tests {
    use strata_acct_types::{MessageEntry, MsgPayload};
    use strata_codec::encode_to_vec;

    use super::*;

    fn build_rpc_manifest(
        sequence: u64,
        prev_next_msg_idx: u64,
        new_next_msg_idx: u64,
    ) -> RpcSnarkAcctUpdateManifest {
        let extra_data = UpdateExtraData::new([3; 32].into(), [4; 32].into(), 0, 0);
        let encoded_extra_data = encode_to_vec(&extra_data).expect("extra data encodes");

        RpcSnarkAcctUpdateManifest::new(
            sequence,
            Some([2; 32].into()),
            prev_next_msg_idx,
            new_next_msg_idx,
            Some(encoded_extra_data.into()),
        )
    }

    fn build_rpc_message(index: u64) -> RpcIndexedEntry<RpcMessageEntry> {
        let message = MessageEntry::new(AccountId::new([5; 32]), 6, MsgPayload::new_empty());
        RpcIndexedEntry::new(index, message.into())
    }

    fn expect_conversion_error<T>(
        result: Result<T, OLAccountUpdateConversionError>,
    ) -> OLAccountUpdateConversionError {
        match result {
            Ok(_) => panic!("RPC account update conversion must fail"),
            Err(error) => error,
        }
    }

    #[test]
    fn test_empty_inbox_range_has_no_pages() {
        assert!(inbox_message_ranges(10, 10).next().is_none());
    }

    #[test]
    fn test_inbox_range_at_limit_has_one_page() {
        assert_eq!(
            inbox_message_ranges(10, 1_010).collect::<Vec<_>>(),
            vec![10..1_010]
        );
    }

    #[test]
    fn test_large_inbox_range_is_split_into_contiguous_pages() {
        assert_eq!(
            inbox_message_ranges(10, 2_015).collect::<Vec<_>>(),
            vec![10..1_010, 1_010..2_010, 2_010..2_015]
        );
    }

    #[test]
    fn test_rpc_account_update_is_converted() {
        let rpc_manifest = build_rpc_manifest(7, 10, 12);
        let rpc_messages = [build_rpc_message(10), build_rpc_message(11)];
        let manifest = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect("valid RPC manifest converts");

        let update = convert_account_update(manifest, &rpc_messages)
            .expect("valid RPC account update converts");

        assert_eq!(update.manifest().update_seq_no(), Seqno::new(7));
        assert_eq!(update.manifest().prev_next_msg_idx(), 10);
        assert_eq!(update.manifest().new_next_msg_idx(), 12);
        assert_eq!(update.inbox_messages().len(), 2);
    }

    #[test]
    fn test_update_sequence_number_mismatch_is_rejected() {
        let rpc_manifest = build_rpc_manifest(8, 10, 10);

        let error = expect_conversion_error(convert_update_manifest(Seqno::new(7), &rpc_manifest));

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::UpdateSeqNoMismatch {
                expected: 7,
                actual: 8,
            }
        ));
    }

    #[test]
    fn test_missing_inner_state_root_is_rejected() {
        let mut rpc_manifest = build_rpc_manifest(7, 10, 10);
        rpc_manifest = RpcSnarkAcctUpdateManifest::new(
            rpc_manifest.seq_no(),
            None,
            rpc_manifest.prev_next_msg_idx(),
            rpc_manifest.new_next_msg_idx(),
            rpc_manifest.extra_data().cloned(),
        );

        let error = expect_conversion_error(convert_update_manifest(Seqno::new(7), &rpc_manifest));

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::MissingInnerStateRoot { update_seq_no: 7 }
        ));
    }

    #[test]
    fn test_missing_extra_data_is_rejected() {
        let rpc_manifest = RpcSnarkAcctUpdateManifest::new(7, Some([2; 32].into()), 10, 10, None);

        let error = expect_conversion_error(convert_update_manifest(Seqno::new(7), &rpc_manifest));

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::MissingExtraData { update_seq_no: 7 }
        ));
    }

    #[test]
    fn test_invalid_extra_data_is_rejected() {
        let rpc_manifest = RpcSnarkAcctUpdateManifest::new(
            7,
            Some([2; 32].into()),
            10,
            10,
            Some(vec![0xff].into()),
        );

        let error = expect_conversion_error(convert_update_manifest(Seqno::new(7), &rpc_manifest));

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::DecodeExtraData {
                update_seq_no: 7,
                ..
            }
        ));
    }

    #[test]
    fn test_inbox_cursor_regression_is_rejected() {
        let rpc_manifest = build_rpc_manifest(7, 11, 10);

        let error = expect_conversion_error(convert_update_manifest(Seqno::new(7), &rpc_manifest));

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::InvalidManifest {
                update_seq_no: 7,
                ..
            }
        ));
    }

    #[test]
    fn test_inbox_message_count_above_protocol_limit_is_rejected() {
        let message_count = MAX_PROCESSED_MESSAGES + 1;
        let rpc_manifest = build_rpc_manifest(7, 10, 10 + message_count);

        let error = expect_conversion_error(convert_update_manifest(Seqno::new(7), &rpc_manifest));

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::InboxMessageCountExceedsProtocolLimit {
                update_seq_no: 7,
                count,
                maximum,
            } if count == message_count && maximum == MAX_PROCESSED_MESSAGES
        ));
    }

    #[test]
    fn test_missing_inbox_messages_are_rejected() {
        let rpc_manifest = build_rpc_manifest(7, 10, 12);
        let rpc_messages = [build_rpc_message(10)];
        let manifest = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect("valid RPC manifest converts");

        let error = expect_conversion_error(convert_account_update(manifest, &rpc_messages));

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::InboxMessageCountMismatch {
                update_seq_no: 7,
                expected: 2,
                actual: 1,
            }
        ));
    }

    #[test]
    fn test_inbox_index_discontinuity_is_rejected() {
        let rpc_manifest = build_rpc_manifest(7, 10, 12);
        let rpc_messages = [build_rpc_message(10), build_rpc_message(12)];
        let manifest = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect("valid RPC manifest converts");

        let error = expect_conversion_error(convert_account_update(manifest, &rpc_messages));

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::InboxIndexMismatch {
                update_seq_no: 7,
                expected: 11,
                actual: 12,
            }
        ));
    }
}
