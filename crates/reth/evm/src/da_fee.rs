//! Per-transaction data-availability (DA) fee sizing.
//!
//! The DA fee charges a transaction for the Bitcoin data-availability cost of its
//! state diff: `da_fee = da_rate_wei_per_byte * diff_size`. This module owns the
//! single, deterministic routine that computes `diff_size` for one transaction from
//! the EVM state change-set.
//!
//! Determinism is the core requirement: the sequencer, a re-executing full node, and
//! the chunk proof must all compute the identical `diff_size` (and therefore the
//! identical post-charge balances / state root). [`calc_diff_size`] is a pure,
//! order-independent integer sum over the change-set using only compile-time constants,
//! so it reproduces exactly across all three.
//!
//! This routine is intended to be the single source of truth for DA sizing — the
//! in-EVM charge and the fee-estimation RPCs must both call it, so a quote can never
//! disagree with the charge.
//!
//! TODO(STR-4226): Refactor and move it to a separate crate outside of reth so that
//! all the components (rpc, execution, guest) can access it without necessarily
//! depending on heavy reth crates.

use reth_evm::{eth::EthEvmContext, Database};
use revm::state::EvmState;
use revm_primitives::{Bytes, KECCAK_EMPTY, U256};

use crate::utils::WEI_PER_SAT;

/// Byte cost attributed to the key (address) of a changed account.
const ACCOUNT_KEY_BYTES: u64 = 20;

/// Byte cost attributed to the account-info change of a changed EOA.
///
/// Alpen's DA encoding stores account info as trimmed deltas (balance delta, nonce
/// varint, unset code-hash), so an EOA change is small. Conservative typical estimate.
const ACCOUNT_INFO_EOA_BYTES: u64 = 12;

/// Byte cost attributed to the account-info change of a changed contract account.
///
/// Adds the 33-byte code-hash register that a contract carries over an EOA. Deployed
/// bytecode itself is deduplicated by hash at the batch level and is not attributed
/// per transaction here (future: attribute deploy bytecode once per unique deployment).
const ACCOUNT_INFO_CONTRACT_BYTES: u64 = 44;

/// Byte cost attributed to the key of a changed storage slot (untrimmed 32-byte hash).
const SLOT_KEY_BYTES: u64 = 32;

/// Byte cost attributed to the value of a changed storage slot (trimmed; typical).
const SLOT_VALUE_BYTES: u64 = 8;

/// DA-coverage report values written by the charge handler into the shared report cell
/// and read by the sequencer's payload builder for tx admission.
///
/// The default / reset value is [`DA_COVERAGE_UNKNOWN`] (`0`) — deliberately *not* `OK` —
/// so a cell that was never written for the current transaction (e.g. a zero-fee system
/// call that skips the charge, or a stale value from a previous tx) is never mistaken for
/// "covered". The payload builder skips a transaction only on an explicit
/// [`DA_COVERAGE_CAPPED`]; `UNKNOWN` and `OK` both mean "include".
pub const DA_COVERAGE_UNKNOWN: u64 = 0;
/// The transaction's DA fee was fully covered by its unused authorized gas.
pub const DA_COVERAGE_OK: u64 = 1;
/// The transaction's DA fee was capped by its unused authorized gas — under-covered, so
/// the protocol would subsidize it. The payload builder skips such transactions.
pub const DA_COVERAGE_CAPPED: u64 = 2;

/// Computes the DA `diff_size` (in bytes) for a single transaction's state change-set.
///
/// `state` is the post-execution [`EvmState`] for the transaction. Every touched
/// account and every changed storage slot contributes a fixed byte cost, summed
/// directly — no discount, compression, or fixed-overhead adjustment is applied.
///
/// The result is deterministic and independent of map iteration order.
pub fn calc_diff_size(state: &EvmState) -> u64 {
    let mut diff_size: u64 = 0;

    for account in state.values() {
        // Only accounts that were actually touched enter the state diff.
        if !account.is_touched() {
            continue;
        }

        let account_info_bytes = if account.info.code_hash != KECCAK_EMPTY {
            ACCOUNT_INFO_CONTRACT_BYTES
        } else {
            ACCOUNT_INFO_EOA_BYTES
        };
        diff_size = diff_size.saturating_add(ACCOUNT_KEY_BYTES + account_info_bytes);

        for slot in account.storage.values() {
            if slot.is_changed() {
                diff_size = diff_size.saturating_add(SLOT_KEY_BYTES + SLOT_VALUE_BYTES);
            }
        }
    }

    diff_size
}

