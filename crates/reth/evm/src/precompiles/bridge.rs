use alpen_primitives::{
    wei_to_sats, WithdrawalCalldata, WithdrawalIntentEvent, BRIDGEOUT_PRECOMPILE_ADDRESS,
};
use evm2::{
    interpreter::{GasTracker, Message},
    precompile::PrecompileOutput,
    precompiles::PrecompileResult,
    Evm, PrecompileError,
};
use revm_primitives::{Bytes, Log, LogData, U256};
use strata_bridge_params::BridgeParams;
use strata_primitives::bitcoin_bosd::Descriptor;

use crate::evm2_settlement::AlpenEvmTypes;

// REVIEW(STR-3676): Replace these draft values with protocol-approved launch constants.
/// Fixed raw EVM gas charged for bridge-out precompile execution.
const BRIDGEOUT_BASE_GAS: u64 = 10_000;

/// Raw EVM gas charged per calldata byte handled by the bridge-out precompile.
const BRIDGEOUT_CALLDATA_BYTE_GAS: u64 = 16;

/// Machine-readable failure reasons returned by the bridge-out precompile.
///
/// Each variant is encoded as a Solidity ABI custom error: the 4-byte selector
/// `bytes4(keccak256(signature))` followed by ABI-encoded parameters. The canonical
/// definitions consumers decode against live in `IBridgeOut.sol` (next to this file).
/// The selector bytes below are asserted against `keccak256` of the signatures in
/// `test_custom_error_selectors_match_signatures`, so they cannot silently drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeOutError {
    /// `IncorrectCallType()` — precompile was not reached via a direct CALL.
    IncorrectCallType,
    /// `MalformedCalldata()` — calldata too short to hold operator selector + BOSD.
    MalformedCalldata,
    /// `MalformedCalldataBosd()` — BOSD bytes are not a valid descriptor.
    MalformedCalldataBosd,
    /// `OversizeBosd(uint256 max)` — BOSD descriptor length exceeds `max` bytes.
    OversizeBosd { max: U256 },
    /// `NonIntegerAmount()` — value is not a whole number of satoshis.
    NonIntegerAmount,
    /// `IncorrectAmount(uint256 denomination)` — value is zero or not a multiple of
    /// `denomination` sats.
    IncorrectAmount { denomination: U256 },
    /// `OversizeWithdrawal(uint256 max)` — value exceeds the `max` withdrawal (sats).
    OversizeWithdrawal { max: U256 },
}

impl BridgeOutError {
    /// The Solidity custom-error selector, `bytes4(keccak256(signature))`.
    const fn selector(self) -> [u8; 4] {
        match self {
            Self::IncorrectCallType => [0x7a, 0x5e, 0x63, 0xdc],
            Self::MalformedCalldata => [0x59, 0x17, 0x0b, 0xf0],
            Self::MalformedCalldataBosd => [0xc8, 0xe4, 0x58, 0x92],
            Self::OversizeBosd { .. } => [0x27, 0x25, 0xac, 0x73],
            Self::NonIntegerAmount => [0xf7, 0x73, 0x8c, 0x57],
            Self::IncorrectAmount { .. } => [0x88, 0x96, 0x7d, 0x2f],
            Self::OversizeWithdrawal { .. } => [0xb0, 0x70, 0x13, 0x77],
        }
    }

    /// The single `uint256` parameter, if the error carries one.
    const fn param(self) -> Option<U256> {
        match self {
            Self::OversizeBosd { max } => Some(max),
            Self::IncorrectAmount { denomination } => Some(denomination),
            Self::OversizeWithdrawal { max } => Some(max),
            Self::IncorrectCallType
            | Self::MalformedCalldata
            | Self::MalformedCalldataBosd
            | Self::NonIntegerAmount => None,
        }
    }

    /// ABI-encodes the error as `selector ++ abi.encode(params)`.
    fn abi_encode(self) -> Bytes {
        let mut out = Vec::with_capacity(4 + 32);
        out.extend_from_slice(&self.selector());
        if let Some(value) = self.param() {
            // A `uint256` parameter is its 32-byte big-endian representation.
            out.extend_from_slice(&value.to_be_bytes::<32>());
        }
        Bytes::from(out)
    }
}

