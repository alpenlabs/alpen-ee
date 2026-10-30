//! The chunk input layout the V1 guest reads.
//!
//! V1's only SP1 program is the v0.4.0 release. Its guest reads an older
//! [`PrivateInput`](alpen_chunk_runtime::PrivateInput) layout, whose chunk data
//! still carries a `prev_exec_blkid` that later releases dropped. This module
//! keeps that layout so the host can still feed the V1 guest, and so the
//! native host can read it when it proves V1.
//!
//! rkyv lays out a struct by its field types and their order, so these types
//! must not change. The tests pin them to bytes the v0.4.0 types produced.
//!
//! Delete this module and the `V1` input variant once no network proves V1,
//! that is once every network has rotated to V2.

use alpen_chain_types::ChunkTransition;
use alpen_chunk_runtime::{
    ArchivedPrivateInput as ArchivedCurrentInput, PrivateInput as CurrentInput, RawBlockData,
};
use alpen_params::{AlpenParams, AlpenSpecId};
use rkyv::{Archive, Deserialize, Serialize, rancor::Error as RkyvError};
use ssz::{Decode, Encode};
use zkaleido::ZkVmEnvSerde;

use crate::verify_and_commit;

/// Native counterpart of [`crate::process_ee_chunk`] for an input in the V1
/// layout.
///
/// It runs this source's chunk check, not the v0.4.0 guest's.
pub fn process_ee_chunk(zkvm: &impl ZkVmEnvSerde, params: &AlpenParams, spec_version: AlpenSpecId) {
    let buf = zkvm.read_buf();
    let input =
        rkyv::from_bytes::<PrivateInput, RkyvError>(&buf).expect("failed to decode V1 chunk input");
    let current_bytes =
        rkyv::to_bytes::<RkyvError>(&input.into_current()).expect("failed to encode chunk input");
    let current = rkyv::access::<ArchivedCurrentInput, RkyvError>(&current_bytes)
        .expect("failed to access rkyv archive");

    verify_and_commit(zkvm, params, spec_version, current);
}

/// Private input of the V1 chunk guest.
#[derive(Debug, Archive, Deserialize, Serialize)]
pub struct PrivateInput {
    chunk_transition_ssz: Vec<u8>,
    raw_chunk: RawChunkData,
    raw_prev_header: Vec<u8>,
    raw_partial_pre_state: Vec<u8>,
}

impl PrivateInput {
    pub fn new(
        chunk_transition: ChunkTransition,
        blocks: Vec<RawBlockData>,
        raw_prev_header: Vec<u8>,
        raw_partial_pre_state: Vec<u8>,
    ) -> Self {
        let raw_chunk = RawChunkData {
            blocks,
            prev_exec_blkid: chunk_transition.parent_exec_blkid().0,
        };
        Self {
            chunk_transition_ssz: chunk_transition.as_ssz_bytes(),
            raw_chunk,
            raw_prev_header,
            raw_partial_pre_state,
        }
    }

    /// Converts the input to the current layout, dropping `prev_exec_blkid`.
    fn into_current(self) -> CurrentInput {
        let chunk_transition = ChunkTransition::from_ssz_bytes(&self.chunk_transition_ssz)
            .expect("failed to decode chunk transition");
        CurrentInput::new(
            chunk_transition,
            alpen_chunk_runtime::RawChunkData::new(self.raw_chunk.blocks),
            self.raw_prev_header,
            self.raw_partial_pre_state,
        )
    }
}

#[derive(Debug, Archive, Deserialize, Serialize)]
struct RawChunkData {
    blocks: Vec<RawBlockData>,

    /// The chunk's parent block ID. The V1 guest never reads it.
    prev_exec_blkid: [u8; 32],
}

#[cfg(test)]
mod tests {
    use alpen_chain_types::{ExecHeaderSummary, ExecInputs, ExecOutputs};
    use rkyv::util::AlignedVec;
    use strata_acct_types::Hash;

    use super::*;

    /// The input below, encoded by the v0.4.0 types.
    const V040_BYTES: &str = "0101010101010101010101010101010101010101010101010101010101010101020202020202020202020202020202020202020202020202020202020202020203030303030303030303030303030303030303030303030303030303030303036c000000700000007400000004000000040000000c0000000c0000000c00000000b1b2040000000c0000000c0000000c00000000b3040000000c0000000c0000000c000000000000d9ffffff02000000d3ffffff04000000cfffffff0d000000d4ffffff01000000cdffffff04000000c9ffffff0d000000a1a2a3c1c200000020ffffff81000000c0ffffff020000000101010101010101010101010101010101010101010101010101010101010101c8ffffff03000000c3ffffff02000000";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn unhex(hex: &str) -> AlignedVec {
        let mut bytes = AlignedVec::new();
        for start in (0..hex.len()).step_by(2) {
            bytes.push(u8::from_str_radix(&hex[start..start + 2], 16).expect("valid hex"));
        }
        bytes
    }

    fn sample_parts() -> (ChunkTransition, Vec<RawBlockData>) {
        let transition = ChunkTransition::new(
            Hash::from([1; 32]),
            Hash::from([2; 32]),
            Hash::from([3; 32]),
            ExecHeaderSummary::new_empty(),
            ExecInputs::new_empty(),
            ExecOutputs::new_empty(),
        );
        let blocks = vec![
            RawBlockData::new(
                vec![0xb1, 0xb2],
                ExecInputs::new_empty(),
                ExecOutputs::new_empty(),
            ),
            RawBlockData::new(
                vec![0xb3],
                ExecInputs::new_empty(),
                ExecOutputs::new_empty(),
            ),
        ];
        (transition, blocks)
    }

    #[test]
    fn test_layout_matches_v040() {
        let (transition, blocks) = sample_parts();
        let input = PrivateInput::new(transition, blocks, vec![0xa1, 0xa2, 0xa3], vec![0xc1, 0xc2]);

        let bytes = rkyv::to_bytes::<RkyvError>(&input).expect("encode V1 chunk input");
        assert_eq!(hex(&bytes), V040_BYTES);
    }

    #[test]
    fn test_v040_input_converts_to_current_layout() {
        let input = rkyv::from_bytes::<PrivateInput, RkyvError>(&unhex(V040_BYTES))
            .expect("decode V1 chunk input");
        let (transition, blocks) = sample_parts();
        let expected = CurrentInput::new(
            transition,
            alpen_chunk_runtime::RawChunkData::new(blocks),
            vec![0xa1, 0xa2, 0xa3],
            vec![0xc1, 0xc2],
        );

        let converted = rkyv::to_bytes::<RkyvError>(&input.into_current()).expect("encode");
        let expected = rkyv::to_bytes::<RkyvError>(&expected).expect("encode");
        assert_eq!(converted.as_slice(), expected.as_slice());
    }
}
