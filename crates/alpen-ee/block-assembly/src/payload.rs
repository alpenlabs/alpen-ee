use std::num::NonZero;

use alloy_primitives::B256;
use alpen_ee_common::{DepositInfo, EnginePayload, PayloadBuildAttributes, PayloadBuilderEngine};
use alpen_ee_params::AlpenSpecId;
use alpen_reth_evm::subject_to_address_unchecked;
use strata_acct_types::Hash;
use strata_ee_acct_types::{PendingInputEntry, UpdateExtraData};
use strata_predicate::PredicateKey;
use tracing::{debug, info};

/// Entries the next block drains from the account's pending-input queue.
pub(crate) struct ConsumedInputs {
    /// Deposits to mint in the block, with their assigned indices.
    pub(crate) deposits: Vec<DepositInfo>,
    /// How many pending entries the block drains, counting a rotation.
    pub(crate) processed: usize,
    /// The predicate rotation the block drains, if it reached one.
    ///
    /// A rotation ends the block, so it is always the last drained entry.
    /// This is the only place a consumed rotation is determined, so everything
    /// that depends on one reads it from here and they cannot disagree.
    pub(crate) new_predicate: Option<PredicateKey>,
}

/// Walks the pending queue in order and returns what the next block consumes.
///
/// Deposits are taken up to `max_deposits`. A `PredicateRotation` is drained
/// too but ends the block, so nothing queued after it is taken.
pub(crate) fn extract_consumed_inputs(
    pending_inputs: &[PendingInputEntry],
    max_deposits: NonZero<u8>,
    next_deposit_idx: u64,
) -> ConsumedInputs {
    let mut deposits = Vec::new();
    let mut processed = 0usize;
    let mut new_predicate = None;

    for entry in pending_inputs {
        if deposits.len() >= max_deposits.get() as usize {
            break;
        }
        match entry {
            PendingInputEntry::Deposit(data) => {
                deposits.push(DepositInfo::new(
                    next_deposit_idx + deposits.len() as u64,
                    subject_to_address_unchecked(&data.dest()),
                    data.value(),
                ));
                processed += 1;
            }
            PendingInputEntry::PredicateRotation(key) => {
                processed += 1;
                new_predicate = Some(key.clone());
                break;
            }
        }
    }

    ConsumedInputs {
        deposits,
        processed,
        new_predicate,
    }
}

