use std::mem::size_of;

use alloy_consensus::{EthereumReceipt, EthereumTxEnvelope, TxEip4844};
use alloy_primitives::{address, Address, Log};
use alloy_sol_types::{sol, SolEvent};
use serde::{Deserialize, Serialize};
use strata_bridge_params::BridgeParams;
use strata_ol_bridge_types::OperatorSelection;
use strata_primitives::{bitcoin_bosd::Descriptor, buf::Buf32};
use thiserror::Error;

/// The address for the Bridgeout precompile contract.
pub const BRIDGEOUT_PRECOMPILE_ADDRESS: Address =
    address!("5400000000000000000000000000000000000001");

/// A withdrawal that a transaction requested through the bridge-out
/// precompile, decoded from its [`WithdrawalIntentEvent`] log.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct WithdrawalIntent {
    /// Amount to be withdrawn in sats.
    pub amt: u64,

    /// User's operator selection for withdrawal assignment.
    pub selected_operator: OperatorSelection,

    /// Dynamic-sized bytes BOSD descriptor for the withdrawal destinations in L1.
    pub destination: Descriptor,
}

sol! {
    event WithdrawalIntentEvent(
        /// Withdrawal amount in sats.
        uint64 amount,
        /// Selected operator index. `u32::MAX` means no selection.
        uint32 selectedOperator,
        /// BOSD descriptor for withdrawal destinations in L1.
        bytes destination,
    );
}

/// Structured calldata for the bridge-out withdrawal precompile.
///
/// Wire format: `[4 bytes: operator index (big-endian u32)][BOSD bytes]`
/// - `u32::MAX` (`0xFFFFFFFF`): no operator selection (any operator)
/// - Any other value: specific operator index
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawalCalldata {
    /// User's operator selection for withdrawal assignment.
    pub selected_operator: OperatorSelection,

    /// Raw BOSD descriptor bytes.
    pub bosd: Vec<u8>,
}

/// Size of the operator index field in calldata (u32 = 4 bytes).
const OPERATOR_INDEX_SIZE: usize = size_of::<u32>();

impl WithdrawalCalldata {
    /// Encodes the calldata to bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(OPERATOR_INDEX_SIZE + self.bosd.len());
        buf.extend_from_slice(&self.selected_operator.raw().to_be_bytes());
        buf.extend_from_slice(&self.bosd);
        buf
    }

    /// Decodes calldata from bytes.
    ///
    /// Returns `None` if the data is too short (needs at least 5 bytes: 4 operator + 1 BOSD).
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() <= OPERATOR_INDEX_SIZE {
            return None;
        }

        let (operator_bytes, bosd) = data.split_at(OPERATOR_INDEX_SIZE);
        let raw = u32::from_be_bytes(operator_bytes.try_into().expect("exactly 4 bytes"));

        Some(Self {
            selected_operator: OperatorSelection::from_raw(raw),
            bosd: bosd.to_vec(),
        })
    }
}

/// Error returned when a bridge-out log cannot be decoded into a valid withdrawal intent.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum WithdrawalIntentExtractionError {
    #[error("failed to decode withdrawal intent event for tx {txid:?}")]
    EventDecode { txid: Buf32 },
    #[error("withdrawal descriptor length {len} exceeds maximum {max} for tx {txid:?}")]
    InvalidDescriptorLength { txid: Buf32, len: usize, max: u32 },
    #[error("failed to decode withdrawal descriptor for tx {txid:?}")]
    DescriptorDecode { txid: Buf32 },
}

/// Extracts withdrawal intents from bridge-out events in transaction receipts.
/// Returns an error if a log emitted by the bridge-out precompile is malformed.
///
/// The parameter types are the alloy types behind reth's `TransactionSigned`
/// and `Receipt` aliases, so reth callers pass their slices as they are.
///
/// # Panics
///
/// Panics if the number of transactions does not match the number of receipts.
pub fn extract_withdrawal_intents(
    transactions: &[EthereumTxEnvelope<TxEip4844>],
    receipts: &[EthereumReceipt],
    bridge_params: &BridgeParams,
) -> Result<Vec<WithdrawalIntent>, WithdrawalIntentExtractionError> {
    assert_eq!(
        transactions.len(),
        receipts.len(),
        "transactions and receipts must have the same length"
    );

    let mut intents = Vec::new();

    for (tx, receipt) in transactions.iter().zip(receipts.iter()) {
        let txid = Buf32((*tx.hash()).into());
        for log in &receipt.logs {
            if let Some(intent) = extract_withdrawal_intent_from_log(txid, log, bridge_params)? {
                intents.push(intent);
            }
        }
    }

    Ok(intents)
}

