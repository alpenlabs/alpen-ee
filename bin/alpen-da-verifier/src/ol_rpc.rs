//! Provides access to the OL RPC methods used by the verifier.

use std::ops::Range;

use alpen_acct_state::{EeAccountUpdateManifest, EeAccountUpdateManifestError};
use alpen_acct_types::UpdateExtraData;
use async_trait::async_trait;
use eyre::Context;
use jsonrpsee::{
    core::ClientError as JsonRpcClientError,
    http_client::{HttpClient, HttpClientBuilder},
    types::error::INVALID_PARAMS_CODE,
};
use strata_acct_types::{AccountId, MessageEntry, MsgPayloadError};
use strata_codec::{decode_buf_exact, CodecError};
use strata_ol_rpc_api::OLClientRpcClient;
use strata_ol_rpc_types::{
    OLBlockTag, RpcIndexedEntry, RpcMessageEntry, RpcSnarkAcctUpdateManifest,
};
use strata_snark_acct_types::{Seqno, MAX_PROCESSED_MESSAGES};
use thiserror::Error;
use url::Url;

use crate::account_state::{OLAccountUpdate, OLAccountUpdateError, OLAccountUpdateSource};

const OL_RPC_INBOX_MESSAGE_LIMIT: u64 = 1_000;

#[derive(Debug, Error)]
pub(crate) enum OLAccountUpdateConversionError {
    #[error("OL update sequence number mismatch (expected {expected}, got {actual})")]
    UpdateSeqNoMismatch { expected: u64, actual: u64 },

    #[error(
        "OL update sequence number {update_seq_no} has no inner-state root; the configured OL RPC endpoint may not retain account update metadata"
    )]
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

/// Failure to retrieve or decode an OL account update over RPC.
#[derive(Debug, Error)]
pub(crate) enum RpcOLAccountUpdateError {
    /// Fetching finalized OL account state failed.
    #[error("failed to fetch finalized OL account state: {source}")]
    FetchAccountState {
        #[source]
        source: JsonRpcClientError,
    },

    /// Finalized OL state does not contain the configured EE account.
    #[error("finalized OL state does not contain EE account {account_id}")]
    AccountStateUnavailable { account_id: AccountId },

    /// OL has no manifest for the requested update.
    #[error("OL update manifest {update_seq_no} for account {account_id} is not available")]
    ManifestNotFound {
        account_id: AccountId,
        update_seq_no: u64,
    },

    /// Fetching an update manifest failed.
    #[error("failed to fetch OL update manifest {update_seq_no}: {source}")]
    FetchManifest {
        update_seq_no: u64,
        #[source]
        source: JsonRpcClientError,
    },

    /// Fetching an inbox message range failed.
    #[error("failed to fetch OL inbox range {start}..{end}: {source}")]
    FetchInboxMessages {
        start: u64,
        end: u64,
        #[source]
        source: JsonRpcClientError,
    },

    /// The RPC response does not contain a valid account update.
    #[error(transparent)]
    InvalidAccountUpdate(#[from] OLAccountUpdateConversionError),
}

impl RpcOLAccountUpdateError {
    /// Returns whether retrying may succeed without changing verifier configuration or data.
    pub(crate) fn is_recoverable(&self) -> bool {
        match self {
            Self::FetchAccountState { source }
            | Self::FetchManifest { source, .. }
            | Self::FetchInboxMessages { source, .. } => is_recoverable_json_rpc_error(source),
            Self::AccountStateUnavailable { .. } | Self::InvalidAccountUpdate(_) => false,
            // Requests are bounded by finalized OL state, so a missing historical
            // manifest cannot become available on a later attempt.
            Self::ManifestNotFound { .. } => false,
        }
    }
}

/// Failure to confirm the OL endpoint knows the configured EE account.
#[derive(Debug, Error)]
#[error("OL endpoint does not know EE account {account_id}: {source}")]
pub(crate) struct EeAccountCheckError {
    account_id: AccountId,
    #[source]
    source: JsonRpcClientError,
}

#[derive(Debug)]
pub(crate) struct RpcOLAccountUpdateSource {
    client: HttpClient,
}

impl RpcOLAccountUpdateSource {
    pub(crate) fn try_new(rpc_url: &Url) -> eyre::Result<Self> {
        let client = HttpClientBuilder::default()
            .build(rpc_url.as_str())
            .wrap_err_with(|| format!("failed to create OL RPC client for {rpc_url}"))?;
        Ok(Self { client })
    }

