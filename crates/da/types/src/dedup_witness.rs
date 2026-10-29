//! Supplementary "dedup" witness: data the guest needs to verify a published DA
//! blob whose references aren't all carried inline.

use rkyv::{Archive, Deserialize, Serialize};
use rkyv_impl::archive_impl;

/// Extra data the guest needs to verify a published DA blob whose references
/// aren't carried inline.
///
/// DA dedup lets a batch omit bytecode that an account already holds in the
/// batch's pre-state. A later account diff can still reference that code's
/// hash, so the host names a holder for each omitted code hash, and the guest
/// reads the holder's code hash from the authenticated pre-state. The holder
/// took on that code in an earlier batch, whose blob carried the bytes (or the
/// code is in genesis), so a reader of L1 can always recover them.
///
/// Future dedup kinds (account/storage serials) add their own fields here.
#[derive(Clone, Debug, Default, Archive, Deserialize, Serialize)]
#[rkyv(derive(Debug))]
pub struct DedupWitness {
    /// State trie keys (`keccak256(address)`) of accounts that hold, in the
    /// batch's pre-state, the code the blob omits.
    code_holder_keys: Vec<[u8; 32]>,
}

impl DedupWitness {
    pub fn new(code_holder_keys: Vec<[u8; 32]>) -> Self {
        Self { code_holder_keys }
    }

    pub fn empty() -> Self {
        Self::default()
    }
}

#[archive_impl]
impl DedupWitness {
    pub fn code_holder_keys(&self) -> &[[u8; 32]] {
        &self.code_holder_keys
    }
}

#[cfg(test)]
mod tests {
    use rkyv::rancor::Error as RkyvError;

    use super::*;

    #[test]
    fn dedup_witness_with_code_holder_roundtrips_through_rkyv() {
        let witness = DedupWitness::new(vec![[0x11; 32]]);

        let bytes = rkyv::to_bytes::<RkyvError>(&witness).unwrap();
        let archived = rkyv::access::<ArchivedDedupWitness, RkyvError>(&bytes).unwrap();

        assert_eq!(archived.code_holder_keys(), &[[0x11; 32]]);
    }
}
