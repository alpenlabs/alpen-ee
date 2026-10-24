//! The chain of chunk transitions an update applies to an account.
//!
//! The verifier and the update builder both run every transition through
//! [`ChunkChain::apply`], so the builder rejects exactly what the verifier
//! would. A copy of these rules in the builder could drift and let it build
//! updates that can never be proven.

use alpen_acct_types::{EeAccountState, PendingInputEntry, UpdateExtraData};
use alpen_chain_types::{ChunkTransition, ExecOutputs};
use strata_acct_types::Hash;
use strata_snark_acct_types::{OutputMessage, OutputTransfer, UpdateOutputs};

use crate::errors::{AcctRuntimeError, AcctRuntimeResult};

/// Chunk transitions applied on top of an account's last execution block.
///
/// Tracks the tip they reach, how many pending inputs they consume and the
/// outputs they produce.
#[derive(Clone, Debug)]
pub struct ChunkChain {
    tip_blkid: Hash,
    tip_state_root: Hash,
    inputs_consumed: usize,
    outputs: UpdateOutputs,
}

impl ChunkChain {
    /// Starts an empty chain at the account's last execution block.
    pub fn new(state: &EeAccountState) -> Self {
        Self {
            tip_blkid: state.last_exec_blkid(),
            tip_state_root: state.last_exec_state_root(),
            inputs_consumed: 0,
            outputs: UpdateOutputs::new_empty(),
        }
    }

    /// Returns the block ID of the chain's tip.
    ///
    /// The next transition has to build on it.
    pub fn tip_blkid(&self) -> Hash {
        self.tip_blkid
    }

    /// Returns the state root of the chain's tip.
    pub fn tip_state_root(&self) -> Hash {
        self.tip_state_root
    }

    /// Returns how many pending inputs the chain consumed.
    pub fn inputs_consumed(&self) -> usize {
        self.inputs_consumed
    }

    /// Returns the outputs the chain produced.
    pub fn outputs(&self) -> &UpdateOutputs {
        &self.outputs
    }

    /// Returns the extra data an update over this chain carries.
    pub fn extra_data(&self) -> UpdateExtraData {
        // TODO(STR-3257): chunks don't process forced inclusions yet, so an update
        // processes none. Count the ones the chunks process once they do.
        let processed_fincls = 0;

        UpdateExtraData::new(
            self.tip_blkid,
            self.tip_state_root,
            self.inputs_consumed as u32,
            processed_fincls,
        )
    }

    /// Appends a transition to the chain.
    ///
    /// `pending_inputs` is the account's whole pending input queue. The
    /// transition's deposits, then its rotation if it has one, must match the
    /// entries after the ones the chain already consumed.
    ///
    /// On error the chain is unchanged, except after
    /// [`AcctRuntimeError::OutputOverflow`], which leaves it dirty.
    pub fn apply(
        &mut self,
        transition: &ChunkTransition,
        pending_inputs: &[PendingInputEntry],
    ) -> AcctRuntimeResult<()> {
        // A consumed rotation ends the update. The sequencer seals the batch
        // right after the rotating block, but that's only host behavior —
        // without this check a proof could chain further transitions onto the
        // rotation, and those blocks would be authorized by the predecessor
        // predicate. `outputs` is the latch: `merge_outputs` sets the key and
        // nothing ever clears it.
        if self.outputs.new_predicate().is_some() {
            return Err(AcctRuntimeError::NonTerminalRotation);
        }

        if transition.parent_exec_blkid() != self.tip_blkid {
            return Err(AcctRuntimeError::ChainLinkage {
                tip: self.tip_blkid,
                parent: transition.parent_exec_blkid(),
            });
        }

        // Each deposit must match the next pending input in order.
        let mut next_input = self.inputs_consumed;
        for deposit in transition.inputs().subject_deposits() {
            match pending_inputs.get(next_input) {
                Some(PendingInputEntry::Deposit(expected)) if expected == deposit => {
                    next_input += 1;
                }
                _ => {
                    return Err(AcctRuntimeError::DepositMismatch {
                        position: next_input,
                    });
                }
            }
        }

        // A declared rotation must be the next queued entry after the
        // deposits.
        if let Some(new_key) = transition.outputs().new_predicate() {
            match pending_inputs.get(next_input) {
                Some(PendingInputEntry::PredicateRotation(queued)) if queued == new_key => {
                    next_input += 1;
                }
                _ => {
                    return Err(AcctRuntimeError::RotationMismatch {
                        position: next_input,
                    });
                }
            }
        }

        self.merge_outputs(transition.outputs())?;
        self.inputs_consumed = next_input;
        self.tip_blkid = transition.tip_exec_blkid();
        self.tip_state_root = transition.tip_state_root();

        Ok(())
    }

    /// Appends a transition's outputs to the chain's outputs.
    fn merge_outputs(&mut self, outputs: &ExecOutputs) -> AcctRuntimeResult<()> {
        // The chunk and update output types are defined separately because
        // they refer to different concepts, so each entry is converted.
        self.outputs
            .try_extend_transfers(
                outputs
                    .output_transfers()
                    .iter()
                    .map(|e| OutputTransfer::new(e.dest(), e.value())),
            )
            .map_err(|_| AcctRuntimeError::OutputOverflow)?;

        self.outputs
            .try_extend_messages(
                outputs
                    .output_messages()
                    .iter()
                    .map(|e| OutputMessage::new(e.dest(), e.payload().clone())),
            )
            .map_err(|_| AcctRuntimeError::OutputOverflow)?;

        // This overwrites rather than accumulates, which is only safe because
        // `apply` rejects any transition that follows a rotation — so an
        // update holds at most one, and there is never a previous key here to
        // clobber.
        if let Some(new_key) = outputs.new_predicate() {
            self.outputs.set_new_predicate(Some(new_key.clone()));
        }

        Ok(())
    }
}