    /// Confirms the OL endpoint knows the configured EE account.
    ///
    /// The account id is a chain fact carried in the params artifact, so it can
    /// disagree with the endpoint. Proving it at launch keeps unknown-account out
    /// of the verification cycle, where it is indistinguishable from an absent
    /// manifest.
    pub(crate) async fn ensure_ee_account(
        &self,
        account_id: AccountId,
    ) -> Result<(), EeAccountCheckError> {
        self.client
            .get_account_genesis_epoch_commitment(account_id)
            .await
            .map_err(|source| EeAccountCheckError { account_id, source })?;
        Ok(())
    }

    async fn fetch_update_manifest(
        &self,
        account_id: AccountId,
        update_seq_no: u64,
    ) -> Result<RpcSnarkAcctUpdateManifest, RpcOLAccountUpdateError> {
        self.client
            .get_snark_acct_update_manifest(account_id, update_seq_no)
            .await
            .map_err(|source| match source {
                // OL answers every not-found condition with INVALID_PARAMS. Launch
                // already proved the account exists on this endpoint, so the only
                // remaining cause for this call is an absent manifest.
                JsonRpcClientError::Call(ref error) if error.code() == INVALID_PARAMS_CODE => {
                    RpcOLAccountUpdateError::ManifestNotFound {
                        account_id,
                        update_seq_no,
                    }
                }
                source => RpcOLAccountUpdateError::FetchManifest {
                    update_seq_no,
                    source,
                },
            })
    }

    async fn fetch_finalized_next_update_seq_no_inner(
        &self,
        account_id: AccountId,
    ) -> Result<Seqno, RpcOLAccountUpdateError> {
        let account_state = self
            .client
            .get_snark_account_state_by_tag(account_id, OLBlockTag::Finalized)
            .await
            .map_err(|source| RpcOLAccountUpdateError::FetchAccountState { source })?
            .ok_or(RpcOLAccountUpdateError::AccountStateUnavailable { account_id })?;
        Ok(Seqno::new(account_state.seq_no()))
    }

    async fn fetch_inbox_message_range(
        &self,
        account_id: AccountId,
        start: u64,
        end: u64,
    ) -> Result<Vec<RpcIndexedEntry<RpcMessageEntry>>, RpcOLAccountUpdateError> {
        self.client
            .get_snark_acct_inbox_msg_range(account_id, start, end)
            .await
            .map_err(|source| RpcOLAccountUpdateError::FetchInboxMessages { start, end, source })
    }