fn revert_with_error(error: BridgeOutError) -> PrecompileError {
    PrecompileError::Revert(error.abi_encode())
}

/// Custom precompile to burn rollup native token and add bridge out intent of equal amount.
/// Bridge out intent is created during block payload generation.
/// This precompile validates transaction and burns the bridge out amount.
///
/// Calldata format: `[4 bytes: selected_operator (big-endian u32)][BOSD bytes]`
/// - `u32::MAX` (`0xFFFFFFFF`): no operator selection
/// - Any other value: operator index
pub(crate) fn bridge_context_call(
    evm: &mut Evm<'_, AlpenEvmTypes>,
    message: &Message<AlpenEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    let data = evm.message_input(message);
    gas.spend(bridgeout_gas_cost(data.len())?)?;

    // The previous runtime compared execution and bytecode addresses. Keep that
    // guard and its error ordering, including zero-value STATICCALL.
    if message.destination != message.code_address {
        return Err(revert_with_error(BridgeOutError::IncorrectCallType));
    }

    let bridge_params = evm
        .ext()
        .ok_or_else(|| PrecompileError::from("bridge params not configured"))?;
    let calldata = WithdrawalCalldata::decode(data)
        .ok_or_else(|| revert_with_error(BridgeOutError::MalformedCalldata))?;
    validate_bosd(&calldata.bosd, &bridge_params).map_err(revert_with_error)?;
    let amount =
        validate_withdrawal_amount(message.value, &bridge_params).map_err(revert_with_error)?;

    let evt = WithdrawalIntentEvent {
        amount,
        destination: Bytes::from(calldata.bosd),
        selectedOperator: calldata.selected_operator.raw(),
    };
    evm.log(Log {
        address: BRIDGEOUT_PRECOMPILE_ADDRESS,
        data: LogData::from(&evt),
    });
    evm.state_mut()
        .account(&BRIDGEOUT_PRECOMPILE_ADDRESS)?
        .set_balance(U256::ZERO);
    Ok(PrecompileOutput::default())
}

fn bridgeout_gas_cost(calldata_len: usize) -> Result<u64, PrecompileError> {
    let calldata_len = u64::try_from(calldata_len)
        .map_err(|_| PrecompileError::from("Bridgeout calldata length exceeds u64"))?;

    BRIDGEOUT_CALLDATA_BYTE_GAS
        .checked_mul(calldata_len)
        .and_then(|calldata_gas| BRIDGEOUT_BASE_GAS.checked_add(calldata_gas))
        .ok_or_else(|| PrecompileError::from("Bridgeout gas cost overflow"))
}

/// Validates that the withdrawal amount is a positive exact multiple of the denomination
/// and within the cap, returning the amount in satoshis.
fn validate_withdrawal_amount(
    amount_wei: U256,
    bridge_params: &BridgeParams,
) -> Result<u64, BridgeOutError> {
    let (amount_sats, remainder_wei) = wei_to_sats(amount_wei);
    if !remainder_wei.is_zero() {
        return Err(BridgeOutError::NonIntegerAmount);
    }

    // A value that overflows u64 satoshis cannot be within any cap.
    let amount_sats: u64 =
        amount_sats
            .try_into()
            .map_err(|_| BridgeOutError::OversizeWithdrawal {
                max: U256::from(bridge_params.max_withdrawal_amount().unwrap_or(u64::MAX)),
            })?;

    // `BridgeParams` is the source of truth for validity; when it rejects the amount we
    // attribute the specific reason so the caller gets a precise, typed error.
    if !bridge_params.validate_withdrawal_amount(amount_sats) {
        let denomination = bridge_params.denomination();
        if amount_sats == 0 || !amount_sats.is_multiple_of(denomination) {
            return Err(BridgeOutError::IncorrectAmount {
                denomination: U256::from(denomination),
            });
        }
        return Err(BridgeOutError::OversizeWithdrawal {
            max: U256::from(bridge_params.max_withdrawal_amount().unwrap_or(u64::MAX)),
        });
    }

    Ok(amount_sats)
}

