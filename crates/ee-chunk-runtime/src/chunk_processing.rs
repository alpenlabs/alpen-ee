use strata_ee_acct_types::{
    EnvError, EnvResult, ExecBlock, ExecHeader, ExecPartialState, ExecPayload,
    ExecutionEnvironment, Hash,
};
use strata_ee_chain_types::{
    ChunkTransition, ExecInputs, ExecOutputs, OutputMessage, OutputTransfer, SequenceTracker,
    SubjectDepositData,
};
use strata_predicate::PredicateKey;

use crate::chunk::{Chunk, ChunkBlock};

/// Processes a block from a chunk with associated inputs, merging results into
/// the passed state.
pub fn process_block<E: ExecutionEnvironment>(
    ee: &E,
    state: &mut E::PartialState,
    block: &ChunkBlock<'_, E>,
) -> EnvResult<()> {
    // Repackage the block into the payload we can execute.
    let eb = block.exec_block();
    if !E::Block::check_header_matches_body(eb.get_header(), eb.get_body()) {
        return Err(EnvError::InvalidBlock);
    }
    let header_intrinsics = eb.get_header().get_intrinsics();
    let epl = ExecPayload::new(&header_intrinsics, eb.get_body());

    // Execute the block, verify consistency.
    let exec_outp = ee.execute_block_body(state, &epl, block.inputs())?;
    ee.verify_outputs_against_header(eb.get_header(), &exec_outp)?;

    // Check that the EVM-derivable outputs match the chunk block.
    //
    // `new_predicate` is deliberately left out of this comparison: it's an
    // account-level fact drawn from the pending-input queue, not something
    // `execute_block_body` can derive from EVM execution alone (see `evm-ee`'s
    // `execute_block_body`, which only ever sets output messages/transfers and
    // always leaves `new_predicate` empty). Chunk-level verification of a
    // declared rotation against real block execution belongs to a
    // cross-check over the chunk's blocks, not this per-block comparison.
    if exec_outp.outputs().output_transfers() != block.outputs().output_transfers()
        || exec_outp.outputs().output_messages() != block.outputs().output_messages()
    {
        return Err(EnvError::InvalidBlock);
    }

    // Merge the changes and return the outputs.
    ee.merge_write_into_state(state, exec_outp.write_batch())?;
    let computed_state_root = state.compute_state_root()?;
    if computed_state_root != eb.get_header().get_state_root() {
        return Err(EnvError::InvalidBlock);
    }
    ee.update_partial_state_after_block(state, eb.get_header())?;

    Ok(())
}

struct IoTracker<'c> {
    deposits_tracker: SequenceTracker<'c, SubjectDepositData>,
    out_msg_tracker: SequenceTracker<'c, OutputMessage>,
    out_xfr_tracker: SequenceTracker<'c, OutputTransfer>,
    /// Predicate rotation the chunk-level outputs claim, if any.
    expected_new_predicate: Option<&'c PredicateKey>,
    /// Predicate rotation actually observed among the chunk's per-block
    /// outputs, if any. Once set, `check_update` refuses to accept another
    /// block, so the rotating block is always the chunk's last one.
    observed_new_predicate: Option<PredicateKey>,
}

impl<'c> IoTracker<'c> {
    fn from_io(expected_inputs: &'c ExecInputs, expected_outputs: &'c ExecOutputs) -> Self {
        Self {
            deposits_tracker: SequenceTracker::new(expected_inputs.subject_deposits()),
            out_msg_tracker: SequenceTracker::new(expected_outputs.output_messages()),
            out_xfr_tracker: SequenceTracker::new(expected_outputs.output_transfers()),
            expected_new_predicate: expected_outputs.new_predicate(),
            observed_new_predicate: None,
        }
    }