fn extract_withdrawal_intent_from_log(
    txid: Buf32,
    log: &Log,
    bridge_params: &BridgeParams,
) -> Result<Option<WithdrawalIntent>, WithdrawalIntentExtractionError> {
    if log.address != BRIDGEOUT_PRECOMPILE_ADDRESS {
        return Ok(None);
    }

    let event = WithdrawalIntentEvent::decode_log(log)
        .map_err(|_| WithdrawalIntentExtractionError::EventDecode { txid })?;

    let descriptor_len = event.destination.len();
    if !bridge_params.validate_withdrawal_descriptor_len(descriptor_len) {
        return Err(WithdrawalIntentExtractionError::InvalidDescriptorLength {
            txid,
            len: descriptor_len,
            max: bridge_params.max_withdrawal_descriptor_len(),
        });
    }

    let destination = Descriptor::from_bytes(&event.destination)
        .map_err(|_| WithdrawalIntentExtractionError::DescriptorDecode { txid })?;

    Ok(Some(WithdrawalIntent {
        amt: event.amount,
        destination,
        selected_operator: OperatorSelection::from_raw(event.selectedOperator),
    }))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, LogData};
    use strata_bridge_params::DEFAULT_MAX_WITHDRAWAL_DESCRIPTOR_LEN;

    use super::*;

    fn test_bridge_params() -> BridgeParams {
        BridgeParams::new_with_descriptor_limit(
            100_000_000,
            Some(1_000_000_000),
            DEFAULT_MAX_WITHDRAWAL_DESCRIPTOR_LEN,
        )
        .expect("valid bridge params")
    }

    fn test_txid() -> Buf32 {
        Buf32([7u8; 32])
    }

    fn bridgeout_log(destination: Vec<u8>) -> Log {
        let event = WithdrawalIntentEvent {
            amount: 100,
            destination: Bytes::from(destination),
            selectedOperator: u32::MAX,
        };
        Log {
            address: BRIDGEOUT_PRECOMPILE_ADDRESS,
            data: LogData::from(&event),
        }
    }

    #[test]
    fn test_extract_withdrawal_intent_from_valid_bridgeout_log() {
        let mut bosd = vec![0u8; DEFAULT_MAX_WITHDRAWAL_DESCRIPTOR_LEN as usize];
        bosd[0] = 0x00;
        let log = bridgeout_log(bosd);

        let intent = extract_withdrawal_intent_from_log(test_txid(), &log, &test_bridge_params())
            .unwrap()
            .expect("bridgeout log should produce intent");

        assert_eq!(intent.amt, 100);
        assert_eq!(intent.selected_operator, OperatorSelection::any());
    }

    #[test]
    fn test_extract_withdrawal_intent_ignores_other_addresses() {
        let mut log = bridgeout_log(vec![0x03; 21]);
        log.address = Address::ZERO;

        assert!(
            extract_withdrawal_intent_from_log(test_txid(), &log, &test_bridge_params())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_extract_withdrawal_intent_fails_on_bad_event_log() {
        let log = Log {
            address: BRIDGEOUT_PRECOMPILE_ADDRESS,
            data: LogData::empty(),
        };

        let err = extract_withdrawal_intent_from_log(test_txid(), &log, &test_bridge_params())
            .unwrap_err();

        assert!(matches!(
            err,
            WithdrawalIntentExtractionError::EventDecode { .. }
        ));
    }

    #[test]
    fn test_extract_withdrawal_intent_fails_on_oversized_descriptor() {
        let mut bosd = vec![0u8; DEFAULT_MAX_WITHDRAWAL_DESCRIPTOR_LEN as usize + 1];
        bosd[0] = 0x00;
        let log = bridgeout_log(bosd);

        let err = extract_withdrawal_intent_from_log(test_txid(), &log, &test_bridge_params())
            .unwrap_err();

        assert!(matches!(
            err,
            WithdrawalIntentExtractionError::InvalidDescriptorLength { .. }
        ));
    }

    #[test]
    fn test_extract_withdrawal_intent_fails_on_malformed_descriptor() {
        let log = bridgeout_log(vec![0x03, 0x01, 0x02, 0x03]);

        let err = extract_withdrawal_intent_from_log(test_txid(), &log, &test_bridge_params())
            .unwrap_err();

        assert!(matches!(
            err,
            WithdrawalIntentExtractionError::DescriptorDecode { .. }
        ));
    }
}