/// Builds the block payload.
///
/// All EE <-> EVM conversions should be contained inside here.
pub(crate) async fn build_exec_payload<E: PayloadBuilderEngine>(
    deposits: Vec<DepositInfo>,
    processed_inputs: usize,
    parent_exec_blkid: Hash,
    timestamp_ms: u64,
    deposit_counter: u64,
    spec_version: AlpenSpecId,
    payload_builder: &E,
) -> eyre::Result<(E::TEnginePayload, UpdateExtraData, u64)> {
    let parent = B256::from_slice(parent_exec_blkid.as_slice());
    let timestamp_sec = timestamp_ms / 1_000;

    let deposits_processed = deposits.len() as u64;
    let processed_inputs = processed_inputs as u32;
    // dont handle forced inclusions currently
    let processed_fincls = 0;

    for (deposit_index, deposit) in deposits.iter().enumerate() {
        info!(
            %parent,
            deposit_index,
            address = %deposit.address(),
            amount_sat = deposit.amount().to_sat(),
            "selected deposit for EE payload",
        );
    }

    debug!(%parent, timestamp = %timestamp_sec, deposits = %processed_inputs, "starting payload build");
    let payload = payload_builder
        .build_payload(PayloadBuildAttributes::new(
            parent,
            timestamp_sec,
            deposits,
            spec_version,
        ))
        .await?;

    let new_tip_blkid = payload.blockhash();
    let new_tip_state_root = payload.state_root();
    debug!(
        ?new_tip_blkid,
        ?new_tip_state_root,
        "payload build complete"
    );

    let update_extra_data = UpdateExtraData::new(
        new_tip_blkid,
        new_tip_state_root,
        processed_inputs,
        processed_fincls,
    );

    Ok((
        payload,
        update_extra_data,
        deposit_counter + deposits_processed,
    ))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use strata_acct_types::{BitcoinAmount, SubjectId};
    use strata_ee_chain_types::SubjectDepositData;

    use super::*;

    fn make_deposit(dest_bytes: [u8; 32], sats: u64) -> PendingInputEntry {
        PendingInputEntry::Deposit(SubjectDepositData::new(
            SubjectId::new(dest_bytes),
            BitcoinAmount::from_sat(sats),
        ))
    }

    #[test]
    fn extract_consumed_inputs_with_valid_address() {
        // SubjectId with valid EVM address: [0x00..0x00 (12 bytes), 0xaa..0xaa (20 bytes)]
        let mut subject_bytes = [0u8; 32];
        subject_bytes[12..32].copy_from_slice(&[0xaa; 20]);
        let next_deposit_idx = 5;

        let inputs = vec![make_deposit(subject_bytes, 1000)];
        let ConsumedInputs {
            deposits,
            processed,
            ..
        } = extract_consumed_inputs(&inputs, NonZero::new(10).unwrap(), next_deposit_idx);

        assert_eq!(deposits.len(), 1);
        assert_eq!(processed, 1);
        assert_eq!(deposits[0].address(), Address::from([0xaa; 20]));
        assert_eq!(deposits[0].idx(), 5);
    }

    #[test]
    fn extract_consumed_inputs_limits_to_max() {
        // Create valid SubjectIds with zero-padded first 12 bytes
        let mut subject1 = [0u8; 32];
        subject1[12..32].copy_from_slice(&[0x01; 20]);
        let mut subject2 = [0u8; 32];
        subject2[12..32].copy_from_slice(&[0x02; 20]);
        let mut subject3 = [0u8; 32];
        subject3[12..32].copy_from_slice(&[0x03; 20]);
        let mut subject4 = [0u8; 32];
        subject4[12..32].copy_from_slice(&[0x04; 20]);
        let mut subject5 = [0u8; 32];
        subject5[12..32].copy_from_slice(&[0x05; 20]);

        let inputs = vec![
            make_deposit(subject1, 1000),
            make_deposit(subject2, 2000),
            make_deposit(subject3, 3000),
            make_deposit(subject4, 4000),
            make_deposit(subject5, 5000),
        ];
        let max = NonZero::new(3).unwrap();
        let next_deposit_idx = 9;

        let ConsumedInputs {
            deposits,
            processed,
            ..
        } = extract_consumed_inputs(&inputs, max, next_deposit_idx);

        assert_eq!(deposits.len(), 3);
        assert_eq!(processed, 3);
        // Verify order is preserved (first 3)
        assert_eq!(deposits[0].amount(), BitcoinAmount::from_sat(1000));
        assert_eq!(deposits[0].idx(), 9);
        assert_eq!(deposits[1].amount(), BitcoinAmount::from_sat(2000));
        assert_eq!(deposits[1].idx(), 10);
        assert_eq!(deposits[2].amount(), BitcoinAmount::from_sat(3000));
        assert_eq!(deposits[2].idx(), 11);
    }

    fn make_rotation() -> PendingInputEntry {
        PendingInputEntry::PredicateRotation(PredicateKey::always_accept())
    }

    #[test]
    fn extract_consumed_inputs_stops_at_a_rotation() {
        let inputs = vec![
            make_deposit([0x01; 32], 1000),
            make_rotation(),
            make_deposit([0x02; 32], 2000),
        ];

        let consumed = extract_consumed_inputs(&inputs, NonZero::new(10).unwrap(), 0);

        // The deposit after the rotation is never extracted in this block.
        assert_eq!(consumed.deposits.len(), 1);
        assert_eq!(consumed.deposits[0].amount(), BitcoinAmount::from_sat(1000));
        // But the rotation itself is drained alongside the deposit before it.
        assert_eq!(consumed.processed, 2);
        assert_eq!(consumed.new_predicate, Some(PredicateKey::always_accept()));
    }

    #[test]
    fn extract_consumed_inputs_rotation_at_the_cap_is_not_reached() {
        // The deposit cap is hit before the rotation is ever inspected.
        let inputs = vec![
            make_deposit([0x01; 32], 1000),
            make_deposit([0x02; 32], 2000),
            make_rotation(),
        ];

        let consumed = extract_consumed_inputs(&inputs, NonZero::new(2).unwrap(), 0);

        assert_eq!(consumed.deposits.len(), 2);
        assert_eq!(consumed.processed, 2);
        // The rotation stays queued, so this block doesn't consume it.
        assert_eq!(consumed.new_predicate, None);

        // With room to spare the same queue does reach it.
        let consumed = extract_consumed_inputs(&inputs, NonZero::new(3).unwrap(), 0);
        assert_eq!(consumed.new_predicate, Some(PredicateKey::always_accept()));
    }
}
