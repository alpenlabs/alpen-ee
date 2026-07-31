use core::future::Future;

use alloy_primitives::U256;
use alloy_rpc_types_eth::{state::StateOverride, BlockId};
use reth_rpc_convert::{RpcConvert, RpcTxReq};
use reth_rpc_eth_api::{
    helpers::{estimate::EstimateCall, Call, EthCall, LoadPendingBlock, LoadState, SpawnBlocking},
    FromEvmError, RpcNodeCore,
};
use reth_rpc_eth_types::EthApiError;

use crate::{eth::fees::da_fee_to_gas, AlpenEthApi};

impl<N, Rpc> EthCall for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
}

impl<N, Rpc> EstimateCall for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    /// Returns the **effective gas** for the transaction: reth's standard execution-gas
    /// estimate plus the gas headroom needed to cover the separate Bitcoin-DA fee.
    ///
    /// This keeps unmodified EVM wallets compatible: they call `eth_estimateGas`, sign the
    /// returned value as `gasLimit`, and their own `gasLimit * maxFeePerGas` reservation
    /// then authorizes the DA charge deducted in-EVM at inclusion. Execution is still
    /// billed on actual `gas_used` (unused gas refunded); the inflation is a reservation
    /// envelope, not extra gas spent.
    // Match the trait's `-> impl Future + Send` signature exactly (as reth's own default
    // does); `async fn` cannot restate the explicit `Send` bound the trait requires.
    #[expect(
        clippy::manual_async_fn,
        reason = "must mirror the trait's impl-Future signature"
    )]
    fn estimate_gas_at(
        &self,
        request: RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>,
        at: BlockId,
        state_override: Option<StateOverride>,
    ) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadPendingBlock,
    {
        async move {
            // reth's standard binary-search estimate for the execution-gas component.
            let (evm_env, resolved_at) = self.evm_env_at(at).await?;
            let exec_request = request.clone();
            let exec_override = state_override.clone();
            let raw_gas = self
                .spawn_blocking_io_fut(move |this| async move {
                    let state = this.state_at_block_id(resolved_at).await?;
                    EstimateCall::estimate_gas_with(
                        &this,
                        evm_env,
                        exec_request,
                        state,
                        exec_override,
                    )
                })
                .await?;

            // Fold in the DA-fee headroom so the signed gas limit reserves enough to cover
            // the separate DA charge.
            let quote = self.da_fee_quote(request, at, state_override).await?;
            let da_gas = da_fee_to_gas(quote.da_fee, quote.base_fee);
            Ok(raw_gas.saturating_add(U256::from(da_gas)))
        }
    }
}

impl<N, Rpc> Call for AlpenEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.eth_api().gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.eth_api().max_simulate_blocks()
    }

    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.eth_api().evm_memory_limit()
    }
}