    /// Fetches an account update, keeping the typed RPC error for classification.
    async fn fetch_account_update_inner(
        &self,
        account_id: AccountId,
        update_seq_no: Seqno,
    ) -> Result<OLAccountUpdate, RpcOLAccountUpdateError> {
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

#[async_trait]
impl OLAccountUpdateSource for RpcOLAccountUpdateSource {
    async fn fetch_finalized_next_update_seq_no(
        &self,
        account_id: AccountId,
    ) -> Result<Seqno, OLAccountUpdateError> {
        self.fetch_finalized_next_update_seq_no_inner(account_id)
            .await
            .map_err(|error| {
                let recoverable = error.is_recoverable();
                OLAccountUpdateError::new(error, recoverable)
            })
    }

    async fn fetch_account_update(
        &self,
        account_id: AccountId,
        update_seq_no: Seqno,
    ) -> Result<OLAccountUpdate, OLAccountUpdateError> {
        self.fetch_account_update_inner(account_id, update_seq_no)
            .await
            .map_err(|error| {
                let recoverable = error.is_recoverable();
                OLAccountUpdateError::new(error, recoverable)
            })
    }
}

fn is_recoverable_json_rpc_error(error: &JsonRpcClientError) -> bool {
    matches!(
        error,
        JsonRpcClientError::Transport(_)
            | JsonRpcClientError::RestartNeeded(_)
            | JsonRpcClientError::RequestTimeout
            | JsonRpcClientError::ServiceDisconnect
    )
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
    use jsonrpsee::types::ErrorObject;
    use strata_acct_types::{MessageEntry, MsgPayload};
    use strata_codec::encode_to_vec;

    use super::*;

    /// Arbitrary first inbox index; paging is relative to it.
    const TEST_INBOX_START: u64 = 10;

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

    /// Builds a manifest that is well-formed apart from its absent inner-state root.
    fn build_rpc_manifest_without_inner_state_root(
        sequence: u64,
        prev_next_msg_idx: u64,
        new_next_msg_idx: u64,
    ) -> RpcSnarkAcctUpdateManifest {
        let complete = build_rpc_manifest(sequence, prev_next_msg_idx, new_next_msg_idx);
        RpcSnarkAcctUpdateManifest::new(
            complete.seq_no(),
            None,
            complete.prev_next_msg_idx(),
            complete.new_next_msg_idx(),
            complete.extra_data().cloned(),
        )
    }

    fn build_rpc_message(index: u64) -> RpcIndexedEntry<RpcMessageEntry> {
        let message = MessageEntry::new(AccountId::new([5; 32]), 6, MsgPayload::new_empty());
        RpcIndexedEntry::new(index, message.into())
    }

    #[test]
    fn test_empty_inbox_range_has_no_pages() {
        assert!(inbox_message_ranges(TEST_INBOX_START, TEST_INBOX_START)
            .next()
            .is_none());
    }

    #[test]
    fn test_inbox_range_at_limit_has_one_page() {
        let end = TEST_INBOX_START + OL_RPC_INBOX_MESSAGE_LIMIT;

        assert_eq!(
            inbox_message_ranges(TEST_INBOX_START, end).collect::<Vec<_>>(),
            vec![TEST_INBOX_START..end]
        );
    }

    #[test]
    fn test_large_inbox_range_is_split_into_contiguous_pages() {
        // Two full pages and a short tail.
        let first_page_end = TEST_INBOX_START + OL_RPC_INBOX_MESSAGE_LIMIT;
        let second_page_end = first_page_end + OL_RPC_INBOX_MESSAGE_LIMIT;
        let end = second_page_end + 5;

        assert_eq!(
            inbox_message_ranges(TEST_INBOX_START, end).collect::<Vec<_>>(),
            vec![
                TEST_INBOX_START..first_page_end,
                first_page_end..second_page_end,
                second_page_end..end,
            ]
        );
    }

    #[test]
    fn test_valid_rpc_account_update_is_converted() {
        let rpc_manifest = build_rpc_manifest(7, 10, 12);
        let rpc_messages = [build_rpc_message(10), build_rpc_message(11)];
        let manifest = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect("valid RPC manifest converts");

        let update = convert_account_update(manifest, &rpc_messages)
            .expect("valid RPC account update converts");

        assert_eq!(update.manifest().update_seq_no(), Seqno::new(7));
        assert_eq!(update.manifest().prev_next_msg_idx(), 10);
        assert_eq!(update.manifest().new_next_msg_idx(), 12);
        assert_eq!(
            update.manifest().expected_inner_state_root(),
            [2; 32].into()
        );
        assert_eq!(
            update.manifest().extra_data(),
            &UpdateExtraData::new([3; 32].into(), [4; 32].into(), 0, 0)
        );
        assert_eq!(update.inbox_messages().len(), 2);
    }

    /// A server rejection repeats for the same request, unlike a dropped
    /// connection, so retrying it never clears.
    fn build_server_rejection() -> JsonRpcClientError {
        JsonRpcClientError::Call(ErrorObject::owned(
            INVALID_PARAMS_CODE,
            "invalid params",
            None::<()>,
        ))
    }

    #[test]
    fn test_manifest_missing_below_finalized_frontier_is_fatal() {
        let error = RpcOLAccountUpdateError::ManifestNotFound {
            account_id: AccountId::new([5; 32]),
            update_seq_no: 7,
        };

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_finalized_account_absence_is_fatal() {
        let error = RpcOLAccountUpdateError::AccountStateUnavailable {
            account_id: AccountId::new([5; 32]),
        };

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_invalid_account_update_is_fatal() {
        let error = RpcOLAccountUpdateError::InvalidAccountUpdate(
            OLAccountUpdateConversionError::MissingExtraData { update_seq_no: 7 },
        );

        assert!(!error.is_recoverable());
    }

    /// Both fetch variants defer to the transport classifier rather than
    /// classifying their own sources, so each is checked either way.
    #[test]
    fn test_fetch_failures_follow_transport_classification() {
        let account_state_fetch = |source| RpcOLAccountUpdateError::FetchAccountState { source };
        let manifest_fetch = |source| RpcOLAccountUpdateError::FetchManifest {
            update_seq_no: 7,
            source,
        };
        let inbox_fetch = |source| RpcOLAccountUpdateError::FetchInboxMessages {
            start: 10,
            end: 12,
            source,
        };

        assert!(account_state_fetch(JsonRpcClientError::RequestTimeout).is_recoverable());
        assert!(!account_state_fetch(build_server_rejection()).is_recoverable());
        assert!(manifest_fetch(JsonRpcClientError::RequestTimeout).is_recoverable());
        assert!(!manifest_fetch(build_server_rejection()).is_recoverable());
        assert!(inbox_fetch(JsonRpcClientError::RequestTimeout).is_recoverable());
        assert!(!inbox_fetch(build_server_rejection()).is_recoverable());
    }

    #[test]
    fn test_transport_interruptions_are_recoverable() {
        assert!(is_recoverable_json_rpc_error(
            &JsonRpcClientError::RequestTimeout
        ));
        assert!(is_recoverable_json_rpc_error(
            &JsonRpcClientError::ServiceDisconnect
        ));
    }

    #[test]
    fn test_server_rejection_is_fatal() {
        assert!(!is_recoverable_json_rpc_error(&build_server_rejection()));
    }

    #[test]
    fn test_manifest_not_matching_request_rejected() {
        let rpc_manifest = build_rpc_manifest(8, 10, 10);

        let error = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect_err("update sequence number mismatch must be rejected");

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::UpdateSeqNoMismatch {
                expected: 7,
                actual: 8,
            }
        ));
    }

    #[test]
    fn test_missing_inner_state_root_rejected() {
        let rpc_manifest = build_rpc_manifest_without_inner_state_root(7, 10, 10);

        let error = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect_err("missing inner state root must be rejected");

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::MissingInnerStateRoot { update_seq_no: 7 }
        ));
    }

