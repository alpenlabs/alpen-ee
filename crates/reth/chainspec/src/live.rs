//! Chainspec whose fork schedule can be updated at runtime.
//!
//! reth captures the chainspec by value in essentially every node component
//! at launch, so a fork whose activation is only known at runtime (derived
//! from the VK-update boundary in the EE inbox ordering) cannot be applied by
//! replacing the spec — every holder must observe the change through the
//! handle it already has. [`AlpenChainSpec`] provides that: `Clone` shares
//! the underlying cell, and [`AlpenChainSpec::set_fork_activation`] swaps in
//! an updated snapshot that all clones observe.
//!
//! Snapshots are leaked (`Box::leak`) so every delegating getter — including
//! the `&`-returning ones like [`EthChainSpec::genesis`] and the
//! borrow-returning [`Hardforks::forks_iter`] — can serve references out of
//! one consistent `&'static` snapshot. A swap happens once per fork
//! activation over the life of the chain, so the leak is bounded and tiny.

use std::sync::{
    atomic::{AtomicPtr, Ordering},
    Arc,
};

use alloy_eips::{eip1559::BaseFeeParams, eip7840::BlobParams};
use alloy_evm::eth::spec::EthExecutorSpec;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::{
    Chain, ChainSpec, DepositContract, EthChainSpec, EthereumHardfork, EthereumHardforks,
    ForkCondition, ForkFilter, ForkId, Hardfork, Hardforks, Head,
};
use reth_network_peers::NodeRecord;

/// Error applying a fork activation to a live chainspec.
#[derive(Debug, thiserror::Error)]
pub enum ForkActivationError {
    /// The fork already has a different concrete activation. Activations are
    /// derived deterministically from chain history, so moving one is always
    /// a bug (or an attempt to re-decide an already-passed boundary).
    #[error("fork {fork} already activates at {existing:?}, refusing to move it to {requested:?}")]
    AlreadyScheduled {
        /// The fork whose activation was being set.
        fork: String,
        /// The activation already in the schedule.
        existing: ForkCondition,
        /// The conflicting requested activation.
        requested: ForkCondition,
    },
}

/// Chainspec whose fork schedule can be updated at runtime.
///
/// `Clone` shares the cell, so every holder observes swaps. Reads are a
/// single atomic pointer load; the returned snapshot is immutable and
/// consistent.
#[derive(Debug, Clone)]
pub struct AlpenChainSpec {
    /// Points at the current leaked [`ChainSpec`] snapshot.
    current: Arc<AtomicPtr<ChainSpec>>,
}

impl AlpenChainSpec {
    /// Wraps a base chainspec (fork coordinates as loaded from params).
    pub fn new(inner: ChainSpec) -> Self {
        let leaked: *mut ChainSpec = Box::leak(Box::new(inner));
        Self {
            current: Arc::new(AtomicPtr::new(leaked)),
        }
    }

    /// Returns the current snapshot.
    ///
    /// The reference is `'static` because snapshots are leaked; it stays
    /// valid (but possibly stale) across concurrent swaps.
    pub fn current(&self) -> &'static ChainSpec {
        // SAFETY: the pointer always comes from `Box::leak` (in `new` or
        // `set_fork_activation`) and is never freed.
        unsafe { &*self.current.load(Ordering::Acquire) }
    }

    /// Publishes a new activation for `fork`, observed by every clone.
    ///
    /// Idempotent: re-applying the activation a fork already has is a no-op.
    /// Refuses to move an existing concrete activation — the caller derives
    /// activations deterministically from chain history, so a conflicting
    /// value indicates a bug, not a legitimate reschedule.
    pub fn set_fork_activation<HF: Hardfork + Copy>(
        &self,
        fork: HF,
        cond: ForkCondition,
    ) -> Result<(), ForkActivationError> {
        loop {
            let cur_ptr = self.current.load(Ordering::Acquire);
            // SAFETY: see `current`.
            let cur = unsafe { &*cur_ptr };

            match cur.hardforks.get(fork) {
                Some(existing) if existing == cond => return Ok(()),
                Some(existing) if existing != ForkCondition::Never => {
                    return Err(ForkActivationError::AlreadyScheduled {
                        fork: fork.name().to_string(),
                        existing,
                        requested: cond,
                    });
                }
                _ => {}
            }

            let mut next = cur.clone();
            next.hardforks.insert(fork, cond);
            let next_ptr: *mut ChainSpec = Box::leak(Box::new(next));

            if self
                .current
                .compare_exchange(cur_ptr, next_ptr, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }
            // Lost a race with a concurrent swap: drop our candidate (it was
            // built from a stale snapshot) and retry against the new one.
            // SAFETY: `next_ptr` came from `Box::leak` above and was never
            // published, so no one else can hold it.
            drop(unsafe { Box::from_raw(next_ptr) });
        }
    }

    /// Returns the blob params for the Cancun fork of the current snapshot.
    ///
    /// Field-access escape hatch for call sites that read
    /// `ChainSpec::blob_params` directly.
    pub fn cancun_blob_params(&self) -> BlobParams {
        self.current().blob_params.cancun
    }
}

