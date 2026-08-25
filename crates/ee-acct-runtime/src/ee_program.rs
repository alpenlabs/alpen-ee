//! EE-specific snark account program implementation.
//!
//! This module provides the [`EeSnarkAccountProgram`] struct, which implements
//! the [`SnarkAccountProgram`] and [`SnarkAccountProgramVerification`] traits
//! for the EE account type.

use std::marker::PhantomData;

use strata_acct_types::ADMIN_MSG_ACCT_ID;
use strata_ee_acct_types::{
    DecodedEeMessageData, EeAccountState, EnvError, ExecutionEnvironment, PendingInputEntry,
    UpdateExtraData,
};
use strata_ee_chain_types::SubjectDepositData;
use strata_snark_acct_runtime::*;

use crate::verification_state::{EeVerificationInput, EeVerificationState};

/// Snark account program for execution environments.
///
/// The type parameter `E` is the execution environment type used for block
/// execution during verification.
#[derive(Debug)]
pub struct EeSnarkAccountProgram<E: ExecutionEnvironment> {
    _pd: PhantomData<E>,
}

impl<E: ExecutionEnvironment> EeSnarkAccountProgram<E> {
    /// Creates a new instance.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Manual impl for [`Default`] because the derive macro wouldn't work here.
impl<E: ExecutionEnvironment> Default for EeSnarkAccountProgram<E> {
    fn default() -> Self {
        Self {
            _pd: Default::default(),
        }
    }
}

impl<E: ExecutionEnvironment> SnarkAccountProgram for EeSnarkAccountProgram<E> {
    type State = EeAccountState;
    type Msg = DecodedEeMessageData;
    type ExtraData = UpdateExtraData;
    type Error = EnvError;

    fn process_message(
        &self,
        state: &mut Self::State,
        msg: InputMessage<Self::Msg>,
    ) -> ProgramResult<(), Self::Error> {
        process_input_message(state, &msg)
    }

    fn pre_finalize_state(
        &self,
        state: &mut Self::State,
        extra_data: &Self::ExtraData,
    ) -> ProgramResult<(), Self::Error> {
        // Apply the claimed final execution metadata. Verified updates check
        // this against the chunk transition chain in `finalize_verification`.
        state.set_last_exec_blkid(*extra_data.new_tip_blkid());
        state.set_last_exec_state_root(*extra_data.new_tip_state_root());

        Ok(())
    }

    fn finalize_state(
        &self,
        state: &mut Self::State,
        extra_data: Self::ExtraData,
    ) -> ProgramResult<(), Self::Error> {
        // Remove consumed pending inputs and forced inclusions.
        //
        // This runs after `finalize_verification` so that chunk verification
        // can still match deposits against the pending input queue.
        state.remove_pending_inputs(*extra_data.processed_inputs() as usize);
        state.remove_pending_fincls(*extra_data.processed_fincls() as usize);

        Ok(())
    }
}

impl<E: ExecutionEnvironment> SnarkAccountProgramVerification for EeSnarkAccountProgram<E> {
    type VState<'a> = EeVerificationState<'a, E>;
    type VInput<'a> = EeVerificationInput<'a, E>;

    fn start_verification<'i, 'u>(
        &self,
        state: &Self::State,
        vinput: Self::VInput<'i>,
        ulinfo: UpdateLedgerInfo<'u>,
    ) -> ProgramResult<Self::VState<'i>, Self::Error> {
        Ok(EeVerificationState::new_from_state(
            vinput.ee(),
            vinput.chunk_predicate_key(),
            state,
            ulinfo.outputs().clone(), // TODO(STR-3685): ugh, avoid this clone
            vinput.input_chunks(),
            vinput.raw_partial_pre_state(),
        ))
    }

    fn verify_coinput<'a>(
        &self,
        _state: &Self::State,
        _vstate: &mut Self::VState<'a>,
        _msg: &InputMessage<Self::Msg>,
        coinput: &[u8],
    ) -> ProgramResult<(), Self::Error> {
        // For both Valid and Unknown messages, require empty coinput.
        // We don't need any message coinputs for the EE right now.
        if !coinput.is_empty() {
            return Err(ProgramError::MalformedCoinput);
        }

        Ok(())
    }

    fn finalize_verification<'a>(
        &self,
        state: &Self::State,
        mut vstate: Self::VState<'a>,
        extra_data: &Self::ExtraData,
    ) -> ProgramResult<(), Self::Error> {
        // Process and verify all chunks sequentially.
        vstate.process_chunks_on_acct(state, extra_data)?;

        // Make sure the extradata tip blkid matches what we verified.
        if *extra_data.new_tip_blkid() != vstate.cur_verified_exec_blkid() {
            return Err(ProgramError::InvalidExtraData);
        }
        if *extra_data.new_tip_state_root() != vstate.cur_verified_exec_state_root() {
            return Err(ProgramError::InvalidExtraData);
        }

        // Make sure the state matches what we verified.
        //
        // This is sorta redundant since we set it based on the extradata tip in
        // `pre_finalize_state`, but it doesn't hurt to have an extra sanity
        // check.
        if state.last_exec_blkid() != vstate.cur_verified_exec_blkid() {
            return Err(ProgramError::InvalidExtraData);
        }
        if state.last_exec_state_root() != vstate.cur_verified_exec_state_root() {
            return Err(ProgramError::InvalidExtraData);
        }

        // Check the other internal obligations.
        vstate
            .check_obligations()
            .map_err(|_| ProgramError::UnsatisfiedObligations)?;

        Ok(())
    }
}

