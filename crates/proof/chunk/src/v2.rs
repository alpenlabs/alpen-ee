//! The chunk input layout the V2 guests read.
//!
//! These are this source's own types, which the V2 guests decode directly.
//! Until the V2 release ships, they may change, and the test below is updated
//! with them. After it ships, a layout change breaks the deployed V2 guest:
//! copy the released types here in place of the re-exports, as [`v1`] does.
//!
//! [`v1`]: super::v1

pub use alpen_chunk_runtime::{PrivateInput, RawChunkData};

#[cfg(test)]
mod tests {
    use alpen_chain_types::{ChunkTransition, ExecHeaderSummary, ExecInputs, ExecOutputs};
    use alpen_chunk_runtime::RawBlockData;
    use rkyv::rancor::Error as RkyvError;
    use strata_acct_types::Hash;

    use super::*;

    /// The input below in the V2 layout.
    const V2_BYTES: &str = "0101010101010101010101010101010101010101010101010101010101010101020202020202020202020202020202020202020202020202020202020202020203030303030303030303030303030303030303030303030303030303030303036c000000700000007400000004000000040000000c0000000c0000000c00000000b1b2040000000c0000000c0000000c00000000b3040000000c0000000c0000000c000000000000d9ffffff02000000d3ffffff04000000cfffffff0d000000d4ffffff01000000cdffffff04000000c9ffffff0d000000a1a2a3c1c200000020ffffff81000000c0ffffff02000000e8ffffff03000000e3ffffff02000000";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn test_layout_is_pinned() {
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
        let input = PrivateInput::new(
            transition,
            RawChunkData::new(blocks),
            vec![0xa1, 0xa2, 0xa3],
            vec![0xc1, 0xc2],
        );

        let bytes = rkyv::to_bytes::<RkyvError>(&input).expect("encode V2 chunk input");
        assert_eq!(hex(&bytes), V2_BYTES);
    }
}
