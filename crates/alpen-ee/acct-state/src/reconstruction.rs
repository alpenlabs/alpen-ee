use strata_acct_types::{Hash, MessageEntry};
use strata_ee_acct_runtime::apply_input_messages;
use strata_ee_acct_types::EeAccountState;
use strata_snark_acct_runtime::IInnerState;

use crate::{EeAccountReconstructionError, EeAccountUpdateManifest};

/// Applies one accepted update after verifying the reconstructed account root.
///
/// The supplied state is changed only after the complete update succeeds.
/// `evm_state_root` is the locally reconstructed EVM post-state root and is
/// authoritative for reconstruction; the state root in
/// [`strata_ee_acct_types::UpdateExtraData`] remains an independently published value.
///
/// # Preconditions
///
/// - `state` is the reconstructed EE account state immediately before applying the update
///   represented by `manifest`.
/// - `evm_state_root` is the EVM post-state root for the update.
/// - `inbox_messages` are the ordered inbox messages consumed by the update.
///
/// This function verifies the message count and reconstructed inner-state root,
/// but these input types do not encode sequence continuity, replay provenance,
/// or message indices.
///
/// # Errors
///
/// Returns an error when the inbox message count is incorrect, applying a
/// message fails, the update consumes unavailable pending entries, or the
/// reconstructed inner-state root differs from the OL-published root.
pub fn apply_ee_account_update_manifest(
    state: &mut EeAccountState,
    evm_state_root: Hash,
    manifest: &EeAccountUpdateManifest,
    inbox_messages: &[MessageEntry],
) -> Result<(), EeAccountReconstructionError> {
    let expected_message_count = manifest.inbox_message_count();
    let actual_message_count = inbox_messages.len() as u64;
    if expected_message_count != actual_message_count {
        return Err(EeAccountReconstructionError::InboxMessageCountMismatch {
            update_seq_no: manifest.update_seq_no(),
            expected: expected_message_count,
            actual: actual_message_count,
        });
    }

    let mut reconstructed = state.clone();
    apply_input_messages(&mut reconstructed, inbox_messages).map_err(|source| {
        EeAccountReconstructionError::ApplyInboxMessages {
            update_seq_no: manifest.update_seq_no(),
            source,
        }
    })?;

    let requested_inputs = *manifest.extra_data().processed_inputs();
    let available_inputs = reconstructed.pending_inputs().len();
    let processed_inputs = requested_inputs as usize;
    if processed_inputs > available_inputs {
        return Err(EeAccountReconstructionError::PendingInputUnderflow {
            update_seq_no: manifest.update_seq_no(),
            requested: requested_inputs,
            available: available_inputs,
        });
    }

    let requested_fincls = *manifest.extra_data().processed_fincls();
    let available_fincls = reconstructed.pending_fincls().len();
    let processed_fincls = requested_fincls as usize;
    if processed_fincls > available_fincls {
        return Err(EeAccountReconstructionError::PendingFinclUnderflow {
            update_seq_no: manifest.update_seq_no(),
            requested: requested_fincls,
            available: available_fincls,
        });
    }

    reconstructed.set_last_exec_blkid(*manifest.extra_data().new_tip_blkid());
    reconstructed.set_last_exec_state_root(evm_state_root);
    reconstructed.remove_pending_inputs(processed_inputs);
    reconstructed.remove_pending_fincls(processed_fincls);

    let reconstructed_inner_state_root = compute_ee_account_inner_root(&reconstructed);
    let expected_inner_state_root = manifest.expected_inner_state_root();
    if reconstructed_inner_state_root != expected_inner_state_root {
        return Err(EeAccountReconstructionError::InnerStateRootMismatch {
            update_seq_no: manifest.update_seq_no(),
            expected: expected_inner_state_root,
            actual: reconstructed_inner_state_root,
        });
    }
    *state = reconstructed;

    Ok(())
}

/// Computes the SSZ tree root of an EE account state.
pub fn compute_ee_account_inner_root(state: &EeAccountState) -> Hash {
    state.compute_state_root()
}

#[cfg(test)]
mod tests {
    use std::slice::from_ref;

    use strata_acct_types::{AccountId, BitcoinAmount, MsgPayload, SubjectId};
    use strata_codec::encode_to_vec;
    use strata_ee_acct_types::{
        DepositMsgData, PendingFinclEntry, UpdateExtraData, DEPOSIT_MSG_TYPE_ID,
    };
    use strata_msg_fmt::{Msg as _, OwnedMsg};
    use strata_snark_acct_types::Seqno;

    use super::*;

    fn make_hash(seed: u8) -> Hash {
        Hash::new([seed; 32])
    }

    fn make_empty_state() -> EeAccountState {
        EeAccountState::new(make_hash(1), make_hash(2), Vec::new(), Vec::new())
    }

    fn build_manifest(
        expected_inner_state_root: Hash,
        prev_next_msg_idx: u64,
        new_next_msg_idx: u64,
        processed_inputs: u32,
        processed_fincls: u32,
    ) -> EeAccountUpdateManifest {
        EeAccountUpdateManifest::try_new(
            Seqno::new(4),
            expected_inner_state_root,
            prev_next_msg_idx,
            new_next_msg_idx,
            UpdateExtraData::new(
                make_hash(3),
                make_hash(4),
                processed_inputs,
                processed_fincls,
            ),
        )
        .expect("test manifest has valid inbox cursors")
    }