/// Validates that input is a valid BOSD [`Descriptor`] within the configured limit.
fn validate_bosd(data: &[u8], bridge_params: &BridgeParams) -> Result<(), BridgeOutError> {
    if !bridge_params.validate_withdrawal_descriptor_len(data.len()) {
        return Err(BridgeOutError::OversizeBosd {
            max: U256::from(bridge_params.max_withdrawal_descriptor_len()),
        });
    }

    Descriptor::from_bytes(data).map_err(|_| BridgeOutError::MalformedCalldataBosd)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use alpen_primitives::WEI_PER_BTC;
    use evm2::{env::BlockEnv, evm::EmptyDB, interpreter::MessageKind, PrecompileHalt};
    use revm_primitives::Address;
    use strata_ol_bridge_types::OperatorSelection;

    use super::*;

    const fn u256_from(val: u128) -> U256 {
        U256::from_limbs([(val & ((1 << 64) - 1)) as u64, (val >> 64) as u64, 0, 0])
    }

    /// Test-only denomination constant (1 BTC in wei).
    const FIXED_WITHDRAWAL_WEI: U256 = u256_from(WEI_PER_BTC);

    /// Valid P2WPKH descriptor: type tag (0x03) + 20-byte hash160.
    const VALID_P2WPKH_BOSD: &[u8; 21] = &{
        let mut buf = [0x14u8; 21];
        buf[0] = 0x03; // P2WPKH type tag
        buf
    };
    const MAX_DESCRIPTOR_LEN: u32 = 81;

    /// Returns the 4-byte selector at the head of ABI-encoded revert data.
    fn selector_of(data: &[u8]) -> [u8; 4] {
        data[0..4].try_into().unwrap()
    }

    /// Decodes the single trailing `uint256` parameter (low 64 bits).
    fn uint_param(data: &[u8]) -> u64 {
        // selector[0..4] | uint256 word[4..36]; low 8 bytes at [28..36].
        u64::from_be_bytes(data[28..36].try_into().unwrap())
    }

    #[test]
    fn test_custom_error_selectors_match_signatures() {
        use revm_primitives::keccak256;

        let cases: [(BridgeOutError, &str); 7] = [
            (BridgeOutError::IncorrectCallType, "IncorrectCallType()"),
            (BridgeOutError::MalformedCalldata, "MalformedCalldata()"),
            (
                BridgeOutError::MalformedCalldataBosd,
                "MalformedCalldataBosd()",
            ),
            (
                BridgeOutError::OversizeBosd { max: U256::ZERO },
                "OversizeBosd(uint256)",
            ),
            (BridgeOutError::NonIntegerAmount, "NonIntegerAmount()"),
            (
                BridgeOutError::IncorrectAmount {
                    denomination: U256::ZERO,
                },
                "IncorrectAmount(uint256)",
            ),
            (
                BridgeOutError::OversizeWithdrawal { max: U256::ZERO },
                "OversizeWithdrawal(uint256)",
            ),
        ];

        for (err, sig) in cases {
            assert_eq!(
                err.selector(),
                keccak256(sig.as_bytes())[..4],
                "selector drift for {sig}",
            );
        }
    }

    #[test]
    fn test_custom_error_abi_encoding_layout() {
        // No-param error: selector only.
        let encoded = BridgeOutError::IncorrectCallType.abi_encode();
        assert_eq!(encoded.len(), 4);
        assert_eq!(
            selector_of(&encoded),
            BridgeOutError::IncorrectCallType.selector()
        );

        // Parametrized error: selector + one uint256 word.
        let encoded = BridgeOutError::IncorrectAmount {
            denomination: U256::from(100_000_000u64),
        }
        .abi_encode();
        assert_eq!(encoded.len(), 36);
        assert_eq!(
            selector_of(&encoded),
            BridgeOutError::IncorrectAmount {
                denomination: U256::ZERO
            }
            .selector()
        );
        assert_eq!(uint_param(&encoded), 100_000_000);
    }

    #[test]
    fn test_decode_calldata_empty() {
        assert!(WithdrawalCalldata::decode(&[]).is_none());
    }

    #[test]
    fn test_decode_calldata_no_preference() {
        let mut data = Vec::new();
        data.extend_from_slice(&u32::MAX.to_be_bytes());
        data.extend_from_slice(VALID_P2WPKH_BOSD);

        let calldata = WithdrawalCalldata::decode(&data).unwrap();
        assert_eq!(calldata.selected_operator, OperatorSelection::any());
        assert_eq!(calldata.bosd, VALID_P2WPKH_BOSD);
    }

    #[test]
    fn test_decode_calldata_operator_42() {
        let mut data = Vec::new();
        data.extend_from_slice(&42u32.to_be_bytes());
        data.extend_from_slice(VALID_P2WPKH_BOSD);

        let calldata = WithdrawalCalldata::decode(&data).unwrap();
        assert_eq!(calldata.selected_operator, OperatorSelection::specific(42));
        assert_eq!(calldata.bosd, VALID_P2WPKH_BOSD);
    }

    #[test]
    fn test_decode_calldata_operator_large() {
        let idx: u32 = 0x01020304;
        let mut data = Vec::new();
        data.extend_from_slice(&idx.to_be_bytes());
        data.extend_from_slice(VALID_P2WPKH_BOSD);

        let calldata = WithdrawalCalldata::decode(&data).unwrap();
        assert_eq!(calldata.selected_operator, OperatorSelection::specific(idx));
        assert_eq!(calldata.bosd, VALID_P2WPKH_BOSD);
    }

    #[test]
    fn test_decode_calldata_operator_zero() {
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(VALID_P2WPKH_BOSD);

        let calldata = WithdrawalCalldata::decode(&data).unwrap();
        assert_eq!(calldata.selected_operator, OperatorSelection::specific(0));
        assert_eq!(calldata.bosd, VALID_P2WPKH_BOSD);
    }

    #[test]
    fn test_decode_calldata_too_short() {
        // Only 3 bytes — less than the minimum 5 (4 operator + 1 BOSD)
        let data = vec![0x00, 0x01, 0x02];
        assert!(WithdrawalCalldata::decode(&data).is_none());
    }

    #[test]
    fn test_decode_calldata_only_operator_no_bosd() {
        // Exactly 4 bytes (operator only, no BOSD)
        let data = vec![0x00, 0x00, 0x00, 0x05];
        assert!(WithdrawalCalldata::decode(&data).is_none());
    }

    #[test]
    fn test_bridgeout_gas_cost_includes_base_and_calldata_bytes() {
        let calldata_len = 4 + VALID_P2WPKH_BOSD.len();

        assert_eq!(
            bridgeout_gas_cost(calldata_len).unwrap(),
            BRIDGEOUT_BASE_GAS + BRIDGEOUT_CALLDATA_BYTE_GAS * calldata_len as u64
        );
    }

    #[test]
    fn test_bridgeout_gas_cost_scales_with_calldata_len() {
        let short = bridgeout_gas_cost(5).unwrap();
        let long = bridgeout_gas_cost(6).unwrap();

        assert_eq!(long - short, BRIDGEOUT_CALLDATA_BYTE_GAS);
    }

    #[test]
    fn test_bridgeout_gas_cost_rejects_overflow() {
        assert!(bridgeout_gas_cost(usize::MAX).is_err());
    }

    // --- withdrawal amount validation tests ---

    fn bridge_params() -> BridgeParams {
        BridgeParams::new_with_descriptor_limit(100_000_000, Some(1_000_000_000), 81).unwrap()
    }

    fn bridge_params_without_cap() -> BridgeParams {
        BridgeParams::new_with_descriptor_limit(100_000_000, None, 81).unwrap()
    }

    fn valid_bridgeout_calldata() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&u32::MAX.to_be_bytes());
        data.extend_from_slice(VALID_P2WPKH_BOSD);
        data
    }

    fn call_bridge(
        kind: MessageKind,
        is_static: bool,
        value: U256,
        gas_limit: u64,
    ) -> (u64, PrecompileResult, Vec<Log>, U256) {
        use crate::{evm2_settlement::tx_registry, precompiles::create_precompiles};
        let spec = evm2::SpecId::PRAGUE;
        let mut evm = Evm::new(
            spec,
            BlockEnv::<AlpenEvmTypes>::default(),
            tx_registry(spec),
            EmptyDB::default(),
            create_precompiles(spec),
        );
        *evm.ext_mut() = Some(bridge_params());
        let mut account = evm
            .state_mut()
            .account(&BRIDGEOUT_PRECOMPILE_ADDRESS)
            .unwrap();
        account.set_balance(value);
        drop(account);
        let message = Message::<AlpenEvmTypes> {
            kind,
            caller_is_static: is_static,
            caller: Address::with_last_byte(42),
            destination: if matches!(kind, MessageKind::DelegateCall | MessageKind::CallCode) {
                Address::with_last_byte(55)
            } else {
                BRIDGEOUT_PRECOMPILE_ADDRESS
            },
            call_target: BRIDGEOUT_PRECOMPILE_ADDRESS,
            code_address: BRIDGEOUT_PRECOMPILE_ADDRESS,
            value,
            input: Bytes::from(valid_bridgeout_calldata()).into(),
            ..Default::default()
        };
        let mut gas = GasTracker::new(gas_limit);
        let output = bridge_context_call(&mut evm, &message, &mut gas);
        let logs = evm.state().logs().to_vec();
        let balance = evm
            .state_mut()
            .account(&BRIDGEOUT_PRECOMPILE_ADDRESS)
            .unwrap()
            .balance();
        (
            gas_limit.saturating_sub(gas.remaining()),
            output,
            logs,
            balance,
        )
    }

    #[test]
    fn test_bridgeout_rejects_non_direct_call() {
        for (kind, is_static) in [
            (MessageKind::DelegateCall, false),
            (MessageKind::CallCode, false),
        ] {
            let (_, output, logs, balance) =
                call_bridge(kind, is_static, FIXED_WITHDRAWAL_WEI, 100_000);
            let Err(PrecompileError::Revert(bytes)) = output else {
                panic!("expected typed revert")
            };
            assert_eq!(
                selector_of(&bytes),
                BridgeOutError::IncorrectCallType.selector()
            );
            assert!(logs.is_empty());
            assert_eq!(balance, FIXED_WITHDRAWAL_WEI);
        }
    }

    #[test]
    fn test_staticcall_preserves_the_zero_value_revert() {
        let (_, output, logs, balance) =
            call_bridge(MessageKind::StaticCall, false, U256::ZERO, 100_000);
        let Err(PrecompileError::Revert(bytes)) = output else {
            panic!("expected typed revert")
        };
        assert_eq!(
            selector_of(&bytes),
            BridgeOutError::IncorrectAmount {
                denomination: U256::ZERO
            }
            .selector()
        );
        assert!(logs.is_empty());
        assert_eq!(balance, U256::ZERO);
    }

    #[test]
    fn test_bridgeout_insufficient_gas_halts() {
        let needed = bridgeout_gas_cost(valid_bridgeout_calldata().len()).unwrap();
        let (_, output, logs, _) =
            call_bridge(MessageKind::Call, false, FIXED_WITHDRAWAL_WEI, needed - 1);
        assert!(matches!(
            output,
            Err(PrecompileError::Halt(PrecompileHalt::OutOfGas))
        ));
        assert!(logs.is_empty());
    }

    #[test]
    fn test_bridgeout_accepts_direct_call_value() {
        let (gas, output, logs, balance) =
            call_bridge(MessageKind::Call, false, FIXED_WITHDRAWAL_WEI, 100_000);
        assert!(output.is_ok());
        assert_eq!(
            gas,
            bridgeout_gas_cost(valid_bridgeout_calldata().len()).unwrap()
        );
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, BRIDGEOUT_PRECOMPILE_ADDRESS);
        assert_eq!(balance, U256::ZERO);
    }

    #[test]
    fn test_bridgeout_over_cap_reverts_with_typed_error() {
        let value = FIXED_WITHDRAWAL_WEI * U256::from(11);
        let (gas, output, logs, balance) = call_bridge(MessageKind::Call, false, value, 100_000);
        let Err(PrecompileError::Revert(bytes)) = output else {
            panic!("expected typed revert")
        };
        assert_eq!(
            gas,
            bridgeout_gas_cost(valid_bridgeout_calldata().len()).unwrap()
        );
        assert_eq!(
            selector_of(&bytes),
            BridgeOutError::OversizeWithdrawal { max: U256::ZERO }.selector()
        );
        assert_eq!(uint_param(&bytes), 1_000_000_000);
        assert!(logs.is_empty());
        assert_eq!(balance, value);
    }

    #[test]
    fn test_validate_withdrawal_exact_denomination() {
        assert_eq!(
            validate_withdrawal_amount(FIXED_WITHDRAWAL_WEI, &bridge_params()).unwrap(),
            100_000_000
        );
    }

    #[test]
    fn test_validate_withdrawal_exact_multiple() {
        assert_eq!(
            validate_withdrawal_amount(FIXED_WITHDRAWAL_WEI * U256::from(3), &bridge_params())
                .unwrap(),
            300_000_000
        );
    }

    #[test]
    fn test_validate_withdrawal_zero_rejected() {
        assert_eq!(
            validate_withdrawal_amount(U256::ZERO, &bridge_params()).unwrap_err(),
            BridgeOutError::IncorrectAmount {
                denomination: U256::from(100_000_000u64)
            }
        );
    }

    #[test]
    fn test_validate_withdrawal_non_multiple_rejected() {
        assert_eq!(
            validate_withdrawal_amount(FIXED_WITHDRAWAL_WEI + U256::from(1), &bridge_params())
                .unwrap_err(),
            BridgeOutError::NonIntegerAmount
        );
    }

    #[test]
    fn test_validate_withdrawal_sub_denomination_multiple_rejected() {
        // 1.5 BTC: a whole number of sats, but not a multiple of the 1 BTC denomination.
        assert_eq!(
            validate_withdrawal_amount(
                FIXED_WITHDRAWAL_WEI + FIXED_WITHDRAWAL_WEI / U256::from(2),
                &bridge_params()
            )
            .unwrap_err(),
            BridgeOutError::IncorrectAmount {
                denomination: U256::from(100_000_000u64)
            }
        );
    }

    #[test]
    fn test_validate_withdrawal_exceeds_cap() {
        assert_eq!(
            validate_withdrawal_amount(FIXED_WITHDRAWAL_WEI * U256::from(11), &bridge_params())
                .unwrap_err(),
            BridgeOutError::OversizeWithdrawal {
                max: U256::from(1_000_000_000u64)
            }
        );
    }

    #[test]
    fn test_validate_withdrawal_at_cap() {
        assert_eq!(
            validate_withdrawal_amount(FIXED_WITHDRAWAL_WEI * U256::from(10), &bridge_params())
                .unwrap(),
            1_000_000_000
        );
    }

    #[test]
    fn test_validate_withdrawal_no_cap() {
        assert_eq!(
            validate_withdrawal_amount(
                FIXED_WITHDRAWAL_WEI * U256::from(100),
                &bridge_params_without_cap()
            )
            .unwrap(),
            10_000_000_000
        );
    }

    #[test]
    fn test_validate_bosd_accepts_descriptor_at_limit() {
        let mut bosd = vec![0u8; MAX_DESCRIPTOR_LEN as usize];
        bosd[0] = 0x00;

        assert!(validate_bosd(&bosd, &bridge_params()).is_ok());
    }

    #[test]
    fn test_validate_bosd_rejects_oversized_descriptor() {
        let mut bosd = vec![0u8; MAX_DESCRIPTOR_LEN as usize + 1];
        bosd[0] = 0x00;

        assert_eq!(
            validate_bosd(&bosd, &bridge_params()).unwrap_err(),
            BridgeOutError::OversizeBosd {
                max: U256::from(MAX_DESCRIPTOR_LEN)
            }
        );
    }

    #[test]
    fn test_validate_bosd_rejects_malformed_descriptor() {
        let bosd = [0x03, 0x01, 0x02, 0x03];

        assert_eq!(
            validate_bosd(&bosd, &bridge_params()).unwrap_err(),
            BridgeOutError::MalformedCalldataBosd
        );
    }
}
