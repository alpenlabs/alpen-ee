use std::num::NonZero;

use alloy_primitives::B256;
use alpen_ee_common::{DepositInfo, EnginePayload, PayloadBuildAttributes, PayloadBuilderEngine};
use alpen_reth_evm::subject_to_address_unchecked;
use strata_acct_types::Hash;
use strata_ee_acct_types::{EeAccountState, PendingInputEntry, UpdateExtraData};
use tracing::{debug, info};

/// Extracts deposits from pending inputs, limited by `max_deposits`, stopping at a queued
/// predicate rotation if one is reached before the cap — never extracting anything queued
/// after it in the same block.
///
/// Returns the extracted deposits and the total number of pending-input entries to remove
/// from the queue for this block: the deposits taken, plus one more if a rotation was reached
/// (it's consumed as the terminal action of this block, alongside whatever deposits preceded
/// it).
pub(crate) fn extract_deposits(
    pending_inputs: &[PendingInputEntry],
    max_deposits: NonZero<u8>,
    next_deposit_idx: u64,
) -> (Vec<DepositInfo>, usize) {
    let mut deposits = Vec::new();
    let mut processed_inputs = 0usize;

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
                processed_inputs += 1;
            }
            PendingInputEntry::PredicateRotation(_) => {
                processed_inputs += 1;
                break;
            }
        }
    }

    (deposits, processed_inputs)
}

/// Builds the block payload.
///
/// All EE <-> EVM conversions should be contained inside here.
pub(crate) async fn build_exec_payload<E: PayloadBuilderEngine>(
    account_state: &mut EeAccountState,
    parent_exec_blkid: Hash,
    timestamp_ms: u64,
    max_deposits_per_block: NonZero<u8>,
    deposit_counter: u64,
    payload_builder: &E,
) -> eyre::Result<(E::TEnginePayload, UpdateExtraData, u64)> {
    let parent = B256::from_slice(parent_exec_blkid.as_slice());
    let timestamp_sec = timestamp_ms / 1_000;

    let (deposits, processed_inputs) = extract_deposits(
        account_state.pending_inputs(),
        max_deposits_per_block,
        deposit_counter,
    );
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
        .build_payload(PayloadBuildAttributes::new(parent, timestamp_sec, deposits))
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
    fn extract_deposits_with_valid_address() {
        // SubjectId with valid EVM address: [0x00..0x00 (12 bytes), 0xaa..0xaa (20 bytes)]
        let mut subject_bytes = [0u8; 32];
        subject_bytes[12..32].copy_from_slice(&[0xaa; 20]);
        let next_deposit_idx = 5;

        let inputs = vec![make_deposit(subject_bytes, 1000)];
        let (deposits, processed) =
            extract_deposits(&inputs, NonZero::new(10).unwrap(), next_deposit_idx);

        assert_eq!(deposits.len(), 1);
        assert_eq!(processed, 1);
        assert_eq!(deposits[0].address(), Address::from([0xaa; 20]));
        assert_eq!(deposits[0].idx(), 5);
    }

    #[test]
    fn extract_deposits_limits_to_max() {
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

        let (deposits, processed) = extract_deposits(&inputs, max, next_deposit_idx);

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
        PendingInputEntry::PredicateRotation(strata_predicate::PredicateKey::always_accept())
    }

    #[test]
    fn extract_deposits_stops_at_a_rotation() {
        let inputs = vec![
            make_deposit([0x01; 32], 1000),
            make_rotation(),
            make_deposit([0x02; 32], 2000),
        ];

        let (deposits, processed) = extract_deposits(&inputs, NonZero::new(10).unwrap(), 0);

        // The deposit after the rotation is never extracted in this block.
        assert_eq!(deposits.len(), 1);
        assert_eq!(deposits[0].amount(), BitcoinAmount::from_sat(1000));
        // But the rotation itself is drained alongside the deposit before it.
        assert_eq!(processed, 2);
    }

    #[test]
    fn extract_deposits_rotation_at_the_cap_is_not_reached() {
        // The deposit cap is hit before the rotation is ever inspected.
        let inputs = vec![
            make_deposit([0x01; 32], 1000),
            make_deposit([0x02; 32], 2000),
            make_rotation(),
        ];

        let (deposits, processed) = extract_deposits(&inputs, NonZero::new(2).unwrap(), 0);

        assert_eq!(deposits.len(), 2);
        assert_eq!(processed, 2);
    }
}
