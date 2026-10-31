//! The account input layout the V1 guest reads.
//!
//! V1's only SP1 program is the v0.4.0 release. Its guest reads two inputs in
//! older layouts than this source's:
//!
//! - The EE input still carries a `raw_prev_header`, which the guest never reads.
//! - The dedup witness carries the bytes of each bytecode the DA blob omits. The guest hashes them
//!   to cover the account diffs that reference that code. Later releases name a pre-state account
//!   holding the code instead.
//!
//! This module keeps those layouts so the host can still feed the V1 guest, and
//! so the native host can read them when it proves V1.
//! rkyv lays out a struct by its field types and their order, so these types
//! must not change. The tests pin them to bytes the v0.4.0 types produced.
//!
//! Delete this module and the `V1` input variant once no network proves V1,
//! that is once every network has rotated to V2.

use alpen_acct_runtime::{
    ArchivedEePrivateInput as ArchivedCurrentEeInput, ChunkInput, EePrivateInput as CurrentEeInput,
};
use alpen_da_runtime::verification::OmittedCodeWitness;
use alpen_da_types::DaBlockWitness;
use alpen_params::AlpenSpecId;
use rkyv::{Archive, Deserialize, Serialize, rancor::Error as RkyvError};
use strata_predicate::PredicateKey;
use strata_snark_acct_runtime::ArchivedPrivateInput as ArchivedUpdatePrivateInput;
use zkaleido::ZkVmEnvSerde;

use crate::verify_and_commit;

/// Native counterpart of [`crate::process_ee_acct_update`] for inputs in the
/// V1 layout.
///
/// It runs this source's account check, except that the omitted bytecode is
/// covered by its bytes, as the v0.4.0 guest checks it.
pub fn process_ee_acct_update(
    zkvm: &impl ZkVmEnvSerde,
    spec_version: AlpenSpecId,
    chunk_predicate_key: &PredicateKey,
) {
    let ee_buf = zkvm.read_buf();
    let ee_input = rkyv::from_bytes::<EePrivateInput, RkyvError>(&ee_buf)
        .expect("failed to decode V1 EE input");
    let current_ee_bytes =
        rkyv::to_bytes::<RkyvError>(&ee_input.into_current()).expect("failed to encode EE input");
    let current_ee_input = rkyv::access::<ArchivedCurrentEeInput, RkyvError>(&current_ee_bytes)
        .expect("failed to access rkyv EE archive");

    let upd_buf = zkvm.read_buf();
    let upd_input = rkyv::access::<ArchivedUpdatePrivateInput, RkyvError>(&upd_buf)
        .expect("failed to access rkyv update archive");

    let da_buf = zkvm.read_buf();
    let da_witness = rkyv::access::<ArchivedDaWitness, RkyvError>(&da_buf)
        .expect("failed to access rkyv V1 DA witness archive");

    verify_and_commit(
        zkvm,
        spec_version,
        chunk_predicate_key,
        current_ee_input,
        upd_input,
        &da_witness.blocks,
        OmittedCodeWitness::Bytecodes(da_witness.omitted_bytecodes()),
    );
}

/// EE private input of the V1 account guest.
#[derive(Debug, Archive, Deserialize, Serialize)]
pub struct EePrivateInput {
    raw_prev_header: Vec<u8>,
    raw_partial_pre_state: Vec<u8>,
    chunks: Vec<ChunkInput>,
}

impl EePrivateInput {
    /// Creates the input with the empty `raw_prev_header` the V1 guest expects.
    pub fn new(raw_partial_pre_state: Vec<u8>, chunks: Vec<ChunkInput>) -> Self {
        Self {
            raw_prev_header: Vec::new(),
            raw_partial_pre_state,
            chunks,
        }
    }

    /// Converts the input to the current layout, dropping `raw_prev_header`.
    fn into_current(self) -> CurrentEeInput {
        CurrentEeInput::new(self.raw_partial_pre_state, self.chunks)
    }
}

/// DA witness of the V1 account guest.
#[derive(Debug, Archive, Deserialize, Serialize)]
pub struct DaWitness {
    blocks: Vec<DaBlockWitness>,
    dedup_da_witness: DedupWitness,
}

impl DaWitness {
    /// Creates the witness from the L1 inclusion blocks and the bytes of each
    /// bytecode the DA blob omits.
    pub fn new(blocks: Vec<DaBlockWitness>, omitted_bytecodes: Vec<Vec<u8>>) -> Self {
        let deduped_bytecode_preimages = omitted_bytecodes
            .into_iter()
            .map(|bytecode| BytecodePreimage { bytecode })
            .collect();
        Self {
            blocks,
            dedup_da_witness: DedupWitness {
                deduped_bytecode_preimages,
            },
        }
    }
}