    fn make_deposit_message() -> MessageEntry {
        let deposit = DepositMsgData::new(SubjectId::new([5; 32]));
        let body = encode_to_vec(&deposit).expect("deposit data encodes");
        let message = OwnedMsg::new(DEPOSIT_MSG_TYPE_ID, body).expect("message builds");
        let payload = MsgPayload::from_bytes(
            BitcoinAmount::try_from(100u64).expect("amount is valid"),
            message.to_vec(),
        )
        .expect("message payload fits");
        MessageEntry::new(AccountId::new([6; 32]), 7, payload)
    }

    #[test]
    fn test_inbox_messages_update_pending_queues() {
        let mut state = make_empty_state();
        let evm_state_root = make_hash(7);
        let inbox_message = make_deposit_message();
        let mut expected_state = state.clone();
        apply_input_messages(&mut expected_state, from_ref(&inbox_message))
            .expect("inbox message applies");
        expected_state.set_last_exec_blkid(make_hash(3));
        expected_state.set_last_exec_state_root(evm_state_root);
        let expected_root = compute_ee_account_inner_root(&expected_state);
        let manifest = build_manifest(expected_root, 10, 11, 0, 0);

        apply_ee_account_update_manifest(&mut state, evm_state_root, &manifest, &[inbox_message])
            .expect("account update applies");

        assert_eq!(state.pending_inputs().len(), 1);
        assert_eq!(state.last_exec_blkid(), make_hash(3));
        assert_eq!(state.last_exec_state_root(), evm_state_root);
    }

    #[test]
    fn test_processed_inputs_and_fincls_are_removed() {
        let mut state = EeAccountState::new(
            make_hash(1),
            make_hash(2),
            Vec::new(),
            vec![PendingFinclEntry::new(9, make_hash(9))],
        );
        let expected_state =
            EeAccountState::new(make_hash(3), make_hash(7), Vec::new(), Vec::new());
        let manifest = build_manifest(compute_ee_account_inner_root(&expected_state), 10, 11, 1, 1);

        apply_ee_account_update_manifest(
            &mut state,
            make_hash(7),
            &manifest,
            &[make_deposit_message()],
        )
        .expect("account update applies");

        assert!(state.pending_inputs().is_empty());
        assert!(state.pending_fincls().is_empty());
    }

    #[test]
    fn test_matching_inner_state_root_commits_reconstructed_state() {
        let evm_state_root = make_hash(7);
        let expected_state =
            EeAccountState::new(make_hash(3), evm_state_root, Vec::new(), Vec::new());
        let expected_root = compute_ee_account_inner_root(&expected_state);
        let manifest = build_manifest(expected_root, 10, 10, 0, 0);
        let mut state = make_empty_state();

        apply_ee_account_update_manifest(&mut state, evm_state_root, &manifest, &[])
            .expect("account update applies");

        assert_eq!(state, expected_state);
    }

    #[test]
    fn test_inner_state_root_mismatch_preserves_state() {
        let mut state = make_empty_state();
        let initial_state = state.clone();
        let manifest = build_manifest(make_hash(8), 10, 10, 0, 0);

        let err = apply_ee_account_update_manifest(&mut state, make_hash(7), &manifest, &[])
            .expect_err("inner-state root mismatch must fail");

        assert!(matches!(
            err,
            EeAccountReconstructionError::InnerStateRootMismatch {
                update_seq_no,
                expected,
                ..
            } if update_seq_no == Seqno::new(4) && expected == make_hash(8)
        ));
        assert_eq!(state, initial_state);
    }

    #[test]
    fn test_pending_input_underflow_preserves_state() {
        let mut state = make_empty_state();
        let initial_state = state.clone();
        let manifest = build_manifest(make_hash(8), 10, 10, 1, 0);

        let err = apply_ee_account_update_manifest(&mut state, make_hash(7), &manifest, &[])
            .expect_err("missing pending input must fail");

        assert!(matches!(
            err,
            EeAccountReconstructionError::PendingInputUnderflow {
                update_seq_no,
                requested: 1,
                available: 0,
            } if update_seq_no == Seqno::new(4)
        ));
        assert_eq!(state, initial_state);
    }

    #[test]
    fn test_pending_fincl_underflow_preserves_state() {
        let mut state = make_empty_state();
        let initial_state = state.clone();
        let manifest = build_manifest(make_hash(8), 10, 10, 0, 1);

        let err = apply_ee_account_update_manifest(&mut state, make_hash(7), &manifest, &[])
            .expect_err("missing pending fincl must fail");

        assert!(matches!(
            err,
            EeAccountReconstructionError::PendingFinclUnderflow {
                update_seq_no,
                requested: 1,
                available: 0,
            } if update_seq_no == Seqno::new(4)
        ));
        assert_eq!(state, initial_state);
    }

    #[test]
    fn test_inbox_message_count_mismatch_preserves_state() {
        let mut state = make_empty_state();
        let initial_state = state.clone();
        let manifest = build_manifest(make_hash(8), 10, 12, 0, 0);

        let err = apply_ee_account_update_manifest(
            &mut state,
            make_hash(7),
            &manifest,
            &[make_deposit_message()],
        )
        .expect_err("incomplete inbox range must fail");

        assert!(matches!(
            err,
            EeAccountReconstructionError::InboxMessageCountMismatch {
                update_seq_no,
                expected: 2,
                actual: 1,
            } if update_seq_no == Seqno::new(4)
        ));
        assert_eq!(state, initial_state);
    }
}
