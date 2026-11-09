//! The key the OL checks a batch's update against.

use alpen_common::{Batch, SequencerOLClient};
use alpen_storage_api::{BatchStorage, ExecBlockStorage};
use eyre::{eyre, Context, Result};
use strata_predicate::PredicateKey;

/// Returns the account predicate key the OL will check `batch`'s update
/// against.
///
/// Batch `n` carries update `n - 1`, and the OL checks each update against
/// the key it holds when it applies the update. That is the key it holds now,
/// changed by each rotation consumed in a batch it has not accepted yet,
/// before `batch`. The batch builder seals on every rotation-consuming block,
/// so a rotation can only be consumed by the last block of its batch, and only
/// those blocks need reading. The OL's sequence number and key come from one
/// read of its account state, so they describe the same state.
///
/// The walk goes back from `batch` and stops at the first earlier batch that
/// ends on a rotation.
pub async fn update_key_for_batch<C, B, E>(
    ol_client: &C,
    batch_storage: &B,
    exec_storage: &E,
    batch: &Batch,
) -> Result<PredicateKey>
where
    C: SequencerOLClient + ?Sized,
    B: BatchStorage + ?Sized,
    E: ExecBlockStorage + ?Sized,
{
    let account = ol_client
        .get_latest_account_state()
        .await
        .context("failed to read the OL account state")?;
    let next_unaccepted = account
        .seq_no
        .inner()
        .checked_add(1)
        .ok_or_else(|| eyre!("max sequence number exceeded"))?;
    if batch.idx() < next_unaccepted {
        return Err(eyre!(
            "the OL has already accepted batch {}, so its key is no longer known",
            batch.idx()
        ));
    }

    // The key of batch `idx + 1` is the rotation batch `idx` ends on, if any,
    // and otherwise the key of batch `idx`.
    let mut idx = batch.idx();
    while idx > next_unaccepted {
        idx -= 1;
        let (earlier, _) = batch_storage
            .get_batch_by_idx(idx)
            .await?
            .ok_or_else(|| eyre!("missing batch {idx} before batch {}", batch.idx()))?;
        let last_block = exec_storage
            .get_exec_block(earlier.last_block())
            .await?
            .ok_or_else(|| {
                eyre!(
                    "missing exec block {} ending batch {idx}",
                    earlier.last_block()
                )
            })?;
        if let Some(rotated) = last_block.package().outputs().new_predicate() {
            return Ok(rotated.clone());
        }
    }

    Ok(account.update_vk)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alpen_acct_types::EeAccountState;
    use alpen_chain_types::{ExecBlockCommitment, ExecBlockPackage, ExecInputs, ExecOutputs};
    use alpen_common::{BatchStatus, ExecBlockRecord, MockSequencerOLClient, OLAccountStateView};
    use alpen_params::AlpenSpecId;
    use alpen_storage_api::{MockBatchStorage, MockExecBlockStorage};
    use strata_acct_types::Hash;
    use strata_identifiers::{Buf32, OLBlockCommitment, OLBlockId};
    use strata_predicate::PredicateTypeId;
    use strata_snark_acct_types::{ProofState, Seqno};

    use super::*;

    fn key(byte: u8) -> PredicateKey {
        PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, vec![byte; 32]).unwrap()
    }

    fn block_hash(n: u64) -> Hash {
        let mut bytes = [0u8; 32];
        bytes[24..].copy_from_slice(&n.to_be_bytes());
        Hash::from(bytes)
    }

    /// Batch `idx` covers the single block `idx`.
    fn batch(idx: u64) -> Batch {
        Batch::new(
            idx,
            block_hash(idx - 1),
            block_hash(idx),
            idx,
            vec![],
            AlpenSpecId::V2,
        )
        .unwrap()
    }

    /// The record of block `n`, which consumes a rotation to `rotation` if
    /// one is given.
    fn block(n: u64, rotation: Option<PredicateKey>) -> ExecBlockRecord {
        let mut outputs = ExecOutputs::new_empty();
        outputs.set_new_predicate(rotation);
        let package = ExecBlockPackage::new(
            ExecBlockCommitment::new(block_hash(n), block_hash(n)),
            ExecInputs::new_empty(),
            outputs,
        );
        ExecBlockRecord::new(
            package,
            EeAccountState::new(block_hash(n), Hash::zero(), vec![], vec![]),
            n,
            OLBlockCommitment::new(n, OLBlockId::from(Buf32::zero())),
            n * 1_000,
            block_hash(n - 1),
            0,
            0,
            AlpenSpecId::V2,
            vec![],
        )
    }

    /// An OL that has accepted every batch up to `accepted` and holds `vk`.
    fn ol(accepted: u64, vk: PredicateKey) -> MockSequencerOLClient {
        let mut ol = MockSequencerOLClient::new();
        ol.expect_get_latest_account_state().returning(move || {
            Ok(OLAccountStateView {
                seq_no: Seqno::from(accepted),
                proof_state: ProofState::new(Hash::zero(), 0),
                update_vk: vk.clone(),
            })
        });
        ol
    }

    /// Storage holding batches `1..=last`, where the batches in `rotations`
    /// end on a block that consumes a rotation to the given key.
    fn storage(
        last: u64,
        rotations: HashMap<u64, PredicateKey>,
    ) -> (MockBatchStorage, MockExecBlockStorage) {
        let mut batches = MockBatchStorage::new();
        batches.expect_get_batch_by_idx().returning(move |idx| {
            Ok((1..=last)
                .contains(&idx)
                .then(|| (batch(idx), BatchStatus::Sealed)))
        });
        let mut blocks = MockExecBlockStorage::new();
        blocks.expect_get_exec_block().returning(move |hash| {
            let n = (1..=last).find(|n| block_hash(*n) == hash);
            Ok(n.map(|n| block(n, rotations.get(&n).cloned())))
        });
        (batches, blocks)
    }

    #[tokio::test]
    async fn the_first_unaccepted_batch_takes_the_ols_key() {
        let (batches, blocks) = storage(5, HashMap::new());

        let resolved = update_key_for_batch(&ol(2, key(1)), &batches, &blocks, &batch(3))
            .await
            .unwrap();

        assert_eq!(resolved, key(1));
    }

    #[tokio::test]
    async fn a_batch_after_an_unaccepted_rotation_takes_its_key() {
        // Batch 4 ends on a block that consumes a rotation to key 2. The OL
        // checks batch 4 itself against key 1, and every batch after it
        // against key 2.
        let (batches, blocks) = storage(6, HashMap::from([(4, key(2))]));
        let ol = ol(2, key(1));

        for (idx, expected) in [(3, key(1)), (4, key(1)), (5, key(2)), (6, key(2))] {
            let resolved = update_key_for_batch(&ol, &batches, &blocks, &batch(idx))
                .await
                .unwrap();
            assert_eq!(resolved, expected, "batch {idx}");
        }
    }

    #[tokio::test]
    async fn a_rotation_the_ol_accepted_is_already_in_its_key() {
        // The OL accepted batch 4 and switched to key 2, so batch 5 must not
        // replay the rotation on top of a key that already holds it.
        let (batches, blocks) = storage(6, HashMap::from([(4, key(2))]));

        let resolved = update_key_for_batch(&ol(4, key(2)), &batches, &blocks, &batch(5))
            .await
            .unwrap();

        assert_eq!(resolved, key(2));
    }

    #[tokio::test]
    async fn a_batch_the_ol_accepted_is_refused() {
        let (batches, blocks) = storage(5, HashMap::new());

        let err = update_key_for_batch(&ol(3, key(1)), &batches, &blocks, &batch(3))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("already accepted"), "{err}");
    }
}