/// Decodes the per-block DA rate (wei per byte) from the EVM header `extra_data`.
///
/// The rate is stored as a big-endian `u64` in the first 8 bytes of `extra_data`.
/// Anything shorter (e.g. the genesis label, or an empty field) decodes to `0`, which
/// disables the DA charge — so the charge is dormant until a rate is committed.
pub fn da_rate_from_extra_data(extra_data: &Bytes) -> u64 {
    if extra_data.len() < 8 {
        return 0;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&extra_data[..8]);
    u64::from_be_bytes(buf)
}

/// Encodes a per-block DA rate (wei per byte) into big-endian `extra_data` bytes.
pub fn da_rate_to_extra_data(da_rate: u64) -> Bytes {
    Bytes::copy_from_slice(&da_rate.to_be_bytes())
}

/// SegWit witness discount: DA payload rides in witness data, weighted at 1/4 of a vByte.
const SEGWIT_WITNESS_DIVISOR: u64 = 4;

/// Default Bitcoin fee rate (sat/vByte) the live DA rate is seeded from when no explicit
/// `ALPEN_DA_RATE_WEI_PER_BYTE` override is set.
///
/// 4 sat/vByte is a conservative normal-conditions rate; after the SegWit witness discount
/// it is exactly 1 satoshi per DA byte. It is only a seed — the live rate is expected to
/// track the sequencer's actual Bitcoin fee rate later (see [`btc_fee_rate_to_da_rate`]).
pub const DEFAULT_DA_BTC_FEE_RATE_SAT_PER_VBYTE: u64 = 4;

/// Default live DA rate (wei per DA byte): [`DEFAULT_DA_BTC_FEE_RATE_SAT_PER_VBYTE`] mapped
/// through the SegWit witness discount (`WEI_PER_SAT` wei, i.e. 1 sat per DA byte).
pub const DEFAULT_DA_RATE_WEI_PER_BYTE: u64 =
    DEFAULT_DA_BTC_FEE_RATE_SAT_PER_VBYTE * WEI_PER_SAT / SEGWIT_WITNESS_DIVISOR;

/// Converts a Bitcoin fee rate (satoshis per virtual byte) to the DA rate (wei per byte).
///
/// `da_rate = btc_fee_rate[sat/vB] * 10^10[wei/sat] / 4` (the SegWit witness discount).
///
/// NOTE: for now this reuses the sequencer's Bitcoin publication fee rate
/// (`btcio::writer::fees::resolve_fee_rate`). The DA fee-model rate is expected to be
/// decoupled from the publication rate — and smoothed/cached — in a later revision.
pub fn btc_fee_rate_to_da_rate(sat_per_vbyte: u64) -> u64 {
    sat_per_vbyte
        .saturating_mul(WEI_PER_SAT)
        .saturating_div(SEGWIT_WITNESS_DIVISOR)
}

/// Computes the DA fee to charge, bounded by the caller's unused authorized gas value.
///
/// The raw fee is `da_rate * diff_size`, capped at `remaining_value` — the value of the
/// gas the caller authorized (prepaid) but did not consume.
///
/// # Policy: cap, never fail
///
/// When the raw fee exceeds `remaining_value` (the committed DA rate rose, or the diff
/// came out larger than the estimate the wallet quoted its `effective_gas` against), this
/// **caps at `remaining_value` and the transaction still succeeds** — it never fails and
/// never charges beyond what the signature authorized. The bound also guarantees the debit
/// is always covered: the caller was just refunded `remaining_value`, so a charge `<=` it
/// cannot fail. The cost of this choice is that the **protocol undercharges (subsidizes)**
/// the shortfall in that case, rather than reverting the transaction.
///
/// This is acceptable for v1 because the quote-to-inclusion window is short (seconds) and
/// the committed rate moves per block, so the shortfall is rare and small. It is *not*
/// turned into an out-of-gas failure: DA is a byte-priced charge, not gas-metered, so a
/// clean OOG would require reverting a fully-executed transaction post-execution (and the
/// forfeited value would go to the coinbase regardless).
///
/// NOTE(fee-model): reduce the subsidy without failing — quote `effective_gas` at
/// `da_rate * (1 + margin)` in the fee RPC (safety margin), and/or add a per-block
/// rate-change bound so the committed rate cannot rise faster than the quoted headroom.
pub fn bounded_da_fee(da_rate: U256, diff_size: u64, remaining_value: U256) -> U256 {
    da_rate
        .saturating_mul(U256::from(diff_size))
        .min(remaining_value)
}

/// Read access to the raw EVM state change-set on the concrete EVM context.
///
/// The generic revm `Handler` cannot reach the whole `EvmState` through `JournalTr`, but
/// the concrete [`EthEvmContext`] exposes it via its journal. The DA charge binds to this
/// trait so it can size the diff in the handler. The fee itself is applied through the
/// journal (`load_account_mut`), not by mutating this map, so accounts stay loaded.
pub trait DaStateAccess {
    /// Returns the transaction's state change-set.
    fn evm_state(&self) -> &EvmState;
}

