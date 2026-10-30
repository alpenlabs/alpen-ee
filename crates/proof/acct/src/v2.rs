//! The account input layout the V2 guests read.
//!
//! These are this source's own types, which the V2 guests decode directly.
//! Until the V2 release ships, they may change, and the tests below are
//! updated with them. After it ships, a layout change breaks the deployed V2
//! guest: copy the released types here in place of the re-exports, as [`v1`]
//! does.
//!
//! The snark account input is not here. Both layouts carry the same type, and
//! the program's tests pin it.
//!
//! [`v1`]: super::v1

pub use alpen_acct_runtime::EePrivateInput;
pub use alpen_da_types::DaWitness;

#[cfg(test)]
mod tests {
    use alpen_acct_runtime::ChunkInput;
    use alpen_chain_types::{ChunkTransition, ExecHeaderSummary, ExecInputs, ExecOutputs};
    use alpen_da_types::{
        BitcoinMerkleProof, DaBlockWitness, DaTxWitness, DedupWitness, L1DaBlockInclusion,
    };
    use rkyv::rancor::Error as RkyvError;
    use strata_acct_types::Hash;

    use super::*;

    /// The EE input below, in the V2 layout.
    const V2_EE_BYTES: &str = "c1c20101010101010101010101010101010101010101010101010101010101010101020202020202020202020202020202020202020202020202020202020202020203030303030303030303030303030303030303030303030303030303030303036c000000700000007400000004000000040000000c0000000c0000000c00000000d1d2d300007affffff81000000f3ffffff0300000068ffffff02000000e8ffffff01000000";

    /// The DA witness below, in the V2 layout.
    const V2_DA_BYTES: &str = "e1e233333333333333333333333333333333333333333333333333333333333333330000dcffffff02000000d6ffffff01000000010000000700000011111111111111111111111111111111111111111111111111111111111111112222222222222222222222222222222222222222222222222222222222222222a8ffffff01000000444444444444444444444444444444444444444444444444444444444444444494ffffff01000000d8ffffff01000000";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn test_ee_input_layout_is_pinned() {
        let transition = ChunkTransition::new(
            Hash::from([1; 32]),
            Hash::from([2; 32]),
            Hash::from([3; 32]),
            ExecHeaderSummary::new_empty(),
            ExecInputs::new_empty(),
            ExecOutputs::new_empty(),
        );
        let input = EePrivateInput::new(
            vec![0xc1, 0xc2],
            vec![ChunkInput::new(transition, vec![0xd1, 0xd2, 0xd3])],
        );

        let bytes = rkyv::to_bytes::<RkyvError>(&input).expect("encode V2 EE input");
        assert_eq!(hex(&bytes), V2_EE_BYTES);
    }

    #[test]
    fn test_da_witness_layout_is_pinned() {
        let block = DaBlockWitness::new(
            L1DaBlockInclusion::new(7, [0x11; 32], [0x22; 32]),
            vec![DaTxWitness::new(
                vec![0xe1, 0xe2],
                BitcoinMerkleProof::new(vec![[0x33; 32]], 1),
            )],
        );
        let witness = DaWitness::new(vec![block], DedupWitness::new(vec![[0x44; 32]]));

        let bytes = rkyv::to_bytes::<RkyvError>(&witness).expect("encode V2 DA witness");
        assert_eq!(hex(&bytes), V2_DA_BYTES);
    }
}
