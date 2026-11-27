//! Alpen gas settlement and DA sizing for the evm2 transaction handler.

use evm2::{
    ethereum::{eip1559, eip2930, eip7702, finalize_gas, legacy, TxEnvelope},
    evm::{
        config::BaseEvmConfigSelector,
        handler::{GasSettlement, TxHandlerHooks},
        State,
    },
    registry::{handler, HandlerResult, TxRegistry},
    Evm, EvmFeatures, EvmTypesHost, SpecId, TxResult,
};
use revm_primitives::{Address, U256};
use strata_bridge_params::BridgeParams;

use crate::da_fee::{
    account_diff_size, bounded_da_fee, storage_diff_size, DA_COVERAGE_CAPPED, DA_COVERAGE_OK,
};

/// Runtime types shared by all Alpen EVM instances.
#[derive(Debug)]
pub struct AlpenEvmTypes;

impl EvmTypesHost for AlpenEvmTypes {
    type ConfigSelector = BaseEvmConfigSelector;
    type SpecId = SpecId;
    type Tx = TxEnvelope;
    type EvmExt = Option<BridgeParams>;
    type MessageExt = ();
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = u64;
    /// Rate in wei per byte, from the block stamp in the block execution context.
    type BlockEnvExt = u64;
    type Host<'a> = Evm<'a, Self>;
}

/// Registers the Ethereum transaction types Alpen accepts, with its own settlement hook.
/// Blob transactions are intentionally not registered, including on post-Cancun forks.
pub(crate) fn tx_registry(spec: SpecId) -> TxRegistry<AlpenEvmTypes, TxResult<AlpenEvmTypes>> {
    type T = AlpenEvmTypes;
    type H = AlpenSettlement;
    let mut registry = TxRegistry::new().with_handler(
        0,
        TxEnvelope::as_legacy,
        handler(
            legacy::prepare_with_hooks::<T, H>,
            legacy::execute_prepared::<T, H>,
        ),
    );
    if spec.enables(SpecId::BERLIN) {
        registry.register(
            1,
            TxEnvelope::as_eip2930,
            handler(
                eip2930::prepare_with_hooks::<T, H>,
                eip2930::execute_prepared::<T, H>,
            ),
        );
    }
    if spec.enables(SpecId::LONDON) {
        registry.register(
            2,
            TxEnvelope::as_eip1559,
            handler(
                eip1559::prepare_with_hooks::<T, H>,
                eip1559::execute_prepared::<T, H>,
            ),
        );
    }
    if spec.enables(SpecId::PRAGUE) {
        registry.register(
            4,
            TxEnvelope::as_eip7702,
            handler(
                eip7702::prepare_with_hooks::<T, H>,
                eip7702::execute_prepared::<T, H>,
            ),
        );
    }
    registry
}

struct AlpenSettlement;

impl TxHandlerHooks<AlpenEvmTypes> for AlpenSettlement {
    fn settle_transaction(
        host: &mut Evm<'_, AlpenEvmTypes>,
        _envelope: &TxEnvelope,
        gas: GasSettlement<AlpenEvmTypes>,
    ) -> HandlerResult<TxResult<AlpenEvmTypes>> {
        let caller = gas.caller;
        let price = gas.gas_price;
        let gas_limit = gas.gas_limit;
        let mut result = finalize_gas(host, gas)?;

        if !host.feature(EvmFeatures::FEE_CHARGE) {
            return Ok(result);
        }

        let used = result.tx_gas_used();
        let remaining_value = U256::from(gas_limit.saturating_sub(used)) * price;
        let beneficiary = host.block().beneficiary;
        let rate = U256::from(host.block().ext);

        // Load and touch the caller and fee recipient before sizing, as in the old
        // reimbursement/reward hooks. Unlike Ethereum, Alpen credits the base fee too.
        // REVM's reimbursement multiplies in u128, while the DA headroom and
        // beneficiary reward are computed in U256. Keep those bounds distinct.
        let refund = U256::from(
            price
                .saturating_to::<u128>()
                .saturating_mul(u128::from(gas_limit.saturating_sub(used))),
        );
        adjust_balance(host.state_mut(), &caller, |balance| {
            balance.checked_add(refund)
        })?;
        let reward = U256::from(used) * price;
        adjust_balance(host.state_mut(), &beneficiary, |balance| {
            balance.checked_add(reward)
        })?;

        if !rate.is_zero() && !price.is_zero() {
            let diff_size = calc_overlay_diff_size(host.state_mut())?;
            let fee = bounded_da_fee(rate, diff_size, remaining_value);
            result.ext = if rate.saturating_mul(U256::from(diff_size)) > remaining_value {
                DA_COVERAGE_CAPPED
            } else {
                DA_COVERAGE_OK
            };
            if !fee.is_zero() {
                adjust_balance(host.state_mut(), &caller, |balance| {
                    balance.checked_sub(fee)
                })?;
                adjust_balance(host.state_mut(), &beneficiary, |balance| {
                    balance.checked_add(fee)
                })?;
            }
        }
        Ok(result)
    }
}

// The legacy journal marks the account touched even when arithmetic would
// overflow or underflow; in that case it does not change its balance.
fn adjust_balance(
    state: &mut State<'_>,
    address: &Address,
    update: impl FnOnce(U256) -> Option<U256>,
) -> evm2::evm::DbResult<()> {
    let mut account = state.account(address)?;
    if let Some(balance) = update(account.balance()) {
        account.set_balance(balance);
    } else {
        account.touch();
    }
    Ok(())
}