/// Processes a single input message, applying decoded message effects.
///
/// This is the shared logic used by both the [`SnarkAccountProgram`]
/// implementation and block assembly.
pub(crate) fn process_input_message(
    state: &mut EeAccountState,
    msg: &InputMessage<DecodedEeMessageData>,
) -> ProgramResult<(), EnvError> {
    // If we recognize it, then we have to do something with it.
    if let Some(decoded_msg) = msg.message() {
        apply_decoded_message(state, decoded_msg, msg.meta().value(), msg.meta().source())?;
    }

    Ok(())
}

/// Applies state changes from a decoded EE message.
pub(crate) fn apply_decoded_message(
    state: &mut EeAccountState,
    msg: &DecodedEeMessageData,
    value: strata_acct_types::BitcoinAmount,
    source: strata_acct_types::AccountId,
) -> ProgramResult<(), EnvError> {
    match msg {
        DecodedEeMessageData::Deposit(data) => {
            // Create deposit data with the actual value from the message.
            let deposit_data = SubjectDepositData::new(*data.dest_subject(), value);
            state.add_pending_input(PendingInputEntry::Deposit(deposit_data));
        }

        DecodedEeMessageData::SubjTransfer(_data) => {
            // TODO(STR-3685): handle subject transfers
        }

        DecodedEeMessageData::Commit(_data) => {
            // Just ignore this one for now because we're not handling it.
            // TODO(STR-3685): support this
        }

        DecodedEeMessageData::PredicateUpdate(data) => {
            // A rotation is only authoritative when sourced from the
            // reserved admin account (see `PredicateUpdateMsgData`'s own
            // doc comment) — decoding says nothing about who sent it, so
            // any account could otherwise forge a type-0x20 message and
            // have its own key enqueued as a "rotation". Messages from any
            // other source are silently ignored, same as an unrecognized
            // message type.
            if source == ADMIN_MSG_ACCT_ID {
                // Queue the rotation in true FIFO order relative to deposits.
                // Consuming it (draining it from the front of the queue) is
                // what actually rotates the key and terminates the
                // batch/chunk that consumed it — see `apply_decoded_message`'s
                // callers in block-assembly and
                // `EeVerificationState::merge_new_outputs`.
                state.add_pending_input(PendingInputEntry::PredicateRotation(
                    data.new_key().clone(),
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use strata_acct_types::{AccountId, BitcoinAmount, Hash};
    use strata_ee_acct_types::PredicateUpdateMsgData;
    use strata_predicate::PredicateKey;

    use super::*;

    fn empty_state() -> EeAccountState {
        EeAccountState::new(Hash::zero(), Hash::zero(), Vec::new(), Vec::new())
    }

    #[test]
    fn predicate_update_from_admin_is_queued() {
        let mut state = empty_state();
        let key = PredicateKey::always_accept();
        let msg = DecodedEeMessageData::PredicateUpdate(PredicateUpdateMsgData::new(key.clone()));

        apply_decoded_message(
            &mut state,
            &msg,
            BitcoinAmount::default(),
            ADMIN_MSG_ACCT_ID,
        )
        .expect("apply succeeds");

        assert_eq!(state.pending_inputs().len(), 1);
        assert!(matches!(
            &state.pending_inputs()[0],
            PendingInputEntry::PredicateRotation(k) if k == &key
        ));
    }

    #[test]
    fn predicate_update_from_non_admin_is_ignored() {
        let mut state = empty_state();
        let key = PredicateKey::always_accept();
        let msg = DecodedEeMessageData::PredicateUpdate(PredicateUpdateMsgData::new(key));
        let attacker = AccountId::new([0xAA; 32]);

        apply_decoded_message(&mut state, &msg, BitcoinAmount::default(), attacker)
            .expect("apply succeeds");

        assert!(state.pending_inputs().is_empty());
    }
}
