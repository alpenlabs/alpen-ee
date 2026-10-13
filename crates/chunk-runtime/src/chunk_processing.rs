use alpen_acct_types::{ExecBlock, ExecHeader, ExecPartialState, ExecutionEnvironment};
use alpen_chain_types::{ChunkTransition, ExecInputs, ExecOutputs};

use crate::{
    ChunkRuntimeError, ChunkRuntimeResult,
    chunk::{Chunk, ChunkBlock},
};

/// Processes a block from a chunk with associated inputs, merging results into
/// the passed state.
///
/// `parent_header` is the header of the block this one builds on.
pub fn process_block<E: ExecutionEnvironment>(
    ee: &E,
    state: &mut E::PartialState,
    parent_header: &<E::Block as ExecBlock>::Header,
    block: &ChunkBlock<'_, E>,
) -> ChunkRuntimeResult<(), E::Error> {
    let eb = block.exec_block();
    let header = eb.get_header();

    // This also checks the block builds on its parent.
    ee.verify_header(parent_header, header)?;

    ee.verify_block_pre_execution(eb)?;

    // Check the inputs against the block, then execute it and check the
    // outputs against the header.
    ee.verify_inputs_against_block(eb, block.inputs())?;
    let exec_outp = ee.execute_block(state, eb)?;
    ee.verify_block_post_execution(header, &exec_outp)?;

    // Check that the EVM-derivable outputs match the chunk block.
    //
    // `new_predicate` is deliberately left out of this comparison: it's an
    // account-level fact drawn from the pending-input queue, not something
    // `execute_block` can derive from EVM execution alone (see `evm-ee`'s
    // `execute_block`, which only ever sets output messages/transfers and
    // always leaves `new_predicate` empty). Chunk-level verification of a
    // declared rotation against real block execution belongs to a
    // cross-check over the chunk's blocks, not this per-block comparison.
    if exec_outp.outputs().output_transfers() != block.outputs().output_transfers()
        || exec_outp.outputs().output_messages() != block.outputs().output_messages()
    {
        return Err(ChunkRuntimeError::OutputMismatch {
            expected: Box::new(block.outputs().clone()),
            got: Box::new(exec_outp.outputs().clone()),
        });
    }

    // Merge the changes and return the outputs.
    ee.merge_write_into_state(state, exec_outp.write_batch())?;
    let computed_state_root = state.compute_state_root();
    let header_state_root = header.get_state_root();
    if computed_state_root != header_state_root {
        return Err(ChunkRuntimeError::PostStateRootMismatch {
            expected: header_state_root,
            got: computed_state_root,
        });
    }

    Ok(())
}

/// Chunk-level IO built up from the per-block IO of a chunk's blocks.
struct ChunkIo {
    inputs: ExecInputs,
    outputs: ExecOutputs,
}

impl ChunkIo {
    fn new_empty() -> Self {
        Self {
            inputs: ExecInputs::new_empty(),
            outputs: ExecOutputs::new_empty(),
        }
    }

    /// Appends the next block's inputs and outputs.
    fn extend_from_block<EnvErr>(
        &mut self,
        inps: &ExecInputs,
        outps: &ExecOutputs,
    ) -> ChunkRuntimeResult<(), EnvErr> {
        // A rotation ends the chunk. The sequencer stops assembling at one and
        // seals right after it, but that's only host behavior — without this
        // check a proof could carry blocks that ran after the rotation, and
        // they'd be authorized by the predecessor predicate.
        if self.outputs.new_predicate().is_some() {
            return Err(ChunkRuntimeError::NonTerminalRotation);
        }

        // FIXME(STR-2172): these panic once the chunk's IO goes over the SSZ
        // list bounds. Switch to fallible pushes when chain-types has them.
        for d in inps.subject_deposits() {
            self.inputs.add_subject_deposit(d.clone());
        }
        for t in outps.output_transfers() {
            self.outputs.add_transfer(t.clone());
        }
        for m in outps.output_messages() {
            self.outputs.add_message(m.clone());
        }

        // The guard above makes this block the last one we accept for the
        // chunk, so its rotation is the chunk's rotation.
        if let Some(key) = outps.new_predicate() {
            self.outputs.set_new_predicate(Some(key.clone()));
        }

        Ok(())
    }
}

