use alpen_ee_common::ExecBlockRecord;
use alpen_ee_params::AlpenSpecId;
use borsh::{io, BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use ssz::{Decode, Encode};
use strata_acct_types::{BitcoinAmount, Hash, MessageEntry, MsgPayload};
use strata_ee_acct_types::EeAccountState;
use strata_ee_chain_types::ExecBlockPackage;
use strata_identifiers::OLBlockCommitment;

use super::account_state::DBEeAccountState;

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
pub(crate) struct DBExecBlockRecord {
    pub(crate) blocknum: u64,
    parent_blockhash: Hash,
    timestamp_ms: u64,
    ol_block: OLBlockCommitment,
    /// ExecBlockPackage serialized using SSZ, then wrapped in a Vec<u8> for Borsh
    #[serde(with = "serde_bytes")]
    package_ssz: Vec<u8>,
    account_state: DBEeAccountState,
    next_inbox_msg_idx: u64,
    next_deposit_idx: u64,
    /// Raw `AlpenSpecId` discriminant; `alpen-ee-params` doesn't derive Borsh, so this is
    /// stored as its primitive form and converted at the `ExecBlockRecord` boundary.
    next_spec_version: u16,
    messages: Vec<DBMessageEntry>,
}

impl From<ExecBlockRecord> for DBExecBlockRecord {
    fn from(value: ExecBlockRecord) -> Self {
        let blocknum = value.blocknum();
        let parent_blockhash = value.parent_blockhash();
        let timestamp_ms = value.timestamp_ms();
        let ol_block = *value.ol_block();
        let next_inbox_msg_idx = value.next_inbox_msg_idx();
        let next_deposit_idx = value.next_deposit_idx();
        let next_spec_version = u16::from(value.next_spec_version());
        let (package, account_state, messages) = value.into_parts();
        let package_ssz = package.as_ssz_bytes();
        let account_state = account_state.into();
        let messages = messages.into_iter().map(Into::into).collect();

        Self {
            blocknum,
            parent_blockhash,
            timestamp_ms,
            ol_block,
            package_ssz,
            account_state,
            next_inbox_msg_idx,
            next_deposit_idx,
            next_spec_version,
            messages,
        }
    }
}

impl TryFrom<DBExecBlockRecord> for ExecBlockRecord {
    type Error = ssz::DecodeError;

    fn try_from(value: DBExecBlockRecord) -> Result<Self, Self::Error> {
        let package = ExecBlockPackage::from_ssz_bytes(&value.package_ssz)?;
        let account_state: EeAccountState = value.account_state.into();
        let next_spec_version = AlpenSpecId::try_from(value.next_spec_version)
            .expect("stored spec version must have a known AlpenSpecId variant");

        Ok(ExecBlockRecord::new(
            package,
            account_state,
            value.blocknum,
            value.ol_block,
            value.timestamp_ms,
            value.parent_blockhash,
            value.next_inbox_msg_idx,
            value.next_deposit_idx,
            next_spec_version,
            value.messages.into_iter().map(Into::into).collect(),
        ))
    }
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Serialize, Deserialize)]
struct DBMessageEntry {
    #[serde(with = "hex::serde")]
    source: [u8; 32],
    incl_epoch: u32,
    payload_value_sats: u64,
    #[serde(with = "serde_bytes")]
    payload_data: Vec<u8>,
}

impl From<MessageEntry> for DBMessageEntry {
    fn from(value: MessageEntry) -> Self {
        DBMessageEntry {
            source: value.source.into_inner(),
            incl_epoch: value.incl_epoch,
            payload_value_sats: value.payload().value().to_sat(),
            payload_data: value.payload().data.to_vec(),
        }
    }
}

impl From<DBMessageEntry> for MessageEntry {
    fn from(value: DBMessageEntry) -> Self {
        MessageEntry::new(
            value.source.into(),
            value.incl_epoch,
            MsgPayload::from_bytes(
                BitcoinAmount::try_from(value.payload_value_sats)
                    .expect("database amount must be within the bitcoin money supply"),
                value.payload_data,
            )
            .expect("database message payload bytes must fit within SSZ max length"),
        )
    }
}

/// The record as the sled binary (alpen 0.3.0) stored it: every field of
/// [`DBExecBlockRecord`] except `next_spec_version`, which did not exist.
///
/// Kept for the offline migration only. A record read in this layout gets
/// the spec version that was in force then, [`AlpenSpecId::V0`].
#[cfg(feature = "console")]
#[derive(BorshSerialize, BorshDeserialize)]
struct SledEraExecBlockRecord {
    blocknum: u64,
    parent_blockhash: Hash,
    timestamp_ms: u64,
    ol_block: OLBlockCommitment,
    package_ssz: Vec<u8>,
    account_state: DBEeAccountState,
    next_inbox_msg_idx: u64,
    next_deposit_idx: u64,
    messages: Vec<DBMessageEntry>,
}

#[cfg(feature = "console")]
impl DBExecBlockRecord {
    /// Decodes the sled binary's layout, giving the record the spec version
    /// in force when it was written.
    pub(crate) fn from_sled_era(bytes: &[u8]) -> io::Result<Self> {
        let old = SledEraExecBlockRecord::try_from_slice(bytes)?;
        Ok(Self {
            blocknum: old.blocknum,
            parent_blockhash: old.parent_blockhash,
            timestamp_ms: old.timestamp_ms,
            ol_block: old.ol_block,
            package_ssz: old.package_ssz,
            account_state: old.account_state,
            next_inbox_msg_idx: old.next_inbox_msg_idx,
            next_deposit_idx: old.next_deposit_idx,
            next_spec_version: u16::from(AlpenSpecId::V0),
            messages: old.messages,
        })
    }

    /// Encodes in the sled binary's layout, for tests that build a sled store
    /// from an MDBX one. `None` when the record carries a spec version that
    /// layout could not express.
    pub(crate) fn to_sled_era(&self) -> Option<Vec<u8>> {
        if self.next_spec_version != u16::from(AlpenSpecId::V0) {
            return None;
        }
        let old = SledEraExecBlockRecord {
            blocknum: self.blocknum,
            parent_blockhash: self.parent_blockhash,
            timestamp_ms: self.timestamp_ms,
            ol_block: self.ol_block,
            package_ssz: self.package_ssz.clone(),
            account_state: self.account_state.clone(),
            next_inbox_msg_idx: self.next_inbox_msg_idx,
            next_deposit_idx: self.next_deposit_idx,
            messages: self.messages.clone(),
        };
        borsh::to_vec(&old).ok()
    }
}
