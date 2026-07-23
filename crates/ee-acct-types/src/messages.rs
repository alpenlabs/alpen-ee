//! Definitions for EE message types.

use ssz::Decode;
use strata_acct_types::{MAX_MSG_PAYLOAD_DATA_BYTES, SubjectId};
use strata_codec::{Codec, VarVec, decode_buf_exact, impl_type_flat_struct};
use strata_msg_fmt::{Msg, MsgRef, TypeId};
use strata_predicate::PredicateKey;
use strata_snark_acct_runtime::IAcctMsg;

use crate::{MessageDecodeError, MessageDecodeResult};

/// Maximum byte length for subject transfer data, derived from
/// `MAX_MSG_PAYLOAD_DATA_BYTES` in the acct-types SSZ spec.
const MAX_TRANSFER_DATA_BYTES: u32 = MAX_MSG_PAYLOAD_DATA_BYTES as u32;

/// Message type ID for deposit messages.
pub const DEPOSIT_MSG_TYPE: TypeId = 0x02;

/// Message type ID for subject transfer messages.
pub const SUBJ_TRANSFER_MSG_TYPE: TypeId = 0x01;

/// Message type ID for commit messages.
pub const COMMIT_MSG_TYPE: TypeId = 0x10;

/// Message type ID for admin predicate key (update VK) rotations.
///
/// Emitted by the OL when an admin `EeStfVk` update is applied. The body is
/// the SSZ encoding of the new [`PredicateKey`]. Per the Alpen upgrade
/// design, the batch that consumes this message is the last one proven under
/// the old VK, and the next block is the first one under the new fork rules.
pub const VK_UPDATE_MSG_TYPE: TypeId = 0x20;

/// Decoded possible EE account messages we want to honor.
///
/// This is not intended to capture all possible message types.
// TODO(STR-2172): make zero copy?
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodedEeMessageData {
    /// Deposit from L1 to a subject in the EE.
    Deposit(DepositMsgData),

    /// Transfer from a subject in one EE to a subject in another EE.
    SubjTransfer(SubjTransferMsgData),

    /// Commit an update.
    Commit(CommitMsgData),

    /// Admin rotation of the account's update predicate key (VK).
    VkUpdate(VkUpdateMsgData),
}

impl DecodedEeMessageData {
    /// Decode a raw message buffer, distinguishing its type.
    pub fn decode_raw(buf: &[u8]) -> MessageDecodeResult<DecodedEeMessageData> {
        let msg = MsgRef::try_from(buf).map_err(|_| MessageDecodeError::InvalidFormat)?;
        let body = msg.body();

        match msg.ty() {
            DEPOSIT_MSG_TYPE => {
                let data = decode_codec_msg_body::<DepositMsgData>(body)?;
                Ok(DecodedEeMessageData::Deposit(data))
            }

            SUBJ_TRANSFER_MSG_TYPE => {
                let data = decode_codec_msg_body::<SubjTransferMsgData>(body)?;
                Ok(DecodedEeMessageData::SubjTransfer(data))
            }

            COMMIT_MSG_TYPE => {
                let data = decode_codec_msg_body::<CommitMsgData>(body)?;
                Ok(DecodedEeMessageData::Commit(data))
            }

            VK_UPDATE_MSG_TYPE => {
                // The body is a bare SSZ `PredicateKey`, mirroring how the OL
                // STF encodes the rotation message it appends to the inbox.
                let new_update_vk = PredicateKey::from_ssz_bytes(body)
                    .map_err(|_| MessageDecodeError::InvalidBody)?;
                Ok(DecodedEeMessageData::VkUpdate(VkUpdateMsgData {
                    new_update_vk,
                }))
            }

            ty => Err(MessageDecodeError::UnsupportedType(ty)),
        }
    }
}

impl IAcctMsg for DecodedEeMessageData {
    type ParseError = MessageDecodeError;

    fn try_parse(buf: &[u8]) -> Result<Self, Self::ParseError> {
        Self::decode_raw(buf)
    }
}

/// Decode a message body from a buffer.
fn decode_codec_msg_body<T: Codec>(buf: &[u8]) -> MessageDecodeResult<T> {
    decode_buf_exact(buf).map_err(|_| MessageDecodeError::InvalidBody)
}

impl_type_flat_struct! {
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct DepositMsgData {
        dest_subject: SubjectId,
    }
}

impl_type_flat_struct! {
    /// Describes a transfer between subjects in EEs.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct SubjTransferMsgData {
        source_subject: SubjectId,
        dest_subject: SubjectId,
        transfer_data: VarVec<u8, { MAX_TRANSFER_DATA_BYTES }>,
    }
}

impl SubjTransferMsgData {
    pub fn data_buf(&self) -> &[u8] {
        self.transfer_data().as_slice()
    }
}

impl_type_flat_struct! {
    /// Describes a chunk a sequencer wants to stage.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct CommitMsgData {
        // TODO(STR-3685): rename to new_tip_exec_blkid
        new_tip_exec_blkid: [u8; 32],
    }
}

/// Body of a [`VK_UPDATE_MSG_TYPE`] message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VkUpdateMsgData {
    new_update_vk: PredicateKey,
}

impl VkUpdateMsgData {
    /// Creates new message data.
    pub fn new(new_update_vk: PredicateKey) -> Self {
        Self { new_update_vk }
    }

    /// The predicate key the account rotates to when this message is
    /// consumed.
    pub fn new_update_vk(&self) -> &PredicateKey {
        &self.new_update_vk
    }
}
