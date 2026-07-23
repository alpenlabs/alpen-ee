//! Runtime-derived fork activation types.
//!
//! Fork activations are not configured up front: they are derived at the
//! VK-update boundary in the EE inbox ordering (see the Alpen upgrade
//! design) and persisted so a restarted node re-applies them to its live
//! chainspec before executing or building any block at or past the
//! activation.

use borsh::{BorshDeserialize, BorshSerialize};
use ssz::Decode;
use strata_acct_types::{MessageEntry, ADMIN_MSG_ACCT_ID};
use strata_ee_acct_types::DecodedEeMessageData;
use strata_predicate::PredicateKey;

/// Activation coordinate of a derived fork, mirroring the reth
/// `ForkCondition` variants the derivation produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum ForkActivation {
    /// Activates at the given block height (custom Alpen forks).
    Block(u64),
    /// Activates at the given EVM header timestamp (stock post-merge forks).
    Timestamp(u64),
}

/// A persisted fork activation derived from canonical EE history.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ForkActivationRecord {
    /// The fork's name (e.g. `"Osaka"`), matching the hardfork identifier.
    fork: String,
    /// The derived activation coordinate.
    activation: ForkActivation,
    /// The boundary block: the block that consumed the VK-update message.
    /// The fork is active from the next block onward.
    boundary_blocknum: u64,
}

impl ForkActivationRecord {
    /// Creates a new record.
    pub fn new(fork: String, activation: ForkActivation, boundary_blocknum: u64) -> Self {
        Self {
            fork,
            activation,
            boundary_blocknum,
        }
    }

    /// The fork's name.
    pub fn fork(&self) -> &str {
        &self.fork
    }

    /// The derived activation coordinate.
    pub fn activation(&self) -> ForkActivation {
        self.activation
    }

    /// The block that consumed the VK-update message.
    pub fn boundary_blocknum(&self) -> u64 {
        self.boundary_blocknum
    }
}

/// Finds the predicate key rotation a block's consumed messages carry, if any.
///
/// Only admin-sourced messages are honored, mirroring the OL-side activation
/// rule — the source of an inbox message is the authenticated sender account
/// and the admin id is reserved, so ordinary accounts cannot forge a
/// boundary. When a block consumes several rotations the last one wins,
/// matching the order the OL applies them in.
pub fn find_vk_update(messages: &[MessageEntry]) -> Option<PredicateKey> {
    messages
        .iter()
        .filter(|entry| entry.source() == ADMIN_MSG_ACCT_ID)
        .filter_map(
            |entry| match DecodedEeMessageData::decode_raw(entry.payload_buf()) {
                Ok(DecodedEeMessageData::VkUpdate(data)) => Some(data.new_update_vk().clone()),
                _ => None,
            },
        )
        .next_back()
}

/// Decodes a predicate key from its SSZ encoding.
///
/// Convenience for persisted VK bytes; kept beside the message helper so the
/// encoding stays in one place.
pub fn decode_predicate_key(bytes: &[u8]) -> Option<PredicateKey> {
    PredicateKey::from_ssz_bytes(bytes).ok()
}

#[cfg(test)]
mod tests {
    use ssz::Encode;
    use strata_acct_types::{AccountId, MsgPayload};
    use strata_ee_acct_types::VK_UPDATE_MSG_TYPE;
    use strata_msg_fmt::{Msg, OwnedMsg};
    use strata_predicate::{PredicateKey, PredicateTypeId};

    use super::*;

    fn vk_update_entry(source: AccountId, key: &PredicateKey) -> MessageEntry {
        let msg = OwnedMsg::new(VK_UPDATE_MSG_TYPE, key.as_ssz_bytes()).expect("valid type id");
        let payload = MsgPayload::from_bytes_valueless(msg.to_vec()).expect("payload fits");
        MessageEntry::new(source, 0, payload)
    }

    #[test]
    fn finds_admin_sourced_rotation() {
        let key = PredicateKey::new(PredicateTypeId::Bip340Schnorr, vec![7u8; 32]);
        let messages = [vk_update_entry(ADMIN_MSG_ACCT_ID, &key)];

        assert_eq!(find_vk_update(&messages), Some(key));
    }

    #[test]
    fn ignores_non_admin_sources() {
        let key = PredicateKey::new(PredicateTypeId::Bip340Schnorr, vec![7u8; 32]);
        let messages = [vk_update_entry(AccountId::new([9u8; 32]), &key)];

        assert_eq!(find_vk_update(&messages), None);
    }

    #[test]
    fn last_rotation_wins() {
        let first = PredicateKey::new(PredicateTypeId::Bip340Schnorr, vec![1u8; 32]);
        let second = PredicateKey::new(PredicateTypeId::Bip340Schnorr, vec![2u8; 32]);
        let messages = [
            vk_update_entry(ADMIN_MSG_ACCT_ID, &first),
            vk_update_entry(ADMIN_MSG_ACCT_ID, &second),
        ];

        assert_eq!(find_vk_update(&messages), Some(second));
    }
}