impl ArchivedDaWitness {
    /// Returns the bytes of each bytecode the DA blob omits.
    fn omitted_bytecodes(&self) -> Vec<&[u8]> {
        self.dedup_da_witness
            .deduped_bytecode_preimages
            .iter()
            .map(|preimage| preimage.bytecode.as_slice())
            .collect()
    }
}

#[derive(Debug, Archive, Deserialize, Serialize)]
struct DedupWitness {
    deduped_bytecode_preimages: Vec<BytecodePreimage>,
}

#[derive(Debug, Archive, Deserialize, Serialize)]
struct BytecodePreimage {
    bytecode: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use alpen_chain_types::{ChunkTransition, ExecHeaderSummary, ExecInputs, ExecOutputs};
    use alpen_da_types::{BitcoinMerkleProof, DaTxWitness, L1DaBlockInclusion};
    use rkyv::util::AlignedVec;
    use strata_acct_types::Hash;

    use super::*;

    /// The EE input below, encoded by the v0.4.0 types.
    const V040_EE_BYTES: &str = "c1c20101010101010101010101010101010101010101010101010101010101010101020202020202020202020202020202020202020202020202020202020202020203030303030303030303030303030303030303030303030303030303030303036c000000700000007400000004000000040000000c0000000c0000000c00000000d1d2d300007affffff81000000f3ffffff0300000068ffffff0000000060ffffff02000000e0ffffff01000000";

    /// The DA witness below, encoded by the v0.4.0 types.
    const V040_DA_BYTES: &str = "e1e233333333333333333333333333333333333333333333333333333333333333330000dcffffff02000000d6ffffff01000000010000000700000011111111111111111111111111111111111111111111111111111111111111112222222222222222222222222222222222222222222222222222222222222222a8ffffff010000006080fe00fcffffff02000000f6ffffff01000000a0ffffff01000000e8ffffff02000000";

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

    fn sample_chunks() -> Vec<ChunkInput> {
        let transition = ChunkTransition::new(
            Hash::from([1; 32]),
            Hash::from([2; 32]),
            Hash::from([3; 32]),
            ExecHeaderSummary::new_empty(),
            ExecInputs::new_empty(),
            ExecOutputs::new_empty(),
        );
        vec![ChunkInput::new(transition, vec![0xd1, 0xd2, 0xd3])]
    }

    #[test]
    fn test_ee_input_layout_matches_v040() {
        let input = EePrivateInput::new(vec![0xc1, 0xc2], sample_chunks());

        let bytes = rkyv::to_bytes::<RkyvError>(&input).expect("encode V1 EE input");
        assert_eq!(hex(&bytes), V040_EE_BYTES);
    }

    #[test]
    fn test_da_witness_layout_matches_v040() {
        let block = DaBlockWitness::new(
            L1DaBlockInclusion::new(7, [0x11; 32], [0x22; 32]),
            vec![DaTxWitness::new(
                vec![0xe1, 0xe2],
                BitcoinMerkleProof::new(vec![[0x33; 32]], 1),
            )],
        );
        let witness = DaWitness::new(vec![block], vec![vec![0x60, 0x80], vec![0xfe]]);

        let bytes = rkyv::to_bytes::<RkyvError>(&witness).expect("encode V1 DA witness");
        assert_eq!(hex(&bytes), V040_DA_BYTES);
    }

    #[test]
    fn test_v040_ee_input_converts_to_current_layout() {
        let input = rkyv::from_bytes::<EePrivateInput, RkyvError>(&unhex(V040_EE_BYTES))
            .expect("decode V1 EE input");
        let expected = CurrentEeInput::new(vec![0xc1, 0xc2], sample_chunks());

        let converted = rkyv::to_bytes::<RkyvError>(&input.into_current()).expect("encode");
        let expected = rkyv::to_bytes::<RkyvError>(&expected).expect("encode");
        assert_eq!(converted.as_slice(), expected.as_slice());
    }

    #[test]
    fn test_v040_da_witness_yields_its_bytecodes() {
        let bytes = unhex(V040_DA_BYTES);
        let witness =
            rkyv::access::<ArchivedDaWitness, RkyvError>(&bytes).expect("access V1 DA witness");

        assert_eq!(witness.blocks.len(), 1);
        assert_eq!(
            witness.omitted_bytecodes(),
            [&[0x60, 0x80][..], &[0xfe][..]]
        );
    }
}
