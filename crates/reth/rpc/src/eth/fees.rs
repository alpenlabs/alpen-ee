//! `alpen_estimateFees` RPC: the full fee quote (execution gas + Bitcoin DA) for a
//! transaction, including the effective gas a standard wallet should sign.

use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumberOrTag;
use alloy_json_rpc::RpcObject;
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
use reth_rpc_eth_api::{helpers::Call, FromEvmError, RpcConvert, RpcNodeCore};
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
    /// Execution fee (base fee * gas) + DA fee (wei); excludes the priority tip.
    pub total_fee: U256,
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
        let block = block_number.unwrap_or_default();
        let overrides = EvmOverrides::new(state_override, None);

        // Simulate the transaction once to measure both the gas consumed and the
        // resulting state diff (the DA footprint) in a single pass.
        let res = self.transact_call_at(request, block, overrides).await?;
        let gas_used = res.result.gas_used();
        let diff_size = calc_diff_size(&res.state);

        // The DA rate rides the head block header's extra_data (the consensus source of
        // truth used by the STF charge), and the base fee is the EIP-1559 head value the
        // transaction would pay at inclusion.
        let header = self
            .provider()
            .latest_header()
            .map_err(EthApiError::from_eth_err::<ProviderError>)?
            .ok_or_else(|| EthApiError::HeaderNotFound(BlockNumberOrTag::Latest.into()))?;
        let da_rate = da_rate_from_extra_data(header.extra_data());
        let base_fee = header.base_fee_per_gas().unwrap_or_default();

        // Inflate the DA fee by the safety margin so the signed envelope absorbs a mild
        // adverse move in the committed rate before inclusion.
        let da_fee_raw = U256::from(da_rate).saturating_mul(U256::from(diff_size));
        let da_fee = da_fee_raw.saturating_mul(U256::from(BPS_DENOM + DA_FEE_SAFETY_MARGIN_BPS))
            / U256::from(BPS_DENOM);

        // effective_gas folds the DA fee into the signed gas limit at the current base
        // fee. A zero base fee (before the base-fee floor lands) can't fold DA into gas,
        // so fall back to raw gas rather than dividing by zero.
        let effective_gas = if base_fee == 0 {
            gas_used
        } else {
            let base_fee_u = U256::from(base_fee);
            let da_gas = (da_fee + base_fee_u - U256::from(1u64)) / base_fee_u;
            gas_used.saturating_add(da_gas.saturating_to::<u64>())
        };

        let total_fee = U256::from(gas_used).saturating_mul(U256::from(base_fee)) + da_fee;

        Ok(FeeEstimate {
            gas_used,
            base_fee,
            diff_size,
            da_rate,
            da_fee,
            effective_gas,
            total_fee,
        })
    }
}
