//! Runtime discovery of Alpen spec activations from the account inbox.

use alpen_ee_params::{AlpenSpecId, AlpenSpecSchedule, AlpenSpecScheduleError};
use strata_acct_types::{MessageEntry, ADMIN_MSG_ACCT_ID};
use strata_ee_acct_types::DecodedEeMessageData;
use tracing::info;

/// Live view of the spec activation schedule, advanced by predicate
/// rotations observed in the account inbox.
///
/// The params artifact only carries the base schedule; an upgrade's real
/// activation coordinate is defined by where the admin's predicate-update
/// message lands in the inbox ordering. The block builder feeds every fetched
/// inbox window through [`SpecTracker::observe_messages`] before assembly, so
/// the schedule is current when the block's governing version is resolved.
///
/// A rotation at inbox index `i` activates the successor version from
/// coordinate `i + 1`: the block consuming the rotation is the last one
/// governed by the predecessor, mirroring the OL rule that the update
/// consuming the message is the last one verified under the old key.
// TODO(STR-3998): activations discovered at runtime are lost on restart; a
// restarted node only knows the artifact's base schedule until the operator
// pins the discovered coordinate there. Re-derive them at boot by rescanning
// the stored exec block records' messages.
#[derive(Debug)]
pub(crate) struct SpecTracker {
    schedule: AlpenSpecSchedule,
    /// Inbox coordinate up to which messages have been scanned. Guards
    /// re-observation: a failed block build is retried with the same fetched
    /// window, and re-scheduling the same rotation would erroneously activate
    /// yet another successor.
    next_msg_idx: u64,
}

impl SpecTracker {
    /// Creates a tracker over the artifact's base `schedule`, scanning from
    /// inbox coordinate `next_msg_idx` onward.
    pub(crate) fn new(schedule: AlpenSpecSchedule, next_msg_idx: u64) -> Self {
        Self {
            schedule,
            next_msg_idx,
        }
    }

    /// Scans an inbox window for admin predicate rotations and schedules the
    /// successor version for each, where `first_msg_idx` is the global inbox
    /// index of `messages[0]`. Already-scanned indices are skipped.
    ///
    /// Errs when a rotation activates a version this binary has no
    /// [`AlpenSpecId`] variant for — the node cannot execute the upgrade, so
    /// block building must stop rather than continue under stale rules.
    pub(crate) fn observe_messages(
        &mut self,
        first_msg_idx: u64,
        messages: &[MessageEntry],
    ) -> Result<(), AlpenSpecScheduleError> {
        for (offset, entry) in messages.iter().enumerate() {
            let msg_idx = first_msg_idx + offset as u64;
            if msg_idx < self.next_msg_idx {
                continue;
            }
            if is_predicate_rotation(entry) {
                let coord = msg_idx + 1;
                let activated = self.schedule.schedule_successor(coord)?;
                info!(
                    ?activated,
                    coord, msg_idx, "discovered spec activation from predicate rotation"
                );
            }
            self.next_msg_idx = msg_idx + 1;
        }
        Ok(())
    }

    /// Returns the version governing `coord` under the current schedule.
    pub(crate) fn active_spec_at(&self, coord: u64) -> AlpenSpecId {
        self.schedule.active_spec_at(coord)
    }
}

/// Returns whether `entry` is a predicate rotation staged by the OL: sourced
/// from the reserved admin account and decoding as a predicate update.
fn is_predicate_rotation(entry: &MessageEntry) -> bool {
    entry.source() == ADMIN_MSG_ACCT_ID
        && matches!(
            DecodedEeMessageData::decode_raw(entry.payload_buf()),
            Ok(DecodedEeMessageData::PredicateUpdate(_))
        )
}

#[cfg(test)]
mod tests {
    use ssz::Encode;
    use strata_acct_types::{AccountId, BitcoinAmount, MsgPayload};
    use strata_ee_acct_types::PREDICATE_UPDATE_MSG_TYPE;
    use strata_msg_fmt::{Msg, OwnedMsg};
    use strata_predicate::PredicateKey;

    use super::*;

    /// A rotation entry as the OL STF stages it: admin-sourced, valueless,
    /// SPS-52 type `0x20` with the SSZ-encoded key as body.
    fn rotation_entry(source: AccountId) -> MessageEntry {
        let body = PredicateKey::always_accept().as_ssz_bytes();
        let msg = OwnedMsg::new(PREDICATE_UPDATE_MSG_TYPE, body).expect("valid message");
        let payload =
            MsgPayload::from_bytes(BitcoinAmount::from_sat(0), msg.to_vec()).expect("fits payload");
        MessageEntry::new(source, 0, payload)
    }

    fn plain_entry() -> MessageEntry {
        let payload = MsgPayload::from_bytes(BitcoinAmount::from_sat(100), vec![]).expect("empty");
        MessageEntry::new(AccountId::new([2u8; 32]), 0, payload)
    }

    #[test]
    fn admin_rotation_activates_the_successor_past_its_index() {
        let mut tracker = SpecTracker::new(AlpenSpecSchedule::genesis(), 5);
        let messages = vec![plain_entry(), rotation_entry(ADMIN_MSG_ACCT_ID)];

        tracker
            .observe_messages(5, &messages)
            .expect("v1 is known to this binary");

        // Rotation at index 6 -> v1 governs from coordinate 7: the consuming
        // block (starting at 5 or 6) stays on v0.
        assert_eq!(tracker.active_spec_at(6), AlpenSpecId::V0);
        assert_eq!(tracker.active_spec_at(7), AlpenSpecId::V1);
    }

    #[test]
    fn non_admin_rotation_is_ignored() {
        let mut tracker = SpecTracker::new(AlpenSpecSchedule::genesis(), 0);
        let messages = vec![rotation_entry(AccountId::new([3u8; 32]))];

        tracker.observe_messages(0, &messages).expect("no-op");

        assert_eq!(tracker.active_spec_at(u64::MAX), AlpenSpecId::V0);
    }

    #[test]
    fn reobserving_the_same_window_is_idempotent() {
        // A failed block build retries with the same fetched window; without
        // the watermark the same rotation would schedule v2.
        let mut tracker = SpecTracker::new(AlpenSpecSchedule::genesis(), 0);
        let messages = vec![rotation_entry(ADMIN_MSG_ACCT_ID)];

        tracker.observe_messages(0, &messages).expect("first scan");
        tracker
            .observe_messages(0, &messages)
            .expect("retried scan");

        assert_eq!(tracker.active_spec_at(u64::MAX), AlpenSpecId::V1);
    }

    #[test]
    fn rotation_past_known_versions_errs() {
        // Two rotations, but this binary only knows v1: the second one is an
        // upgrade it cannot execute.
        let mut tracker = SpecTracker::new(AlpenSpecSchedule::genesis(), 0);
        let messages = vec![
            rotation_entry(ADMIN_MSG_ACCT_ID),
            rotation_entry(ADMIN_MSG_ACCT_ID),
        ];

        assert_eq!(
            tracker.observe_messages(0, &messages),
            Err(AlpenSpecScheduleError::UnknownSuccessor(2))
        );
    }
}
