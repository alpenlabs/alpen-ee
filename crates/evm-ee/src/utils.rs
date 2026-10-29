//! Utility functions for EVM block execution.
//!
//! This module contains utility functions used during block execution that don't
//! belong to any specific type.

use alloy_consensus::BlockBody;
use alpen_chain_types::ExecInputs;
use alpen_primitives::{sats_to_gwei, subject_to_address_unchecked};
use reth_ethereum_primitives::{Block, TransactionSigned};
use reth_primitives_traits::RecoveredBlock;

use crate::{errors::EvmEnvError, types::EvmBlock};

/// Recovers the transaction senders of an EVM block.
pub(crate) fn recover_block(block: EvmBlock) -> Result<RecoveredBlock<Block>, EvmEnvError> {
    block
        .into_sealed()
        .try_recover()
        .map_err(|_| EvmEnvError::SenderRecovery)
}

/// Validates that deposits from ExecInputs match the withdrawals field in the block.
///
/// In Alpen, the EIP-4895 withdrawals field is hijacked to represent deposits from the
/// orchestration layer. This function ensures that the authenticated deposits in ExecInputs
/// match what's committed in the block's withdrawals field.
///
/// # Warning
/// **Deposits and withdrawals must be in the same order.** This function performs a
/// pairwise comparison using `zip()`, so the nth deposit must match the nth withdrawal.
/// Any reordering will cause validation to fail.
///
/// # Mapping
/// - `Withdrawal.address` ← last 20 bytes of `SubjectDepositData.dest` (SubjectId)
/// - `Withdrawal.amount` ← `SubjectDepositData.value` in Gwei
/// - `Withdrawal.index` and `Withdrawal.validator_index` are ignored (not meaningful for deposits)
pub(crate) fn validate_deposits_against_block(
    body: &BlockBody<TransactionSigned>,
    inputs: &ExecInputs,
) -> Result<(), EvmEnvError> {
    // Get withdrawals from the block body (this is where deposits are stored)
    let block_withdrawals = body
        .withdrawals
        .as_ref()
        .map(|w| w.as_slice())
        .unwrap_or(&[]);

    let subject_deposits = inputs.subject_deposits();

    // Check counts match
    if block_withdrawals.len() != subject_deposits.len() {
        return Err(EvmEnvError::DepositCountMismatch {
            deposits: subject_deposits.len(),
            withdrawals: block_withdrawals.len(),
        });
    }

    // Validate each deposit matches the corresponding withdrawal
    for (index, (withdrawal, deposit)) in block_withdrawals
        .iter()
        .zip(subject_deposits.iter())
        .enumerate()
    {
        let expected_address = subject_to_address_unchecked(&deposit.dest());

        // Convert satoshis to gwei (1 sat = 10 gwei, per 1 BTC = 10^8 sats = 10^9 gwei)
        let expected_amount = sats_to_gwei(deposit.value().to_sat())
            .ok_or(EvmEnvError::DepositAmountOverflow { index })?;

        // Validate address and amount match
        if withdrawal.address != expected_address || withdrawal.amount != expected_amount {
            return Err(EvmEnvError::DepositMismatch { index });
        }

        // Note: withdrawal.index and withdrawal.validator_index are not validated
        // as they are not meaningful in the deposit context
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip4895::Withdrawal;
    use alpen_chain_types::{ExecInputs, SubjectDepositData};
    use revm_primitives::Address;
    use strata_acct_types::{BitcoinAmount, SubjectId};

    use super::*;

    #[test]
    fn test_validate_deposits_valid_match() {
        // Create valid subject ID: [0x00..0x00 (12 bytes), address (20 bytes)]
        let mut subject_bytes = [0u8; 32];
        subject_bytes[31] = 0x42; // Last byte of address
        let subject_id = SubjectId::new(subject_bytes);
        let expected_address = subject_to_address_unchecked(&subject_id);

        // Create a block body with matching withdrawal
        let body = BlockBody {
            transactions: vec![],
            ommers: vec![],
            withdrawals: Some(
                vec![Withdrawal {
                    index: 0,
                    validator_index: 0,
                    address: expected_address,
                    amount: 100, // 10 sats * 10 = 100 gwei
                }]
                .into(),
            ),
        };

        // Create matching deposit input
        let mut inputs = ExecInputs::new_empty();
        let deposit = SubjectDepositData::new(subject_id, BitcoinAmount::try_from(10).unwrap());
        inputs.add_subject_deposit(deposit);

        // Should succeed - perfect match
        let result = validate_deposits_against_block(&body, &inputs);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_deposits_address_mismatch() {
        // Valid subject ID but different address
        let mut subject_bytes = [0u8; 32];
        subject_bytes[31] = 0xff; // Different address
        let subject_id = SubjectId::new(subject_bytes);

        // Create a block body with different address
        let body = BlockBody {
            transactions: vec![],
            ommers: vec![],
            withdrawals: Some(
                vec![Withdrawal {
                    index: 0,
                    validator_index: 0,
                    address: Address::ZERO, // Wrong address
                    amount: 100,
                }]
                .into(),
            ),
        };

        // Create deposit with different address
        let mut inputs = ExecInputs::new_empty();
        let deposit = SubjectDepositData::new(subject_id, BitcoinAmount::try_from(10).unwrap());
        inputs.add_subject_deposit(deposit);

        // Should fail - address mismatch
        let result = validate_deposits_against_block(&body, &inputs);
        assert!(matches!(
            result,
            Err(EvmEnvError::DepositMismatch { index: 0 })
        ));
    }

    #[test]
    fn test_validate_deposits_non_zero_padding_truncates() {
        // Verification truncates non-zero padding, the same as block assembly.
        let subject_id = SubjectId::new([0xff; 32]);

        // Create a block body
        let body = BlockBody {
            transactions: vec![],
            ommers: vec![],
            withdrawals: Some(
                vec![Withdrawal {
                    index: 0,
                    validator_index: 0,
                    address: Address::from([0xff; 20]),
                    amount: 100,
                }]
                .into(),
            ),
        };

        let mut inputs = ExecInputs::new_empty();
        let deposit = SubjectDepositData::new(subject_id, BitcoinAmount::try_from(10).unwrap());
        inputs.add_subject_deposit(deposit);

        let result = validate_deposits_against_block(&body, &inputs);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_deposits_amount_mismatch() {
        let mut subject_bytes = [0u8; 32];
        subject_bytes[31] = 0x42;
        let subject_id = SubjectId::new(subject_bytes);
        let expected_address = subject_to_address_unchecked(&subject_id);

        // Create a block body with wrong amount
        let body = BlockBody {
            transactions: vec![],
            ommers: vec![],
            withdrawals: Some(
                vec![Withdrawal {
                    index: 0,
                    validator_index: 0,
                    address: expected_address,
                    amount: 200, // Wrong amount (should be 100)
                }]
                .into(),
            ),
        };

        // Create deposit with different amount
        let mut inputs = ExecInputs::new_empty();
        let deposit = SubjectDepositData::new(subject_id, BitcoinAmount::try_from(10).unwrap()); // 10 sats = 100 gwei
        inputs.add_subject_deposit(deposit);

        // Should fail - amount mismatch
        let result = validate_deposits_against_block(&body, &inputs);
        assert!(matches!(
            result,
            Err(EvmEnvError::DepositMismatch { index: 0 })
        ));
    }
}
