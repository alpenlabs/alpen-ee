//! EVM bytecode-dedup layer of the DA witness: determining which account
//! bytecodes the published blob omits (because a pre-state account holds them),
//! naming a holder for each, and the [`DedupWitnessResolver`] seam that turns a
//! published blob into a [`DedupWitness`].

use std::collections::BTreeSet;

use alloy_primitives::{keccak256, Address, B256, KECCAK256_EMPTY};
use alpen_common::DaContext;
use alpen_da_types::{DaBlob, DedupWitness};
use alpen_reth_statediff::AccountChange;
use async_trait::async_trait;

use super::DaWitnessBuildError;

/// Resolves the supplementary [`DedupWitness`] the guest needs to verify a
/// published DA blob whose references aren't all carried inline.
///
/// Today only bytecode holders are resolved; future DA-dedup optimizations
/// (e.g. account/storage serials) add per-kind resolution here.
#[async_trait]
pub trait DedupWitnessResolver {
    /// Names an account holding, in the batch's pre-state, each bytecode the
    /// blob references but omits.
    ///
    /// The prover adds these accounts to the batch's partial pre-state, so the
    /// guest can read their code hashes.
    async fn resolve_code_holders(
        &self,
        blob: &DaBlob,
    ) -> Result<Vec<Address>, DaWitnessBuildError>;
}

/// Host-side [`DedupWitnessResolver`] backed by the sequencer's DA filter.
///
/// The publisher left a bytecode out only after checking that the filter's
/// holder for it still held it in the batch's pre-state. The filter never
/// replaces a holder once recorded, so the same lookup here names that same
/// account.
#[derive(Debug)]
pub struct DaDedupResolver<'a, C: ?Sized> {
    da_ctx: &'a C,
}

impl<'a, C: ?Sized> DaDedupResolver<'a, C> {
    pub fn new(da_ctx: &'a C) -> Self {
        Self { da_ctx }
    }
}

#[async_trait]
impl<C> DedupWitnessResolver for DaDedupResolver<'_, C>
where
    C: DaContext + Sync + ?Sized,
{
    async fn resolve_code_holders(
        &self,
        blob: &DaBlob,
    ) -> Result<Vec<Address>, DaWitnessBuildError> {
        deduped_account_code_hashes(blob)
            .into_iter()
            .map(|code_hash| {
                self.da_ctx
                    .code_hash_holder(&code_hash)
                    .map_err(|e| DaWitnessBuildError::CodeHolderStore {
                        hash: format!("{code_hash:?}"),
                        error: e.to_string(),
                    })?
                    .ok_or_else(|| DaWitnessBuildError::CodeHolderMissing(format!("{code_hash:?}")))
            })
            .collect()
    }
}

/// Builds the [`DedupWitness`] naming `code_holders` by their state trie keys.
pub(crate) fn dedup_witness_for_holders(code_holders: &[Address]) -> DedupWitness {
    DedupWitness::new(
        code_holders
            .iter()
            .map(|holder| keccak256(holder).0)
            .collect(),
    )
}

/// Returns account code hashes referenced by the blob but absent from the current blob bytecodes.
///
/// These are the hashes affected by DA bytecode dedup: the account diff still
/// advertises a `code_hash`, but the current L1 blob no longer carries the
/// matching bytecode bytes.
pub(crate) fn deduped_account_code_hashes(blob: &DaBlob) -> Vec<B256> {
    let mut missing = BTreeSet::new();

    for change in blob.state_diff.accounts.values() {
        let account_diff = match change {
            AccountChange::Created(diff) | AccountChange::Updated(diff) => diff,
            AccountChange::Deleted => continue,
        };
        let Some(code_hash) = account_diff.code_hash.new_value().map(|hash| hash.0) else {
            continue;
        };
        if code_hash == KECCAK256_EMPTY
            || blob.state_diff.deployed_bytecodes.contains_key(&code_hash)
        {
            continue;
        }
        missing.insert(code_hash);
    }

    missing.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::{Bytes, U256};
    use alpen_da_types::EvmHeaderSummary;
    use alpen_params::AlpenSpecId;
    use alpen_reth_statediff::{AccountDiff, BatchStateDiff};
    use strata_db_types::DbResult;

    use super::*;

    /// DA filter stub answering holder lookups from a fixed map.
    struct Holders(BTreeMap<B256, Address>);

    impl DaContext for Holders {
        fn code_hash_holder(&self, code_hash: &B256) -> DbResult<Option<Address>> {
            Ok(self.0.get(code_hash).copied())
        }

        fn mark_code_hashes_published(&self, _holders: &[(B256, Address)]) -> DbResult<()> {
            unimplemented!("read-only stub")
        }

        fn update_da_filter(&self, _block_hashes: &[B256]) -> DbResult<()> {
            unimplemented!("read-only stub")
        }
    }

    /// A blob with two contracts: one whose bytecode it carries, one whose
    /// bytecode it leaves out.
    fn blob_with_one_deduped_code() -> (DaBlob, B256) {
        let inline = Bytes::from_static(&[0x60, 0x01]);
        let deduped_code_hash = keccak256([0x60, 0x02]);

        let mut state_diff = BatchStateDiff::new();
        state_diff.accounts.insert(
            Address::from([0x11; 20]),
            AccountChange::Created(AccountDiff::new_created(U256::ZERO, 1, keccak256(&inline))),
        );
        state_diff.accounts.insert(
            Address::from([0x22; 20]),
            AccountChange::Created(AccountDiff::new_created(U256::ZERO, 1, deduped_code_hash)),
        );
        state_diff
            .deployed_bytecodes
            .insert(keccak256(&inline), inline);

        let blob = DaBlob {
            spec_version: AlpenSpecId::V1,
            update_seq_no: 7,
            evm_header: EvmHeaderSummary {
                block_num: 10,
                timestamp: 1_700_000_000,
                base_fee: 100,
                gas_used: 21_000,
                gas_limit: 36_000_000,
                da_rate: Some(2_500_000_000),
            },
            state_diff,
        };
        (blob, deduped_code_hash)
    }

    #[tokio::test]
    async fn resolver_names_the_holder_of_each_deduped_code_hash() {
        let (blob, deduped_code_hash) = blob_with_one_deduped_code();
        let holder = Address::from([0x33; 20]);
        let holders = Holders(BTreeMap::from([(deduped_code_hash, holder)]));

        let resolved = DaDedupResolver::new(&holders)
            .resolve_code_holders(&blob)
            .await
            .unwrap();

        assert_eq!(resolved, vec![holder]);
        assert_eq!(
            dedup_witness_for_holders(&resolved).code_holder_keys(),
            &[keccak256(holder).0]
        );
    }

    #[tokio::test]
    async fn resolver_fails_when_a_deduped_code_hash_has_no_holder() {
        let (blob, _) = blob_with_one_deduped_code();
        let holders = Holders(BTreeMap::new());

        let err = DaDedupResolver::new(&holders)
            .resolve_code_holders(&blob)
            .await
            .expect_err("an omitted code hash with no holder must fail");

        assert!(matches!(err, DaWitnessBuildError::CodeHolderMissing(_)));
    }
}