    /// Processes a pair of inputs and outputs, verifying they're all correct.
    fn check_update(&mut self, inps: &ExecInputs, outps: &ExecOutputs) -> EnvResult<()> {
        // A rotation ends the chunk. The sequencer stops assembling at one and
        // seals right after it, but that's only host behavior — without this
        // check a proof could carry blocks that ran after the rotation, and
        // they'd be authorized by the predecessor predicate.
        if self.observed_new_predicate.is_some() {
            return Err(EnvError::NonTerminalRotation);
        }

        // Check them first.
        self.deposits_tracker
            .check_inputs(inps.subject_deposits())
            .map_err(|_| EnvError::InconsistentChunkIo)?;
        self.out_msg_tracker
            .check_inputs(outps.output_messages())
            .map_err(|_| EnvError::InconsistentChunkIo)?;
        self.out_xfr_tracker
            .check_inputs(outps.output_transfers())
            .map_err(|_| EnvError::InconsistentChunkIo)?;

        // And then advance them after they've all been checked.
        self.deposits_tracker
            .advance_unchecked(inps.subject_deposits().len());
        self.out_msg_tracker
            .advance_unchecked(outps.output_messages().len());
        self.out_xfr_tracker
            .advance_unchecked(outps.output_transfers().len());

        // Track any predicate rotation this block declares. The guard above
        // makes this block the last one we accept for the chunk.
        if let Some(key) = outps.new_predicate() {
            self.observed_new_predicate = Some(key.clone());
        }

        Ok(())
    }

    fn is_all_consumed(&self) -> bool {
        self.deposits_tracker.is_fully_consumed()
            && self.out_msg_tracker.is_fully_consumed()
            && self.out_xfr_tracker.is_fully_consumed()
    }

    fn verify_all_consumed(&self) -> EnvResult<()> {
        if !self.is_all_consumed() {
            return Err(EnvError::InconsistentChunkIo);
        }
        // The chunk-level claim must equal exactly what its blocks produced.
        if self.expected_new_predicate != self.observed_new_predicate.as_ref() {
            return Err(EnvError::InconsistentChunkIo);
        }
        Ok(())
    }
}

/// Processes a chunk's blocks and updates the state, checking the IO against an
/// expected IO trace.
fn process_chunk_blocks<E: ExecutionEnvironment>(
    ee: &E,
    state: &mut E::PartialState,
    chunk: &Chunk<'_, E>,
    verified_tip: Hash,
    expected_inputs: &ExecInputs,
    expected_outputs: &ExecOutputs,
) -> EnvResult<()> {
    // 1. Check that the chunk is nonempty.
    if chunk.blocks().is_empty() {
        return Err(EnvError::MalformedChainSegment);
    }

    // 2. Process each block, tracking the IO traces and chain continuity.
    let mut io_tracker = IoTracker::from_io(expected_inputs, expected_outputs);
    let mut cur_verified_tip_blkid = verified_tip;
    for cb in chunk.blocks() {
        let header = cb.exec_block().get_header();

        // Verify it builds on the previous block.
        if header.get_parent_id() != cur_verified_tip_blkid {
            return Err(EnvError::MismatchedChainSegment);
        }

        // Verify the block itself.
        process_block(ee, state, cb)?;

        // Check the block's IO.
        io_tracker.check_update(cb.inputs(), cb.outputs())?;

        cur_verified_tip_blkid = header.compute_block_id();
    }

    // 3. Make sure all the trackers are consumed.
    io_tracker.verify_all_consumed()?;

    Ok(())
}