impl EthChainSpec for AlpenChainSpec {
    type Header = alloy_consensus::Header;

    fn chain(&self) -> Chain {
        self.current().chain()
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.current().base_fee_params_at_timestamp(timestamp)
    }

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        self.current().blob_params_at_timestamp(timestamp)
    }

    fn deposit_contract(&self) -> Option<&DepositContract> {
        self.current().deposit_contract()
    }

    fn genesis_hash(&self) -> B256 {
        self.current().genesis_hash()
    }

    fn prune_delete_limit(&self) -> usize {
        self.current().prune_delete_limit()
    }

    fn display_hardforks(&self) -> Box<dyn core::fmt::Display> {
        Box::new(self.current().display_hardforks())
    }

    fn genesis_header(&self) -> &Self::Header {
        self.current().genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.current().genesis()
    }

    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        self.current().bootnodes()
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.current().final_paris_total_difficulty()
    }
}

impl Hardforks for AlpenChainSpec {
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.current().fork(fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.current().forks_iter()
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        Hardforks::fork_id(self.current(), head)
    }

    fn latest_fork_id(&self) -> ForkId {
        Hardforks::latest_fork_id(self.current())
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        Hardforks::fork_filter(self.current(), head)
    }
}

impl EthereumHardforks for AlpenChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.current().ethereum_fork_activation(fork)
    }
}

impl EthExecutorSpec for AlpenChainSpec {
    fn deposit_contract_address(&self) -> Option<Address> {
        self.current().deposit_contract_address()
    }
}

#[cfg(test)]
mod tests {
    use reth_chainspec::{EthereumHardfork, EthereumHardforks, ForkCondition};

    use super::AlpenChainSpec;
    use crate::{chain_value_parser, DEV_CHAIN_SPEC};

    fn dev_spec() -> AlpenChainSpec {
        let base = chain_value_parser(DEV_CHAIN_SPEC).expect("dev chain should parse");
        AlpenChainSpec::new((*base).clone())
    }

    #[test]
    fn clones_observe_activation_swaps() {
        let spec = dev_spec();
        let clone = spec.clone();
        assert!(!clone.is_osaka_active_at_timestamp(1_000_000));

        spec.set_fork_activation(EthereumHardfork::Osaka, ForkCondition::Timestamp(500_000))
            .expect("activation should apply");

        assert!(clone.is_osaka_active_at_timestamp(1_000_000));
        assert!(!clone.is_osaka_active_at_timestamp(499_999));
    }

    #[test]
    fn activation_is_idempotent_but_immovable() {
        let spec = dev_spec();
        let cond = ForkCondition::Timestamp(500_000);
        spec.set_fork_activation(EthereumHardfork::Osaka, cond)
            .expect("first activation should apply");
        spec.set_fork_activation(EthereumHardfork::Osaka, cond)
            .expect("re-applying the same activation is a no-op");
        assert!(spec
            .set_fork_activation(EthereumHardfork::Osaka, ForkCondition::Timestamp(600_000))
            .is_err());
    }

    #[test]
    fn genesis_facts_survive_swaps() {
        let spec = dev_spec();
        let hash_before = spec.current().genesis_hash();
        spec.set_fork_activation(EthereumHardfork::Osaka, ForkCondition::Timestamp(500_000))
            .expect("activation should apply");
        assert_eq!(spec.current().genesis_hash(), hash_before);
    }
}