    #[test]
    fn test_missing_extra_data_rejected() {
        let rpc_manifest = RpcSnarkAcctUpdateManifest::new(7, Some([2; 32].into()), 10, 10, None);

        let error = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect_err("missing extra data must be rejected");

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::MissingExtraData { update_seq_no: 7 }
        ));
    }

    #[test]
    fn test_invalid_extra_data_rejected() {
        let rpc_manifest = RpcSnarkAcctUpdateManifest::new(
            7,
            Some([2; 32].into()),
            10,
            10,
            Some(vec![0xff].into()),
        );

        let error = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect_err("invalid extra data must be rejected");

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::DecodeExtraData {
                update_seq_no: 7,
                ..
            }
        ));
    }

    #[test]
    fn test_inbox_cursor_regression_rejected() {
        let rpc_manifest = build_rpc_manifest(7, 11, 10);

        let error = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect_err("inbox cursor regression must be rejected");

        assert!(matches!(
            error,
            OLAccountUpdateConversionError::InvalidManifest {
                update_seq_no: 7,
                ..
            }
        ));
    }

    #[test]
    fn test_inbox_message_count_above_protocol_limit_rejected() {
        let message_count = MAX_PROCESSED_MESSAGES + 1;
        let rpc_manifest = build_rpc_manifest(7, 10, 10 + message_count);

        let error = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect_err("inbox message count above protocol limit must be rejected");

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
    fn test_missing_inbox_messages_rejected() {
        let rpc_manifest = build_rpc_manifest(7, 10, 12);
        let rpc_messages = [build_rpc_message(10)];
        let manifest = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect("valid RPC manifest converts");

        let error = convert_account_update(manifest, &rpc_messages)
            .expect_err("missing inbox messages must be rejected");

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
    fn test_inbox_index_discontinuity_rejected() {
        let rpc_manifest = build_rpc_manifest(7, 10, 12);
        let rpc_messages = [build_rpc_message(10), build_rpc_message(12)];
        let manifest = convert_update_manifest(Seqno::new(7), &rpc_manifest)
            .expect("valid RPC manifest converts");

        let error = convert_account_update(manifest, &rpc_messages)
            .expect_err("inbox index discontinuity must be rejected");

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
