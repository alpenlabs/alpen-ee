//! EE-specific update builder with chunk-aware interface.
//!
//! Wraps the generic snark account builder and tracks chain tip and pending
//! inputs, allowing the consumer to query available inputs and accept
//! validated [`ChunkTransition`]s.

use alpen_acct_types::{EeAccountState, PendingInputEntry};
use alpen_chain_types::ChunkTransition;
use strata_acct_types::{Hash, MessageEntry};
use strata_snark_acct_runtime::{PrivateInput, UpdateBuilder as GenericUpdateBuilder};
use strata_snark_acct_types::{
    LedgerRefs, Seqno, SnarkAccountState, UpdateOperationData, UpdateOutputs,
};

use crate::{
    builder_errors::BuilderResult, chunk_chain::ChunkChain, ee_program::EeSnarkAccountProgram,
    verification_state::EeVerificationInput,
};

/// EE update builder with chunk-aware interface.
///
/// Tracks the execution chain tip and pending input queue internally,
/// validates [`ChunkTransition`]s against this state, and accumulates
/// outputs. The consumer can query [`Self::remaining_pending_inputs`] and
/// [`Self::cur_tip_blkid`] to construct valid transitions.
#[expect(
    missing_debug_implementations,
    reason = "the generic builder does not implement Debug"
)]
pub struct UpdateBuilder<'i> {
    inner: GenericUpdateBuilder<'i, EeSnarkAccountProgram>,

    /// Chunk transitions accepted so far, checked by the same rules the
    /// verifier runs.
    chain: ChunkChain,

    /// Snapshot of pending inputs after message processing. Not mutated —
    /// used to validate chunks and let the consumer query remaining inputs.
    pending_inputs: Vec<PendingInputEntry>,
}

impl<'i> UpdateBuilder<'i> {
    /// Creates a new EE update builder.
    ///
    /// Initializes with an empty message set. Messages are added incrementally
    /// via [`Self::add_message`] or [`Self::add_messages`].
    pub fn new(
        snark_state: SnarkAccountState,
        initial_state: EeAccountState,
        vinput: EeVerificationInput<'i>,
    ) -> BuilderResult<Self> {
        let pending_inputs = initial_state.pending_inputs().to_vec();
        let chain = ChunkChain::new(&initial_state);

        let program = EeSnarkAccountProgram::new();

        let inner = GenericUpdateBuilder::new(
            program,
            snark_state,
            initial_state,
            vinput,
            LedgerRefs::new_empty(),
            UpdateOutputs::new_empty(),
        )?;

        Ok(Self {
            inner,
            chain,
            pending_inputs,
        })
    }

    /// Overrides the seqno that the finalized update will be tagged with.
    ///
    /// By default the inner builder uses the snark account state's expected
    /// next seqno; set this to build an update bound to a different seqno
    /// (e.g. when constructing a sequence of updates ahead of submission).
    pub fn set_override_seq_no(&mut self, seq_no: Option<Seqno>) {
        self.inner.set_override_seq_no(seq_no);
    }

    /// Returns the current account state.
    pub fn cur_acct_state(&self) -> &EeAccountState {
        self.inner.cur_state()
    }

    /// Returns the accumulated outputs so far.
    pub fn outputs(&self) -> &UpdateOutputs {
        self.chain.outputs()
    }

    /// Returns the current chain tip block ID.
    ///
    /// A chunk's `parent_exec_blkid` must equal this value.
    pub fn cur_tip_blkid(&self) -> Hash {
        self.chain.tip_blkid()
    }

    /// Adds a message and immediately processes it with an empty coinput.
    ///
    /// Any new pending inputs created by the message (e.g. deposits) are
    /// appended to the internal tracking list.
    pub fn add_message(&mut self, msg: MessageEntry) -> BuilderResult<()> {
        self.inner.add_message_with_coinput(msg, Vec::new())?;

        // Extend pending inputs with any new entries from this message.
        let state_pending = self.inner.cur_state().pending_inputs();
        if state_pending.len() > self.pending_inputs.len() {
            self.pending_inputs
                .extend_from_slice(&state_pending[self.pending_inputs.len()..]);
        }

        Ok(())
    }

    /// Adds multiple messages, processing each with an empty coinput.
    ///
    /// Convenience batch method equivalent to calling [`Self::add_message`] for each.
    pub fn add_messages(
        &mut self,
        msgs: impl IntoIterator<Item = MessageEntry>,
    ) -> BuilderResult<()> {
        for msg in msgs {
            self.add_message(msg)?;
        }
        Ok(())
    }

    /// Returns the remaining pending inputs that haven't been consumed by
    /// chunks yet.
    ///
    /// A chunk's deposits must match these in order.
    pub fn remaining_pending_inputs(&self) -> &[PendingInputEntry] {
        &self.pending_inputs[self.chain.inputs_consumed()..]
    }

    /// Returns the next `count` pending inputs available for consumption.
    ///
    /// Returns fewer than `count` if not enough remain.
    pub fn next_pending_inputs(&self, count: usize) -> &[PendingInputEntry] {
        let remaining = self.remaining_pending_inputs();
        let n = count.min(remaining.len());
        &remaining[..n]
    }

    /// Returns the total number of pending inputs remaining.
    pub fn remaining_input_count(&self) -> usize {
        self.remaining_pending_inputs().len()
    }

    /// Accepts a chunk transition, validating chain linkage and input matching.
    ///
    /// Runs the same checks as the verifier (see [`ChunkChain::apply`]), so a
    /// transition this accepts is one the verifier accepts too.
    pub fn accept_chunk_transition(&mut self, transition: &ChunkTransition) -> BuilderResult<()> {
        self.chain.apply(transition, &self.pending_inputs)?;
        Ok(())
    }

    /// Builds the update, returning operation data and raw coinputs.
    ///
    /// Sets the update's outputs and extra data from the accepted chunks, then
    /// delegates to the generic builder.
    ///
    /// Uses the unverified finalization path since the builder has already
    /// validated chunk transitions locally via [`Self::accept_chunk_transition`].
    /// Clones internal state so the builder can be reused.
    pub fn build(&mut self) -> BuilderResult<(UpdateOperationData, Vec<Vec<u8>>)> {
        *self.inner.outputs_mut() = self.chain.outputs().clone();
        let (op, coinputs) = self
            .inner
            .build_operation_data_unverified(self.chain.extra_data())?;
        Ok((op, coinputs))
    }

    /// Builds a [`PrivateInput`] for proof generation.
    ///
    /// Sets the update's outputs and extra data from the accepted chunks, then
    /// delegates to the inner builder's verified finalization path. Clones
    /// internal state so the builder can be reused.
    ///
    /// FIXME(STR-3685): the verified path only sees the chunks passed in the `vinput`
    /// given to [`Self::new`], and its expected outputs were fixed then too.
    /// After [`Self::accept_chunk_transition`] it fails, so this only works
    /// for updates with no chunks. Fixing it needs the generic builder to take
    /// the chunks at finalization.
    pub fn build_private_input(&mut self) -> BuilderResult<PrivateInput> {
        *self.inner.outputs_mut() = self.chain.outputs().clone();
        Ok(self.inner.build_private_input(self.chain.extra_data())?)
    }
}
