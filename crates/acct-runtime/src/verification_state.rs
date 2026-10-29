//! Verification state for EE accounts.
//!
//! This module contains the verification state types used during update
//! processing in SNARK proofs.

use alpen_acct_types::{EeAccountState, UpdateExtraData};
use strata_predicate::PredicateKey;
use strata_snark_acct_types::UpdateOutputs;

use crate::{
    chunk_chain::ChunkChain,
    errors::{AcctRuntimeError, AcctRuntimeResult},
    private_input::ArchivedChunkInput,
};

/// Verification input for EE accounts.
///
/// This is passed by value to `start_verification` when using the verification
/// path, so that its contents (the references) can be moved into `VState`.
#[derive(Clone, Debug)]
pub struct EeVerificationInput<'a> {
    /// Predicate used for verifying chunk proofs.
    chunk_predicate_key: &'a PredicateKey,

    /// Chunk transitions and their proofs, in chain order.
    input_chunks: &'a [ArchivedChunkInput],
}

impl<'a> EeVerificationInput<'a> {
    /// Constructs a new instance.
    pub fn new(
        chunk_predicate_key: &'a PredicateKey,
        input_chunks: &'a [ArchivedChunkInput],
    ) -> Self {
        Self {
            chunk_predicate_key,
            input_chunks,
        }
    }

    pub fn chunk_predicate_key(&self) -> &'a PredicateKey {
        self.chunk_predicate_key
    }

    pub fn input_chunks(&self) -> &'a [ArchivedChunkInput] {
        self.input_chunks
    }
}

/// Verification state for EE accounts.
///
/// This type tracks all verification-related state during update processing:
/// the chain of verified chunk transitions, the outputs the update claims, and
/// the chunks still to verify.
#[derive(Clone, Debug)]
pub struct EeVerificationState<'a> {
    /// Predicate used for verifying chunk proofs.
    chunk_predicate_key: &'a PredicateKey,

    /// Outputs the update claims.
    expected_outputs: UpdateOutputs,

    /// Chunk transitions verified so far.
    chain: ChunkChain,

    /// Chunk transitions to verify.
    input_chunks: &'a [ArchivedChunkInput],
}

impl<'a> EeVerificationState<'a> {
    /// Constructs a verification state using the account's initial state as a
    /// reference, along with the verification input data.
    pub fn new_from_state(
        chunk_predicate_key: &'a PredicateKey,
        state: &EeAccountState,
        expected_outputs: UpdateOutputs,
        input_chunks: &'a [ArchivedChunkInput],
    ) -> Self {
        Self {
            chunk_predicate_key,
            expected_outputs,
            chain: ChunkChain::new(state),
            input_chunks,
        }
    }

    /// Returns the predkey used to verify chunk transition proofs.
    pub fn chunk_predicate_key(&self) -> &'a PredicateKey {
        self.chunk_predicate_key
    }

    /// Returns the chunk transitions verified so far.
    pub fn chain(&self) -> &ChunkChain {
        &self.chain
    }

    /// Verifies all chunk transitions against the account's predicate key and
    /// applies them to the chain.
    ///
    /// Returns the extra data the chunks produce, for the caller to compare
    /// whole against the claimed extra data.
    pub(crate) fn process_chunks_on_acct(
        &mut self,
        state: &EeAccountState,
    ) -> AcctRuntimeResult<UpdateExtraData> {
        for (index, chunk) in self.input_chunks.iter().enumerate() {
            let transition = chunk
                .try_decode_chunk_transition()
                .map_err(|source| AcctRuntimeError::DecodeChunkTransition { index, source })?;

            // The chain checks are cheap and the proof check is not, so a chunk
            // that breaks the chain is rejected before its proof is checked.
            // Any error aborts the update, so applying first is safe.
            self.chain.apply(&transition, state.pending_inputs())?;

            self.chunk_predicate_key()
                .verify_claim_witness(chunk.chunk_transition_ssz(), chunk.proof())
                .map_err(|_| AcctRuntimeError::InvalidChunkProof { index })?;
        }

        Ok(self.chain.extra_data())
    }

    /// Final checks to see if there's anything in the verification state that
    /// were supposed to have been dealt with but weren't.
    ///
    /// Predicate rotations: the OL applies whatever predicate an update
    /// declares, without restriction — declaring only the queued key is this
    /// EE's own policy, enforced here since `expected_outputs` (declared)
    /// must equal the chain's outputs (accumulated from the transition that
    /// consumed the admin message's rotation).
    pub(crate) fn check_obligations(&self) -> AcctRuntimeResult<()> {
        if self.expected_outputs != *self.chain.outputs() {
            return Err(AcctRuntimeError::OutputsMismatch {
                claimed: Box::new(self.expected_outputs.clone()),
                computed: Box::new(self.chain.outputs().clone()),
            });
        }

        Ok(())
    }
}
