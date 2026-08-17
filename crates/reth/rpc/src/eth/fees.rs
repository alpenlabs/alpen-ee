//! Fee estimation for the Alpen fee model (execution gas + Bitcoin DA).
//!
//! Exposes `alpen_estimateFees` (the explicit breakdown) and the shared quote logic used
//! by the `eth_estimateGas` override in the `call` module, which folds the DA fee into the
//! returned gas so standard wallets reserve enough to cover the separate DA charge.

use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumberOrTag;
use alloy_json_rpc::RpcObject;
use alloy_network::TransactionBuilder;
use alloy_primitives::U256;
use alloy_rpc_types_eth::{
    state::{EvmOverrides, StateOverride},
    BlockId,
};
use alpen_reth_evm::da_fee::{calc_diff_size, da_rate_from_extra_data};
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_provider::ProviderError;
use reth_rpc_convert::RpcTxReq;
use reth_rpc_eth_api::{
    helpers::{estimate::EstimateCall, Call},
    EthApiTypes, FromEvmError, RpcConvert, RpcNodeCore,
};
use reth_rpc_eth_types::{error::FromEthApiError, EthApiError};
use reth_storage_api::BlockReaderIdExt;
use serde::{Deserialize, Serialize};

use crate::AlpenEthApi;

/// Basis-points denominator for the DA-fee safety margin.
const BPS_DENOM: u64 = 10_000;

/// Safety margin folded into the quoted DA fee (10%).
///
/// The quote inflates the DA fee so the signed effective-gas envelope still covers the
/// charge if the committed DA rate ticks up between the quote and block inclusion. See
/// the fee-model spec (§11, safety).
const DA_FEE_SAFETY_MARGIN_BPS: u64 = 1_000;

/// Quotes the DA fee (wei) for a diff of `diff_size` bytes at `da_rate`, including the
/// safety margin.
pub(crate) fn quote_da_fee(da_rate: u64, diff_size: u64) -> U256 {
    U256::from(da_rate)
        .saturating_mul(U256::from(diff_size))
        .saturating_mul(U256::from(BPS_DENOM + DA_FEE_SAFETY_MARGIN_BPS))
        / U256::from(BPS_DENOM)
}

/// Converts a DA fee (wei) into the gas headroom that covers it at `base_fee`.
///
/// Returns `ceil(da_fee / base_fee)`, or `0` when `base_fee` is zero (before the base-fee
/// floor lands the DA fee cannot be folded into gas).
pub(crate) fn da_fee_to_gas(da_fee: U256, base_fee: u64) -> u64 {
    if base_fee == 0 {
        return 0;
    }
    let base = U256::from(base_fee);
    ((da_fee + base - U256::from(1u64)) / base).saturating_to::<u64>()
}

/// Components of a transaction's DA fee, measured by simulating it once.
pub(crate) struct DaFeeQuote {
    /// Raw EVM gas consumed by the simulated execution.
    pub gas_used: u64,
    /// EIP-1559 base fee per gas at the chain head (wei).
    pub base_fee: u64,
    /// Estimated per-transaction DA payload size (bytes).
    pub diff_size: u64,
    /// Committed DA rate (wei per DA byte) read from the head block header.
    pub da_rate: u64,
    /// DA fee (wei), including the safety margin.
    pub da_fee: U256,
}

/// Full fee breakdown for a transaction, returned by `alpen_estimateFees`.
///
/// Gas and byte-size fields are plain integers; wei amounts are quantities. `effective_gas`
/// is the value a standard wallet should sign as its gas limit so its own
/// `gasLimit * maxFeePerGas` reservation authorizes the separate DA fee.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeEstimate {
    /// Raw EVM gas consumed by execution.
    pub gas_used: u64,
    /// EIP-1559 base fee per gas at the chain head (wei).
    pub base_fee: u64,
    /// Estimated per-transaction DA payload size (bytes).
    pub diff_size: u64,
    /// Committed DA rate (wei per DA byte) read from the head block header.
    pub da_rate: u64,
    /// DA fee charged for this transaction (wei), including the safety margin.
    pub da_fee: U256,
    /// Gas limit a standard wallet should sign: folds the DA fee into gas at `base_fee`.
    pub effective_gas: u64,
    /// Total fee (wei) the sender pays: execution fee (effective gas price * gas, i.e. base
    /// fee plus the priority tip) + DA fee.
    pub total_fee: U256,
}