impl<DB: Database> DaStateAccess for EthEvmContext<DB> {
    fn evm_state(&self) -> &EvmState {
        &self.journaled_state.state
    }
}

#[cfg(test)]
mod tests {
    use revm::state::{Account, AccountInfo, EvmStorageSlot};
    use revm_primitives::{Address, B256, U256};

    use super::*;

    fn touched_eoa() -> Account {
        let mut account = Account::from(AccountInfo::default());
        account.mark_touch();
        account
    }

    fn touched_contract() -> Account {
        let info = AccountInfo {
            code_hash: B256::repeat_byte(0x11),
            ..AccountInfo::default()
        };
        let mut account = Account::from(info);
        account.mark_touch();
        account
    }

    fn state_of(accounts: impl IntoIterator<Item = (Address, Account)>) -> EvmState {
        let mut state = EvmState::default();
        for (addr, account) in accounts {
            state.insert(addr, account);
        }
        state
    }

    /// Expected `diff_size` for the given account/storage byte totals: the raw sum,
    /// with no discount, compression, or overhead applied.
    fn expected(account_raw: u64, storage_raw: u64) -> u64 {
        account_raw + storage_raw
    }

    #[test]
    fn empty_state_is_zero() {
        assert_eq!(calc_diff_size(&EvmState::default()), 0);
    }

    #[test]
    fn untouched_accounts_are_ignored() {
        // An account present but not touched must not contribute.
        let state = state_of([(
            Address::repeat_byte(1),
            Account::from(AccountInfo::default()),
        )]);
        assert_eq!(calc_diff_size(&state), 0);
    }

    #[test]
    fn single_eoa() {
        let state = state_of([(Address::repeat_byte(1), touched_eoa())]);
        assert_eq!(
            calc_diff_size(&state),
            expected(ACCOUNT_KEY_BYTES + ACCOUNT_INFO_EOA_BYTES, 0)
        );
    }

    #[test]
    fn contract_costs_more_than_eoa() {
        let eoa = state_of([(Address::repeat_byte(1), touched_eoa())]);
        let contract = state_of([(Address::repeat_byte(1), touched_contract())]);
        assert!(calc_diff_size(&contract) > calc_diff_size(&eoa));
    }

    #[test]
    fn changed_storage_counts_unchanged_does_not() {
        let mut account = touched_eoa();
        // changed slot: original != present
        account.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(9), 0),
        );
        // unchanged slot: original == present
        account
            .storage
            .insert(U256::from(2), EvmStorageSlot::new(U256::from(7), 0));

        let state = state_of([(Address::repeat_byte(1), account)]);
        assert_eq!(
            calc_diff_size(&state),
            expected(
                ACCOUNT_KEY_BYTES + ACCOUNT_INFO_EOA_BYTES,
                SLOT_KEY_BYTES + SLOT_VALUE_BYTES
            )
        );
    }

    #[test]
    fn deterministic_regardless_of_insertion_order() {
        let a = (Address::repeat_byte(1), touched_eoa());
        let b = (Address::repeat_byte(2), touched_contract());
        let forward = state_of([a.clone(), b.clone()]);
        let backward = state_of([b, a]);
        assert_eq!(calc_diff_size(&forward), calc_diff_size(&backward));
    }

    #[test]
    fn da_rate_extra_data_roundtrips() {
        let rate = 2_500_000_000_u64;
        assert_eq!(da_rate_from_extra_data(&da_rate_to_extra_data(rate)), rate);
    }

    #[test]
    fn short_extra_data_decodes_to_zero() {
        // Genesis label / empty field => no rate => charge is dormant.
        assert_eq!(da_rate_from_extra_data(&Bytes::from_static(b"SC")), 0);
        assert_eq!(da_rate_from_extra_data(&Bytes::new()), 0);
    }

    #[test]
    fn bounded_da_fee_caps_at_remaining_value() {
        let da_rate = U256::from(1_000u64);
        // raw = 1000 * 50 = 50_000; budget 60_000 => full fee charged.
        assert_eq!(
            bounded_da_fee(da_rate, 50, U256::from(60_000u64)),
            U256::from(50_000u64)
        );
        // raw = 50_000 but budget only 20_000 => capped (undercharge, never overcharge).
        assert_eq!(
            bounded_da_fee(da_rate, 50, U256::from(20_000u64)),
            U256::from(20_000u64)
        );
        // zero rate or zero budget => zero fee.
        assert_eq!(
            bounded_da_fee(U256::ZERO, 50, U256::from(60_000u64)),
            U256::ZERO
        );
        assert_eq!(bounded_da_fee(da_rate, 50, U256::ZERO), U256::ZERO);
    }
}