/// Verifies a chunk transition using the pre state, parent header, etc.
pub fn verify_chunk_transition<E: ExecutionEnvironment>(
    tsn: &ChunkTransition,
    ee: &E,
    prev_header: &<E::Block as ExecBlock>::Header,
    state: &mut E::PartialState,
    chunk: &Chunk<'_, E>,
) -> EnvResult<()> {
    // 1. Make sure the parent block ID we have that we're extending from
    // matches the chunk transition.
    let computed_prev_blkid = prev_header.compute_block_id();
    if computed_prev_blkid != tsn.parent_exec_blkid() {
        // TODO(STR-3685): better error type?
        return Err(EnvError::MismatchedChainSegment);
    }

    // 2. Make sure the chunk is nonempty and check that the last block matches
    // the chunk transition.
    let Some(new_tip_header) = chunk.blocks().last().map(|b| b.exec_block().get_header()) else {
        return Err(EnvError::MalformedChainSegment);
    };

    let computed_new_tip_blkid = new_tip_header.compute_block_id();
    if computed_new_tip_blkid != tsn.tip_exec_blkid() {
        return Err(EnvError::MismatchedChainSegment);
    }
    if &new_tip_header.get_exec_header_summary() != tsn.tip_exec_header_summary() {
        return Err(EnvError::MismatchedChainSegment);
    }

    // 2. Make sure the state matches the parent block's state root.
    let computed_pre_sr = state.compute_state_root()?;
    if computed_pre_sr != prev_header.get_state_root() {
        return Err(EnvError::MismatchedCurStateData);
    }

    // 3. Execute the blocks in the chunk. Each block's post-state root is
    // verified before its header can be used as the parent of a later block.
    process_chunk_blocks(
        ee,
        state,
        chunk,
        tsn.parent_exec_blkid(),
        tsn.inputs(),
        tsn.outputs(),
    )?;

    // 4. Compute the final state root and make sure it matches.
    let computed_post_sr = state.compute_state_root()?;
    if computed_post_sr != new_tip_header.get_state_root() {
        return Err(EnvError::MismatchedChainSegment);
    }
    if computed_post_sr != tsn.tip_state_root() {
        return Err(EnvError::MismatchedChainSegment);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use strata_ee_acct_types::{
        BlockAssembler, ExecBlock, ExecBlockOutput, ExecHeader, ExecPayload,
    };
    use strata_ee_chain_types::{ExecHeaderSummary, ExecInputs, ExecOutputs};
    use strata_simple_ee::{
        SimpleBlock, SimpleBlockBody, SimpleExecutionEnvironment, SimpleHeader,
        SimpleHeaderIntrinsics, SimplePartialState, SimpleTransaction,
    };

    use super::*;
    use crate::chunk::{Chunk, ChunkBlock};

    fn alice() -> strata_acct_types::SubjectId {
        strata_acct_types::SubjectId::from([1u8; 32])
    }

    fn bob() -> strata_acct_types::SubjectId {
        strata_acct_types::SubjectId::from([2u8; 32])
    }

    /// Builds a valid SimpleBlock by executing the body against the given state,
    /// returning the block, its inputs, outputs, and the post-state.
    fn build_block(
        ee: &SimpleExecutionEnvironment,
        state: &SimplePartialState,
        parent_blkid: Hash,
        index: u64,
        body: SimpleBlockBody,
        inputs: ExecInputs,
    ) -> (SimpleBlock, ExecInputs, ExecOutputs, SimplePartialState) {
        let intrinsics = SimpleHeaderIntrinsics {
            parent_blkid,
            index,
        };
        let payload = ExecPayload::new(&intrinsics, &body);
        let output: ExecBlockOutput<SimpleExecutionEnvironment> =
            ee.execute_block_body(state, &payload, &inputs).unwrap();

        let header = ee.complete_header(&payload, &output).unwrap();
        let block = SimpleBlock::new(header, body);

        let mut post_state = state.clone();
        ee.merge_write_into_state(&mut post_state, output.write_batch())
            .unwrap();

        let outputs = output.outputs().clone();
        (block, inputs, outputs, post_state)
    }

    #[test]
    fn test_process_chunk_blocks_multi_block() {
        let ee = SimpleExecutionEnvironment;

        // Initial state: alice has 1000.
        let mut accounts = BTreeMap::new();
        accounts.insert(alice(), 1000);
        let initial_state = SimplePartialState::new(accounts);

        let genesis_header = SimpleHeader::genesis();
        let genesis_blkid = genesis_header.compute_block_id();

        // Block 1: alice -> bob 200
        let (block1, inp1, out1, state1) = build_block(
            &ee,
            &initial_state,
            genesis_blkid,
            1,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 200,
            }]),
            ExecInputs::new_empty(),
        );
        let blkid1 = block1.get_header().compute_block_id();

        // Block 2: alice -> bob 300
        let (block2, inp2, out2, state2) = build_block(
            &ee,
            &state1,
            blkid1,
            2,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 300,
            }]),
            ExecInputs::new_empty(),
        );
        let blkid2 = block2.get_header().compute_block_id();

        // Block 3: alice -> bob 100
        let (block3, inp3, out3, _state3) = build_block(
            &ee,
            &state2,
            blkid2,
            3,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 100,
            }]),
            ExecInputs::new_empty(),
        );

        // Aggregate inputs and outputs across the chunk.
        let chunk_inputs = ExecInputs::new_empty();

        let chunk_outputs = ExecOutputs::new_empty();

        // Build the chunk.
        let chunk_blocks = vec![
            ChunkBlock::new(&inp1, &out1, block1),
            ChunkBlock::new(&inp2, &out2, block2),
            ChunkBlock::new(&inp3, &out3, block3),
        ];
        let chunk = Chunk::new(chunk_blocks);

        // Process starting from the initial state.
        let mut state = initial_state;
        process_chunk_blocks(
            &ee,
            &mut state,
            &chunk,
            genesis_blkid,
            &chunk_inputs,
            &chunk_outputs,
        )
        .expect("multi-block chunk should process successfully");

        // Verify final balances: alice=1000-200-300-100=400, bob=200+300+100=600
        assert_eq!(state.accounts().get(&alice()), Some(&400));
        assert_eq!(state.accounts().get(&bob()), Some(&600));
    }

    #[test]
    fn process_chunk_blocks_rejects_forged_intermediate_state_root() {
        let ee = SimpleExecutionEnvironment;

        let mut accounts = BTreeMap::new();
        accounts.insert(alice(), 1000);
        let initial_state = SimplePartialState::new(accounts);

        let genesis_header = SimpleHeader::genesis();
        let genesis_blkid = genesis_header.compute_block_id();

        let (block1, inp1, out1, state1) = build_block(
            &ee,
            &initial_state,
            genesis_blkid,
            1,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 200,
            }]),
            ExecInputs::new_empty(),
        );
        let forged_header1 = SimpleHeader::new(genesis_blkid, Hash::from([0x42; 32]), 1);
        let block1 = SimpleBlock::new(forged_header1, block1.get_body().clone());
        let forged_block1_id = block1.get_header().compute_block_id();

        let (block2, inp2, out2, _state2) = build_block(
            &ee,
            &state1,
            forged_block1_id,
            2,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 300,
            }]),
            ExecInputs::new_empty(),
        );

        let chunk = Chunk::new(vec![
            ChunkBlock::new(&inp1, &out1, block1),
            ChunkBlock::new(&inp2, &out2, block2),
        ]);
        let mut state = initial_state;

        let err = process_chunk_blocks(
            &ee,
            &mut state,
            &chunk,
            genesis_blkid,
            &ExecInputs::new_empty(),
            &ExecOutputs::new_empty(),
        )
        .expect_err("forged intermediate state root must be rejected");
        assert!(matches!(err, EnvError::InvalidBlock));
    }

    /// A chunk whose rotating block is followed by an ordinary block must be
    /// rejected: those later blocks would still be authorized by the
    /// predecessor predicate. Host-side sealing keeps this from happening,
    /// but a malicious prover isn't bound by host behavior.
    #[test]
    fn process_chunk_blocks_rejects_block_after_a_rotation() {
        let ee = SimpleExecutionEnvironment;

        let mut accounts = BTreeMap::new();
        accounts.insert(alice(), 1000);
        let initial_state = SimplePartialState::new(accounts);

        let genesis_header = SimpleHeader::genesis();
        let genesis_blkid = genesis_header.compute_block_id();

        // Block 1 consumes the rotation.
        let (block1, inp1, mut out1, state1) = build_block(
            &ee,
            &initial_state,
            genesis_blkid,
            1,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 200,
            }]),
            ExecInputs::new_empty(),
        );
        let new_key = PredicateKey::always_accept();
        out1.set_new_predicate(Some(new_key.clone()));
        let blkid1 = block1.get_header().compute_block_id();

        // Block 2 rides along after it in the same chunk.
        let (block2, inp2, out2, _state2) = build_block(
            &ee,
            &state1,
            blkid1,
            2,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 300,
            }]),
            ExecInputs::new_empty(),
        );

        // The chunk declares exactly the rotation its blocks produced, so the
        // only thing wrong here is that the rotation isn't terminal.
        let mut chunk_outputs = ExecOutputs::new_empty();
        chunk_outputs.set_new_predicate(Some(new_key));

        let chunk = Chunk::new(vec![
            ChunkBlock::new(&inp1, &out1, block1),
            ChunkBlock::new(&inp2, &out2, block2),
        ]);
        let mut state = initial_state;

        let err = process_chunk_blocks(
            &ee,
            &mut state,
            &chunk,
            genesis_blkid,
            &ExecInputs::new_empty(),
            &chunk_outputs,
        )
        .expect_err("a block after a rotation must be rejected");
        assert!(matches!(err, EnvError::NonTerminalRotation));
    }

    /// The mirror of the case above: a rotation on the chunk's last block is
    /// the shape the sequencer actually produces and must still pass.
    #[test]
    fn process_chunk_blocks_accepts_a_rotation_on_the_last_block() {
        let ee = SimpleExecutionEnvironment;

        let mut accounts = BTreeMap::new();
        accounts.insert(alice(), 1000);
        let initial_state = SimplePartialState::new(accounts);

        let genesis_header = SimpleHeader::genesis();
        let genesis_blkid = genesis_header.compute_block_id();

        let (block1, inp1, out1, state1) = build_block(
            &ee,
            &initial_state,
            genesis_blkid,
            1,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 200,
            }]),
            ExecInputs::new_empty(),
        );
        let blkid1 = block1.get_header().compute_block_id();

        let (block2, inp2, mut out2, _state2) = build_block(
            &ee,
            &state1,
            blkid1,
            2,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 300,
            }]),
            ExecInputs::new_empty(),
        );
        let new_key = PredicateKey::always_accept();
        out2.set_new_predicate(Some(new_key.clone()));

        let mut chunk_outputs = ExecOutputs::new_empty();
        chunk_outputs.set_new_predicate(Some(new_key));

        let chunk = Chunk::new(vec![
            ChunkBlock::new(&inp1, &out1, block1),
            ChunkBlock::new(&inp2, &out2, block2),
        ]);
        let mut state = initial_state;

        process_chunk_blocks(
            &ee,
            &mut state,
            &chunk,
            genesis_blkid,
            &ExecInputs::new_empty(),
            &chunk_outputs,
        )
        .expect("a rotation on the last block should process successfully");

        assert_eq!(state.accounts().get(&alice()), Some(&500));
        assert_eq!(state.accounts().get(&bob()), Some(&500));
    }

    #[test]
    fn verify_chunk_transition_rejects_wrong_tip_state_root() {
        let ee = SimpleExecutionEnvironment;

        let mut accounts = BTreeMap::new();
        accounts.insert(alice(), 1000);
        let initial_state = SimplePartialState::new(accounts);

        let prev_header =
            SimpleHeader::new(Hash::zero(), initial_state.compute_state_root().unwrap(), 0);
        let prev_blkid = prev_header.compute_block_id();
        let (block, inputs, outputs, _post_state) = build_block(
            &ee,
            &initial_state,
            prev_blkid,
            1,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 200,
            }]),
            ExecInputs::new_empty(),
        );
        let tip_blkid = block.get_header().compute_block_id();

        let chunk_transition = ChunkTransition::new(
            prev_blkid,
            tip_blkid,
            Hash::from([9u8; 32]),
            block.get_header().get_exec_header_summary(),
            inputs.clone(),
            outputs.clone(),
        );
        let chunk = Chunk::new(vec![ChunkBlock::new(&inputs, &outputs, block)]);
        let mut state = initial_state;

        let err = verify_chunk_transition(&chunk_transition, &ee, &prev_header, &mut state, &chunk)
            .expect_err("wrong tip state root must be rejected");
        assert!(matches!(err, EnvError::MismatchedChainSegment));
    }

    #[test]
    fn verify_chunk_transition_rejects_wrong_tip_header_summary() {
        let ee = SimpleExecutionEnvironment;

        let mut accounts = BTreeMap::new();
        accounts.insert(alice(), 1000);
        let initial_state = SimplePartialState::new(accounts);

        let prev_header =
            SimpleHeader::new(Hash::zero(), initial_state.compute_state_root().unwrap(), 0);
        let prev_blkid = prev_header.compute_block_id();
        let (block, inputs, outputs, post_state) = build_block(
            &ee,
            &initial_state,
            prev_blkid,
            1,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 200,
            }]),
            ExecInputs::new_empty(),
        );
        let tip_blkid = block.get_header().compute_block_id();
        let tip_state_root = post_state.compute_state_root().unwrap();

        let chunk_transition = ChunkTransition::new(
            prev_blkid,
            tip_blkid,
            tip_state_root,
            ExecHeaderSummary::from_vec(vec![1]).unwrap(),
            inputs.clone(),
            outputs.clone(),
        );
        let chunk = Chunk::new(vec![ChunkBlock::new(&inputs, &outputs, block)]);
        let mut state = initial_state;

        let err = verify_chunk_transition(&chunk_transition, &ee, &prev_header, &mut state, &chunk)
            .expect_err("wrong tip header summary must be rejected");
        assert!(matches!(err, EnvError::MismatchedChainSegment));
    }
}
