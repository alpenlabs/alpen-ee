//! [`RethEvmCodeProvider`] — Reth-backed [`EvmCodeProvider`] for DA blobs.
//!
//! The DA blob pipeline leaves a bytecode out of a batch's blob only when an
//! account already holds it in the batch's pre-state, so it needs to read
//! historical account code. This adapter lives in the binary crate because it
//! depends on `reth_provider::StateProviderFactory`, which is only available
//! where the Reth node is assembled.

use alloy_primitives::{Address, Bytes, B256};
use alpen_common::EvmCodeProvider;
use reth_provider::StateProviderFactory;

/// [`EvmCodeProvider`] backed by a Reth [`StateProviderFactory`].
pub(crate) struct RethEvmCodeProvider<P> {
    provider: P,
}

impl<P> RethEvmCodeProvider<P> {
    pub(crate) fn new(provider: P) -> Self {
        Self { provider }
    }
}

impl<P> EvmCodeProvider for RethEvmCodeProvider<P>
where
    P: StateProviderFactory + Send + Sync,
{
    fn code_hash_at(&self, block_hash: B256, address: Address) -> eyre::Result<Option<B256>> {
        let state = self.provider.history_by_block_hash(block_hash)?;
        Ok(state
            .basic_account(&address)?
            .map(|account| account.get_bytecode_hash()))
    }

    fn bytecode(&self, code_hash: B256) -> eyre::Result<Option<Bytes>> {
        let state = self.provider.latest()?;
        Ok(state
            .bytecode_by_hash(&code_hash)?
            .map(|bytecode| bytecode.0.original_bytes()))
    }
}