impl<N, Rpc> AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    /// Simulates `request` once and returns its DA fee components.
    ///
    /// Measures both the gas consumed and the resulting state-diff (the DA footprint) with
    /// the same `calc_diff_size` estimator the STF charge uses, and reads the committed DA
    /// rate and base fee from the chain-head header (the consensus source of truth).
    pub(crate) async fn da_fee_quote(
        &self,
        request: RpcTxReq<
            <<AlpenEthApi<N, Rpc> as EthApiTypes>::RpcConvert as RpcConvert>::Network,
        >,
        at: BlockId,
        state_override: Option<StateOverride>,
    ) -> Result<DaFeeQuote, EthApiError> {
        let res = self
            .transact_call_at(request, at, EvmOverrides::new(state_override, None))
            .await?;
        let gas_used = res.result.gas_used();
        let diff_size = calc_diff_size(&res.state);

        // Read the DA rate and base fee from the header of the block the transaction is
        // simulated against (`at`), so a historical quote uses that block's committed fee
        // parameters rather than the current head's. Falls back to the latest header when
        // `at` does not resolve to a stored header (e.g. the pending tag).
        let header = match self
            .provider()
            .sealed_header_by_id(at)
            .map_err(EthApiError::from_eth_err::<ProviderError>)?
        {
            Some(header) => header,
            None => self
                .provider()
                .latest_header()
                .map_err(EthApiError::from_eth_err::<ProviderError>)?
                .ok_or_else(|| EthApiError::HeaderNotFound(BlockNumberOrTag::Latest.into()))?,
        };
        let da_rate = da_rate_from_extra_data(header.extra_data());
        let base_fee = header.base_fee_per_gas().unwrap_or_default();
        let da_fee = quote_da_fee(da_rate, diff_size);

        Ok(DaFeeQuote {
            gas_used,
            base_fee,
            diff_size,
            da_rate,
            da_fee,
        })
    }

    /// Pins `at` to a concrete block id so a whole fee estimate is computed against a single
    /// snapshot.
    ///
    /// `latest`/number tags otherwise re-resolve independently on each downstream call (the
    /// breakdown quote and the effective-gas estimate), and a block arriving between those
    /// awaits would mix state, base fee, and DA rate from different heights. Concrete hashes
    /// pass through unchanged; a tag that does not resolve to a stored header (e.g. the
    /// `pending` tag) is left as-is.
    fn pin_block_id(&self, at: BlockId) -> Result<BlockId, EthApiError> {
        if matches!(at, BlockId::Hash(_)) {
            return Ok(at);
        }
        match self
            .provider()
            .sealed_header_by_id(at)
            .map_err(EthApiError::from_eth_err::<ProviderError>)?
        {
            Some(header) => Ok(header.hash().into()),
            None => Ok(at),
        }
    }
}

/// Alpen fee-estimation RPC namespace.
#[rpc(server, namespace = "alpen")]
pub trait AlpenFeeApi<TxReq: RpcObject> {
    /// Estimates the full fee (execution gas + Bitcoin DA) for a transaction.
    #[method(name = "estimateFees")]
    async fn estimate_fees(
        &self,
        request: TxReq,
        block_number: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<FeeEstimate>;
}

#[async_trait]
impl<N, Rpc> AlpenFeeApiServer<RpcTxReq<Rpc::Network>> for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    RpcTxReq<Rpc::Network>: RpcObject,
{
    async fn estimate_fees(
        &self,
        request: RpcTxReq<Rpc::Network>,
        block_number: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<FeeEstimate> {
        // Pin the requested block up front so the breakdown and the effective-gas estimate
        // are derived from the same snapshot; `latest` would otherwise be re-resolved by
        // each call below and could straddle a newly produced block.
        let block = self.pin_block_id(block_number.unwrap_or_default())?;
        let quote = self
            .da_fee_quote(request.clone(), block, state_override.clone())
            .await?;

        // effective_gas is the value a wallet signs as its gas limit, so it must be a safe
        // gas *limit*, not the gas *used* by one roomy simulation: the two differ for txs
        // whose minimum viable limit exceeds their consumption (EIP-150 63/64 forwarding,
        // branching on `gasleft()`). Delegate to the same path as `eth_estimateGas`, which
        // binary-searches the execution gas and already folds in the DA-fee headroom.
        // The execution fee is paid at the transaction's *effective* gas price, which the
        // beneficiary receives in full (base fee + priority tip) — so the total must use it,
        // not the base fee alone. Read the fee fields before `request` is consumed below.
        let effective_gas_price = {
            let tx = request.as_ref();
            if let Some(gas_price) = tx.gas_price() {
                // Legacy / EIP-2930: the gas price already includes any tip.
                gas_price
            } else {
                // EIP-1559: base fee plus the tip, capped by the fee ceiling.
                let tip = tx.max_priority_fee_per_gas().unwrap_or(0);
                let max_fee = tx.max_fee_per_gas().unwrap_or(u128::MAX);
                (quote.base_fee as u128).saturating_add(tip).min(max_fee)
            }
        };

        let effective_gas = self
            .estimate_gas_at(request, block, state_override)
            .await?
            .saturating_to::<u64>();
        let total_fee = U256::from(quote.gas_used).saturating_mul(U256::from(effective_gas_price))
            + quote.da_fee;

        Ok(FeeEstimate {
            gas_used: quote.gas_used,
            base_fee: quote.base_fee,
            diff_size: quote.diff_size,
            da_rate: quote.da_rate,
            da_fee: quote.da_fee,
            effective_gas,
            total_fee,
        })
    }
}
