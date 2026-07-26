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

/// Message type ID for predicate key (update VK) rotations.
///
/// Mirrors `PREDICATE_UPDATE_MSG_TYPE_ID` in `strata-ol-msg-types`: the OL
/// STF stages a rotation enacted by the admin subprotocol as an inbox message
/// of this type, sourced from the reserved admin account.
pub const PREDICATE_UPDATE_MSG_TYPE: TypeId = 0x20;

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

    /// Rotate the account's update predicate key (admin-enacted).
    PredicateUpdate(PredicateUpdateMsgData),
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

            PREDICATE_UPDATE_MSG_TYPE => {
                // The body is the raw SSZ encoding of the new key, not a
                // codec struct; this matches how the OL STF builds the
                // message.
                let new_key = PredicateKey::from_ssz_bytes(body)
                    .map_err(|_| MessageDecodeError::InvalidBody)?;
                Ok(DecodedEeMessageData::PredicateUpdate(
                    PredicateUpdateMsgData::new(new_key),
                ))
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

/// Describes a rotation of the account's update predicate key.
///
/// Decoding says nothing about who sent it: a rotation is only meaningful
/// when the entry's source is the reserved admin account, which consumers
/// must check on the [`MessageEntry`](strata_acct_types::MessageEntry)
/// itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PredicateUpdateMsgData {
    new_key: PredicateKey,
}

impl PredicateUpdateMsgData {
    /// Creates data rotating to `new_key`.
    pub fn new(new_key: PredicateKey) -> Self {
        Self { new_key }
    }

    /// Returns the predicate key the rotation activates.
    pub fn new_key(&self) -> &PredicateKey {
        &self.new_key
    }
}

#[cfg(test)]
mod tests {
    use ssz::Encode;
    use strata_msg_fmt::OwnedMsg;

    use super::*;

    /// Decodes the message shape the OL STF stages for a rotation: SPS-52
    /// type `0x20` with the raw SSZ encoding of the new key as body.
    #[test]
    fn decode_predicate_update_message() {
        let new_key = PredicateKey::always_accept();
        let msg = OwnedMsg::new(PREDICATE_UPDATE_MSG_TYPE, new_key.as_ssz_bytes())
            .expect("valid message");

        let decoded = DecodedEeMessageData::decode_raw(&msg.to_vec()).expect("decodes");

        assert_eq!(
            decoded,
            DecodedEeMessageData::PredicateUpdate(PredicateUpdateMsgData::new(new_key))
        );
    }

    #[test]
    fn predicate_update_with_garbage_body_is_invalid() {
        let msg = OwnedMsg::new(PREDICATE_UPDATE_MSG_TYPE, vec![0xff]).expect("valid message");

        assert!(matches!(
            DecodedEeMessageData::decode_raw(&msg.to_vec()),
            Err(MessageDecodeError::InvalidBody)
        ));
    }
}