/// Sizes the transaction-local overlay before finalization or the DA transfer.
pub(crate) fn calc_overlay_diff_size(state: &mut State<'_>) -> evm2::evm::DbResult<u64> {
    let loaded: Vec<_> = state
        .loaded_accounts()
        .map(|(address, slots)| (*address, slots.copied().collect::<Vec<_>>()))
        .collect();
    let mut size = 0u64;
    for (address, keys) in loaded {
        let mut account = state.account(&address)?;
        if !account.is_touched() {
            continue;
        }
        let code = account.get().and_then(|info| info.code.as_ref());
        let charge_code = code.is_some_and(|code| code.is_eip7702())
            || (account.is_created() && !account.is_empty_code_hash());
        let code_len =
            charge_code.then(|| code.map_or(0, |c| c.original_byte_slice().len() as u64));
        let mut changed_slots = 0;
        for key in keys {
            let slot = account.storage().into_slot(key)?;
            changed_slots += u64::from(slot.current() != slot.original());
        }
        size = size
            .saturating_add(account_diff_size(code_len))
            .saturating_add(storage_diff_size(changed_slots));
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{transaction::Recovered, TxEip1559, TxLegacy};
    use evm2::{env::BlockEnv, evm::InMemoryDB, AccountInfo};
    use revm_primitives::TxKind;

    use super::*;

    const SENDER: Address = Address::with_last_byte(0x77);
    const RECIPIENT: Address = Address::with_last_byte(0x78);
    const COINBASE: Address = Address::with_last_byte(0x79);
    const INITIAL_BALANCE: u64 = 10_000_000;
    const GAS_LIMIT: u64 = 50_000;
    const BASE_FEE: u64 = 10;
    const GAS_PRICE: u64 = 13;

    fn evm(rate: u64) -> Evm<'static, AlpenEvmTypes> {
        let spec = SpecId::PRAGUE;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            &SENDER,
            AccountInfo::empty().with_balance(U256::from(INITIAL_BALANCE)),
        );
        let block = BlockEnv::<AlpenEvmTypes> {
            beneficiary: COINBASE,
            gas_limit: U256::from(30_000_000),
            basefee: U256::from(BASE_FEE),
            ext: rate,
            ..Default::default()
        };
        Evm::new(
            spec,
            block,
            tx_registry(spec),
            db,
            crate::precompiles::create_precompiles(spec),
        )
    }

    fn tx(dynamic: bool) -> Recovered<TxEnvelope> {
        let to = TxKind::Call(RECIPIENT);
        let value = U256::ONE;
        let tx = if dynamic {
            TxEnvelope::Eip1559(TxEip1559 {
                chain_id: 1,
                gas_limit: GAS_LIMIT,
                max_fee_per_gas: 20,
                max_priority_fee_per_gas: 3,
                to,
                value,
                ..Default::default()
            })
        } else {
            TxEnvelope::Legacy(TxLegacy {
                chain_id: Some(1),
                gas_price: GAS_PRICE as u128,
                gas_limit: GAS_LIMIT,
                to,
                value,
                ..Default::default()
            })
        };
        Recovered::new_unchecked(tx, SENDER)
    }

    #[test]
    fn legacy_and_dynamic_fee_reward_full_price_and_charge_da() {
        for dynamic in [false, true] {
            const RATE: u64 = 5;
            let mut evm = evm(RATE);
            let result = evm.transact(&tx(dynamic)).unwrap().commit();
            assert!(result.status);
            assert_eq!(result.tx_gas_used(), 21_000);
            assert_eq!(result.ext, DA_COVERAGE_OK);
            // Sender, recipient and fee recipient are touched; a simple transfer writes no
            // code/slots.
            let fee = 21_000 * GAS_PRICE + 3 * account_diff_size(None) * RATE;
            let state = evm.state_mut();
            assert_eq!(
                state.account(&SENDER).unwrap().balance(),
                U256::from(INITIAL_BALANCE - fee - 1)
            );
            assert_eq!(state.account(&RECIPIENT).unwrap().balance(), U256::ONE);
            assert_eq!(state.account(&COINBASE).unwrap().balance(), U256::from(fee));
        }
    }

    #[test]
    fn capped_tx_can_be_discarded_then_replayed_with_capped_charge() {
        let mut evm = evm(100_000);
        let tx = tx(false);
        let simulated = evm.transact(&tx).unwrap().discard();
        assert_eq!(simulated.ext, DA_COVERAGE_CAPPED);
        assert_eq!(evm.state_mut().account(&SENDER).unwrap().nonce(), 0);
        assert_eq!(
            evm.state_mut().account(&COINBASE).unwrap().balance(),
            U256::ZERO
        );

        let committed = evm.transact(&tx).unwrap().commit();
        assert_eq!(committed.ext, DA_COVERAGE_CAPPED);
        assert_eq!(
            evm.state_mut().account(&COINBASE).unwrap().balance(),
            U256::from(GAS_LIMIT * GAS_PRICE)
        );
        assert_eq!(evm.state_mut().account(&SENDER).unwrap().nonce(), 1);
    }
}
