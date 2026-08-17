use core::future::Future;

use alloy_network::TransactionBuilder;
use alloy_primitives::U256;
use alloy_rpc_types_eth::{state::StateOverride, BlockId};
use reth_rpc_convert::{RpcConvert, RpcTxReq};
use reth_rpc_eth_api::{
    helpers::{estimate::EstimateCall, Call, EthCall, LoadPendingBlock, LoadState, SpawnBlocking},
    FromEvmError, RpcNodeCore,
};
use reth_rpc_eth_types::EthApiError;

use crate::{eth::fees::da_fee_to_gas, AlpenEthApi};

/// Cap on the DA-gas fixpoint iterations in [`AlpenEthApi::estimate_gas_at`].
///
/// The signed gas limit is `raw_gas + da_gas`, and the DA gas depends (for `gasleft()`- or
/// EIP-150-sensitive contracts) on the limit the diff is measured at, so the two are solved
/// by iteration. The DA gas is a small, weakly-dependent addend that settles in a step or
/// two; this bounds the worst case for a pathological contract that never converges.
const DA_GAS_FIXPOINT_ITERS: usize = 3;

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
            // the separate DA charge. Quote against `resolved_at` — the concrete block the
            // raw-gas simulation ran on — not the caller's `at`, so a block landing between
            // the awaits can't pair execution gas from one block with a DA diff, rate, and
            // base fee from another.
            //
            // The DA fee is sized from the transaction's state diff, which for `gasleft()`-
            // or EIP-150-sensitive contracts can depend on the gas limit. So the quote must
            // simulate at the same limit the wallet will ultimately sign (`raw_gas +
            // da_gas`), not a roomy default — otherwise a contract that branches on remaining
            // gas could produce a different diff (and DA fee) at inclusion than was quoted.
            // The signed limit itself depends on the DA gas, so iterate to a (capped)
            // fixpoint.
            let mut effective_gas = raw_gas;
            let mut last_da_gas: Option<u64> = None;
            for _ in 0..DA_GAS_FIXPOINT_ITERS {
                let mut quote_request = request.clone();
                quote_request
                    .as_mut()
                    .set_gas_limit(effective_gas.saturating_to::<u64>());
                let quote = self
                    .da_fee_quote(quote_request, resolved_at, state_override.clone())
                    .await?;
                let da_gas = da_fee_to_gas(quote.da_fee, quote.base_fee);
                effective_gas = raw_gas.saturating_add(U256::from(da_gas));
                if last_da_gas == Some(da_gas) {
                    break;
                }
                last_da_gas = Some(da_gas);
            }
            Ok(effective_gas)
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