/// Processes a chunk's blocks and updates the state, returning the chunk-level
/// IO built from the blocks' IO.
fn process_chunk_blocks<E: ExecutionEnvironment>(
    ee: &E,
    state: &mut E::PartialState,
    chunk: &Chunk<'_, E>,
) -> ChunkRuntimeResult<(ExecInputs, ExecOutputs), E::Error> {
    let mut chunk_io = ChunkIo::new_empty();
    let mut parent_header = chunk.prev_header();
    for cb in chunk.blocks() {
        process_block(ee, state, parent_header, cb)?;
        chunk_io.extend_from_block(cb.inputs(), cb.outputs())?;
        parent_header = cb.exec_block().get_header();
    }

    Ok((chunk_io.inputs, chunk_io.outputs))
}

/// Computes the chunk transition by executing the chunk on top of the pre
/// state.
pub fn compute_chunk_transition<E: ExecutionEnvironment>(
    ee: &E,
    state: &mut E::PartialState,
    chunk: &Chunk<'_, E>,
) -> ChunkRuntimeResult<ChunkTransition, E::Error> {
    let prev_header = chunk.prev_header();

    // 1. Make sure the chunk is nonempty.
    let Some(tip_header) = chunk.blocks().last().map(|b| b.exec_block().get_header()) else {
        return Err(ChunkRuntimeError::EmptyChunk);
    };

    // 2. Make sure the state matches the parent block's state root.
    let computed_pre_sr = state.compute_state_root();
    if computed_pre_sr != prev_header.get_state_root() {
        return Err(ChunkRuntimeError::PreStateRootMismatch {
            expected: prev_header.get_state_root(),
            got: computed_pre_sr,
        });
    }

    // 3. Execute the blocks in the chunk. Each block's post-state root is
    // verified before its header can be used as the parent of a later block.
    let (inputs, outputs) = process_chunk_blocks(ee, state, chunk)?;

    // 4. Compute the final state root and make sure it matches the tip.
    let tip_state_root = state.compute_state_root();
    if tip_state_root != tip_header.get_state_root() {
        return Err(ChunkRuntimeError::PostStateRootMismatch {
            expected: tip_header.get_state_root(),
            got: tip_state_root,
        });
    }

    Ok(ChunkTransition::new(
        prev_header.compute_block_id(),
        tip_header.compute_block_id(),
        tip_state_root,
        tip_header.get_exec_header_summary(),
        inputs,
        outputs,
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alpen_acct_types::{EnvError, ExecBlock, ExecBlockOutput, ExecHeader, Hash};
    use alpen_chain_types::{ExecInputs, ExecOutputs};
    use alpen_simple_ee::{
        SimpleBlock, SimpleBlockBody, SimpleExecutionEnvironment, SimpleHeader,
        SimpleHeaderIntrinsics, SimplePartialState, SimpleTransaction,
    };
    use strata_predicate::PredicateKey;

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
    ) -> (SimpleBlock, ExecInputs, ExecOutputs, SimplePartialState) {
        let intrinsics = SimpleHeaderIntrinsics {
            parent_blkid,
            index,
        };
        let inputs = body.exec_inputs();
        let output: ExecBlockOutput<SimpleExecutionEnvironment> =
            ee.execute_body(state, &body).unwrap();

        let header = ee.complete_header(&intrinsics, &output);
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
        );

        // Build the chunk.
        let chunk_blocks = vec![
            ChunkBlock::new(&inp1, &out1, block1),
            ChunkBlock::new(&inp2, &out2, block2),
            ChunkBlock::new(&inp3, &out3, block3),
        ];
        let chunk = Chunk::new(genesis_header, chunk_blocks);

        // Process starting from the initial state.
        let mut state = initial_state;
        let (chunk_inputs, chunk_outputs) = process_chunk_blocks(&ee, &mut state, &chunk)
            .expect("multi-block chunk should process successfully");
        assert_eq!(chunk_inputs, ExecInputs::new_empty());
        assert_eq!(chunk_outputs, ExecOutputs::new_empty());

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
        );

        let chunk = Chunk::new(
            genesis_header,
            vec![
                ChunkBlock::new(&inp1, &out1, block1),
                ChunkBlock::new(&inp2, &out2, block2),
            ],
        );
        let mut state = initial_state;

        let err = process_chunk_blocks(&ee, &mut state, &chunk)
            .expect_err("forged intermediate state root must be rejected");
        assert!(matches!(
            err,
            ChunkRuntimeError::PostStateRootMismatch { .. }
        ));
    }

    /// The runtime leaves the parent link to `verify_header`, so a block that
    /// doesn't build on the one before it must still stop the chunk.
    #[test]
    fn process_chunk_blocks_rejects_a_block_off_the_parent() {
        let ee = SimpleExecutionEnvironment;

        let mut accounts = BTreeMap::new();
        accounts.insert(alice(), 1000);
        let initial_state = SimplePartialState::new(accounts);

        let (block, inp, out, _state) = build_block(
            &ee,
            &initial_state,
            Hash::from([7u8; 32]),
            1,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 200,
            }]),
        );

        let chunk = Chunk::new(
            SimpleHeader::genesis(),
            vec![ChunkBlock::new(&inp, &out, block)],
        );
        let mut state = initial_state;

        let err = process_chunk_blocks(&ee, &mut state, &chunk)
            .expect_err("a block off the parent must be rejected");
        assert!(matches!(
            err,
            ChunkRuntimeError::Env(EnvError::InvalidHeader)
        ));
    }

    /// A header the environment refuses stops the chunk, even when the block
    /// links to its parent and executes. The simple EE refuses a block whose
    /// index skips one.
    #[test]
    fn process_chunk_blocks_rejects_a_header_the_environment_refuses() {
        let ee = SimpleExecutionEnvironment;

        let mut accounts = BTreeMap::new();
        accounts.insert(alice(), 1000);
        let initial_state = SimplePartialState::new(accounts);

        let genesis_header = SimpleHeader::genesis();
        let genesis_blkid = genesis_header.compute_block_id();

        let (block, inp, out, _state) = build_block(
            &ee,
            &initial_state,
            genesis_blkid,
            2,
            SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
                from: alice(),
                to: bob(),
                value: 200,
            }]),
        );

        let chunk = Chunk::new(genesis_header, vec![ChunkBlock::new(&inp, &out, block)]);
        let mut state = initial_state;

        let err = process_chunk_blocks(&ee, &mut state, &chunk)
            .expect_err("a header that skips an index must be rejected");
        assert!(matches!(
            err,
            ChunkRuntimeError::Env(EnvError::InvalidHeader)
        ));
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
        );
        let new_key = PredicateKey::always_accept();
        out1.set_new_predicate(Some(new_key));
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
        );

        let chunk = Chunk::new(
            genesis_header,
            vec![
                ChunkBlock::new(&inp1, &out1, block1),
                ChunkBlock::new(&inp2, &out2, block2),
            ],
        );
        let mut state = initial_state;

        let err = process_chunk_blocks(&ee, &mut state, &chunk)
            .expect_err("a block after a rotation must be rejected");
        assert!(matches!(err, ChunkRuntimeError::NonTerminalRotation));
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
        );
        let new_key = PredicateKey::always_accept();
        out2.set_new_predicate(Some(new_key.clone()));

        let chunk = Chunk::new(
            genesis_header,
            vec![
                ChunkBlock::new(&inp1, &out1, block1),
                ChunkBlock::new(&inp2, &out2, block2),
            ],
        );
        let mut state = initial_state;

        let (_, chunk_outputs) = process_chunk_blocks(&ee, &mut state, &chunk)
            .expect("a rotation on the last block should process successfully");
        assert_eq!(chunk_outputs.new_predicate(), Some(&new_key));

        assert_eq!(state.accounts().get(&alice()), Some(&500));
        assert_eq!(state.accounts().get(&bob()), Some(&500));
    }

    #[test]
    fn compute_chunk_transition_matches_expected() {
        let ee = SimpleExecutionEnvironment;

        let mut accounts = BTreeMap::new();
        accounts.insert(alice(), 1000);
        let initial_state = SimplePartialState::new(accounts);

        let prev_header = SimpleHeader::new(Hash::zero(), initial_state.compute_state_root(), 0);
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
        );

        let chunk_transition = ChunkTransition::new(
            prev_blkid,
            block.get_header().compute_block_id(),
            post_state.compute_state_root(),
            block.get_header().get_exec_header_summary(),
            inputs.clone(),
            outputs.clone(),
        );
        let chunk = Chunk::new(prev_header, vec![ChunkBlock::new(&inputs, &outputs, block)]);

        let mut state = initial_state;

        let computed = compute_chunk_transition(&ee, &mut state, &chunk)
            .expect("chunk should compute a transition");
        assert_eq!(computed, chunk_transition);
    }
}
