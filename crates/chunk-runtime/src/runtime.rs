//! Toplevel proof logic using no-copy input types.

use alpen_acct_types::ExecutionEnvironment;

use crate::{
    ArchivedPrivateInput, Chunk, ChunkRuntimeError, ChunkRuntimeResult, compute_chunk_transition,
};

/// Verifies the private input's consistency using the provided execution
/// environment.
pub fn verify_input<E: ExecutionEnvironment>(
    ee: &E,
    input: &ArchivedPrivateInput,
) -> ChunkRuntimeResult<(), E::Error> {
    // 1. Parse the various basic inputs.
    let tsn = input
        .try_decode_chunk_transition()
        .map_err(ChunkRuntimeError::DecodeChunkTransition)?;

    // FIXME(STR-3685): do we actually need the header or just the blkid+state?
    let prev_header = input
        .try_decode_prev_header::<E>()
        .map_err(ChunkRuntimeError::DecodeParentHeader)?;

    let mut pre_state = input
        .try_decode_pre_state::<E>()
        .map_err(ChunkRuntimeError::DecodePreState)?;

    // 2. Parse the blocks into a chunk we can execute.
    let blocks = input
        .raw_chunk()
        .blocks()
        .iter()
        .enumerate()
        .map(|(index, b)| b.try_decode_chunk_block::<E>(index))
        .collect::<Result<Vec<_>, _>>()?;

    let chunk = Chunk::<E>::new(prev_header, blocks);

    // 3. Execute the chunk on the pre state and check the transition it
    // produces is the one claimed.
    // TODO(STR-3685): have the guest commit the computed transition and drop
    // the claimed one from `PrivateInput`. This comparison then goes away. It
    // stays for now because that changes the guest's input and so its
    // verifying key.
    let computed_tsn = compute_chunk_transition(ee, &mut pre_state, chunk)?;
    if computed_tsn != tsn {
        return Err(ChunkRuntimeError::TransitionMismatch {
            expected: Box::new(tsn),
            got: Box::new(computed_tsn),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alpen_acct_types::{ExecBlock, ExecHeader, ExecPartialState, Hash};
    use alpen_chain_types::ChunkTransition;
    use alpen_simple_ee::{
        SimpleBlock, SimpleBlockBody, SimpleEnvError, SimpleExecutionEnvironment, SimpleHeader,
        SimpleHeaderIntrinsics, SimplePartialState, SimpleTransaction,
    };
    use rkyv::rancor::Error as RkyvError;
    use strata_acct_types::SubjectId;
    use strata_codec::encode_to_vec;

    use super::*;
    use crate::{PrivateInput, RawBlockData, RawChunkData};

    /// Builds a one-block chunk on top of a funded pre state, returning the
    /// pieces of its private input and the transition it really produces.
    fn build_chunk() -> (
        SimpleHeader,
        SimplePartialState,
        RawChunkData,
        ChunkTransition,
    ) {
        let ee = SimpleExecutionEnvironment;
        let alice = SubjectId::from([1u8; 32]);
        let bob = SubjectId::from([2u8; 32]);

        let mut accounts = BTreeMap::new();
        accounts.insert(alice, 1000);
        let pre_state = SimplePartialState::new(accounts);

        let prev_header = SimpleHeader::new(Hash::zero(), pre_state.compute_state_root(), 0);
        let prev_blkid = prev_header.compute_block_id();

        let intrinsics = SimpleHeaderIntrinsics {
            parent_blkid: prev_blkid,
            index: 1,
        };
        let body = SimpleBlockBody::new(vec![SimpleTransaction::Transfer {
            from: alice,
            to: bob,
            value: 200,
        }]);
        let inputs = body.exec_inputs();
        let output = ee.execute_body(&pre_state, &body).unwrap();
        let header = ee.complete_header(&intrinsics, &output);
        let outputs = output.outputs().clone();
        let block = SimpleBlock::new(header, body);

        let tsn = ChunkTransition::new(
            prev_blkid,
            block.get_header().compute_block_id(),
            block.get_header().get_state_root(),
            block.get_header().get_exec_header_summary(),
            inputs.clone(),
            outputs.clone(),
        );
        let raw_chunk = RawChunkData::new(vec![
            RawBlockData::from_block::<SimpleExecutionEnvironment>(&block, inputs, outputs)
                .unwrap(),
        ]);

        (prev_header, pre_state, raw_chunk, tsn)
    }

    fn run_verify_input(
        prev_header: &SimpleHeader,
        pre_state: &SimplePartialState,
        raw_chunk: RawChunkData,
        tsn: ChunkTransition,
    ) -> ChunkRuntimeResult<(), SimpleEnvError> {
        let private_input = PrivateInput::new(
            tsn,
            raw_chunk,
            encode_to_vec(prev_header).unwrap(),
            encode_to_vec(pre_state).unwrap(),
        );
        let bytes = rkyv::to_bytes::<RkyvError>(&private_input).unwrap();
        let archived = rkyv::access::<ArchivedPrivateInput, RkyvError>(&bytes).unwrap();
        verify_input(&SimpleExecutionEnvironment, archived)
    }

    #[test]
    fn verify_input_accepts_the_computed_transition() {
        let (prev_header, pre_state, raw_chunk, tsn) = build_chunk();

        run_verify_input(&prev_header, &pre_state, raw_chunk, tsn)
            .expect("matching transition should verify");
    }

    #[test]
    fn verify_input_rejects_a_mismatched_transition() {
        let (prev_header, pre_state, raw_chunk, tsn) = build_chunk();
        let claimed = ChunkTransition::new(
            tsn.parent_exec_blkid(),
            tsn.tip_exec_blkid(),
            Hash::from([9u8; 32]),
            tsn.tip_exec_header_summary().clone(),
            tsn.inputs().clone(),
            tsn.outputs().clone(),
        );

        let err = run_verify_input(&prev_header, &pre_state, raw_chunk, claimed.clone())
            .expect_err("a claimed transition that differs from the computed one must be rejected");
        let ChunkRuntimeError::TransitionMismatch { expected, got } = err else {
            panic!("expected a transition mismatch, got {err:?}");
        };
        assert_eq!(*expected, claimed);
        assert_eq!(*got, tsn);
    }
}
