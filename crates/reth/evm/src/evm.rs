use core::error;
use std::sync::{atomic::AtomicU64, Arc};

use reth_evm::{eth::EthEvmContext, precompiles::PrecompilesMap, Database, EvmEnv, EvmFactory};
use revm::{
    context::{
        result::{EVMError, HaltReason},
        BlockEnv, TxEnv,
    },
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    Context, Inspector, MainBuilder, MainContext,
};
use revm_primitives::{hardfork::SpecId, U256};
use strata_bridge_params::{BridgeParams, DEFAULT_MAX_WITHDRAWAL_DESCRIPTOR_LEN};

use crate::{
    apis::AlpenAlloyEvm, da_fee::DA_COVERAGE_UNKNOWN, precompiles::factory, utils::wei_to_sats,
};

/// Per-transaction gas-limit cap, as a multiple of the block gas limit.
///
/// A transaction's signed `gas_limit` may exceed the block gas limit — it is the
/// DA-inflated *authorized* envelope (execution gas + DA-fee headroom), not execution work —
/// and execution is not otherwise capped. This bound (enforced by the EIP-7825 check in
/// [`crate::apis::validation`], before execution) limits how much a single transaction, or a
/// crafted invalid block's transaction, can force a re-executor to run: at most this multiple
/// of the block's real gas budget. It is generous enough for legitimate DA-heavy transactions
/// (whose real execution must still fit the block) while turning the otherwise balance-bounded
/// per-tx work into a small constant factor of block work.
///
/// NOTE(fee-model, calibration): tune against the maximum realistic DA headroom
/// (`da_rate * diff_size / base_fee`) for a block-filling storage-heavy transaction.
const TX_GAS_LIMIT_BLOCK_MULTIPLE: u64 = 4;

/// Custom EVM configuration.
///
/// Carries the bridge withdrawal policy, precompile surface, and beneficiary
/// reward policy. Production defaults match the proof guests; isolated EEST
/// fixtures select canonical Ethereum behavior explicitly.
/// This is a pure, shareable config object with no interior-mutable state.
///
/// Neither the per-block DA rate (an input) nor the per-transaction DA-coverage report (an
/// output) is held here. reth's `EvmEnv` plumbing cannot thread a per-block value into
/// [`EvmFactory::create_evm`], so [`crate::config::AlpenEvmConfig`] stamps the rate onto each
/// freshly created EVM (via [`crate::apis::AlpenAlloyEvm::set_da_rate`]), and each EVM owns
/// its own coverage cell (minted below, read via
/// [`AlpenAlloyEvm::da_report_handle`](crate::apis::AlpenAlloyEvm::da_report_handle)). Both
/// therefore ride the per-execution EVM rather than shared factory state, which keeps
/// concurrent executions race-free.
#[derive(Debug, Clone)]
pub struct AlpenEvmFactory {
    bridge_params: BridgeParams,
    precompile_policy: PrecompilePolicy,
    beneficiary_reward_policy: BeneficiaryRewardPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrecompilePolicy {
    Alpen,
    EestFixture,
}

/// Determines which transaction fees are credited to the block beneficiary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeneficiaryRewardPolicy {
    /// Credits the base fee and priority fee to the beneficiary, as Alpen does.
    AllGasFees,
    /// Applies Ethereum's fork-aware reward rule, including EIP-1559 base-fee burning.
    Ethereum,
}

// Manual instead of derived: `BridgeParams` has no `Default` (denomination
// zero is invalid).
impl Default for AlpenEvmFactory {
    /// Placeholder withdrawal policy for tests and benchmarks that construct
    /// an `AlpenEvmFactory` but don't exercise bridge-out validation. Not
    /// valid params for any real network.
    fn default() -> Self {
        let bridge_params =
            BridgeParams::new_with_descriptor_limit(100_000_000, Some(1_000_000_000), 81)
                .expect("valid bridge params");
        Self::from_bridge_params(&bridge_params)
    }
}

impl AlpenEvmFactory {
    pub fn new(denomination_wei: U256, max_withdrawal_wei: Option<U256>) -> Self {
        let denomination = wei_to_sats_exact(denomination_wei, "denomination_wei");
        let max_withdrawal_amount =
            max_withdrawal_wei.map(|max| wei_to_sats_exact(max, "max_withdrawal_wei"));

        let bridge_params = BridgeParams::new_with_descriptor_limit(
            denomination,
            max_withdrawal_amount,
            DEFAULT_MAX_WITHDRAWAL_DESCRIPTOR_LEN,
        )
        .expect("withdrawal policy constructed from wei must be valid");
        Self::from_bridge_params(&bridge_params)
    }

    pub fn max_withdrawal_descriptor_len(&self) -> u32 {
        self.bridge_params.max_withdrawal_descriptor_len()
    }

    pub fn bridge_params(&self) -> &BridgeParams {
        &self.bridge_params
    }

    /// Creates an [`AlpenEvmFactory`] from [`BridgeParams`].
    ///
    /// Node production paths and proof guests use this constructor. It must
    /// retain the pre-#143 Berlin+BLS+Schnorr surface at every EVM spec.
    pub fn from_bridge_params(bp: &BridgeParams) -> Self {
        Self {
            bridge_params: *bp,
            precompile_policy: PrecompilePolicy::Alpen,
            beneficiary_reward_policy: BeneficiaryRewardPolicy::AllGasFees,
        }
    }

    /// Selects fork-aware canonical precompiles for the isolated EEST node.
    ///
    /// This must not be used by sequencers, full nodes, or proof guests.
    pub fn with_eest_fixture_precompiles(mut self) -> Self {
        self.precompile_policy = PrecompilePolicy::EestFixture;
        self
    }

    /// Reports whether the isolated fixture precompile surface is selected.
    pub const fn uses_eest_fixture_precompiles(&self) -> bool {
        matches!(self.precompile_policy, PrecompilePolicy::EestFixture)
    }

    /// Selects Ethereum's beneficiary reward rule for isolated fixtures.
    pub fn with_ethereum_beneficiary_reward(mut self) -> Self {
        self.beneficiary_reward_policy = BeneficiaryRewardPolicy::Ethereum;
        self
    }

    fn precompiles(&self, spec: SpecId) -> PrecompilesMap {
        match self.precompile_policy {
            PrecompilePolicy::Alpen => factory::create_precompiles_map(spec, self.bridge_params),
            PrecompilePolicy::EestFixture => factory::create_eest_fixture_precompiles_map(spec),
        }
    }
}

/// Mints a fresh per-EVM DA-coverage report cell, initialised to
/// [`DA_COVERAGE_UNKNOWN`] so an unwritten cell is never read as covered.
fn new_da_report_cell() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(DA_COVERAGE_UNKNOWN))
}

fn wei_to_sats_exact(wei: U256, field: &str) -> u64 {
    let (sats, remainder) = wei_to_sats(wei);
    assert!(
        remainder.is_zero(),
        "{field} must be an exact number of satoshis"
    );
    sats.try_into()
        .expect("withdrawal policy amount must fit in u64 satoshis")
}

impl EvmFactory for AlpenEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>, EthInterpreter>> = AlpenAlloyEvm<DB, I>;
    type Tx = TxEnv;
    type Error<DBError: error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Context<DB: Database> = EthEvmContext<DB>;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, mut input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        // Cap the per-tx gas limit at a multiple of the block gas limit (never loosening any
        // existing cap). Enforced before execution by the EIP-7825
        // check in `validation::validate_env`. Set on the cfg so host and guest agree — both
        // build the EVM here, and the bound is derived from the committed block gas limit.
        let tx_gas_cap = input
            .block_env
            .gas_limit
            .saturating_mul(TX_GAS_LIMIT_BLOCK_MULTIPLE);
        input.cfg_env.tx_gas_limit_cap = Some(
            input
                .cfg_env
                .tx_gas_limit_cap
                .map_or(tx_gas_cap, |c| c.min(tx_gas_cap)),
        );

        let precompiles = self.precompiles(input.cfg_env.spec);

        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(precompiles);

        // The DA rate is stamped per block by `AlpenEvmConfig`; a freshly built EVM starts
        // dormant (rate 0) until then. Each EVM owns its own DA-coverage report cell.
        AlpenAlloyEvm::new(
            evm,
            false,
            U256::ZERO,
            new_da_report_cell(),
            self.beneficiary_reward_policy,
        )
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>, EthInterpreter>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        AlpenAlloyEvm::new(
            self.create_evm(db, input)
                .into_inner()
                .with_inspector(inspector),
            true,
            U256::ZERO,
            new_da_report_cell(),
            self.beneficiary_reward_policy,
        )
    }
}

#[cfg(test)]
mod tests {
    use reth_evm::{EvmEnv, EvmFactory};
    use revm::{
        context::{BlockEnv, TxEnv},
        database::{CacheDB, EmptyDB},
        precompile::u64_to_address,
        state::{AccountInfo, Bytecode},
        ExecuteEvm,
    };
    use revm_primitives::{address, hardfork::SpecId, TxKind, B256, U256};

    use super::AlpenEvmFactory;
    use crate::constants::{BRIDGEOUT_PRECOMPILE_ADDRESS, SCHNORR_PRECOMPILE_ADDRESS};

    const BASE_FEE: u64 = 7;
    const GAS_USED: u64 = 21_000;

    #[test]
    fn production_and_guest_constructor_retains_alpen_precompiles_under_osaka() {
        let default_factory = AlpenEvmFactory::default();
        let guest_factory = AlpenEvmFactory::from_bridge_params(default_factory.bridge_params());
        let explicit_factory =
            AlpenEvmFactory::new(U256::from(1_000_000_000_000_000_000_u64), None);

        for factory in [default_factory, guest_factory, explicit_factory] {
            assert!(!factory.uses_eest_fixture_precompiles());
            let precompiles = factory.precompiles(SpecId::OSAKA);
            assert!(precompiles.get(&BRIDGEOUT_PRECOMPILE_ADDRESS).is_some());
            assert!(precompiles.get(&SCHNORR_PRECOMPILE_ADDRESS).is_some());
            assert!(precompiles.get(&u64_to_address(11)).is_some());
            assert!(precompiles.get(&u64_to_address(256)).is_none());
        }
    }

    #[test]
    fn fixture_factory_excludes_alpen_precompiles_and_uses_osaka_set() {
        let factory = AlpenEvmFactory::default().with_eest_fixture_precompiles();
        assert!(factory.uses_eest_fixture_precompiles());
        let precompiles = factory.precompiles(SpecId::OSAKA);

        assert!(precompiles.get(&BRIDGEOUT_PRECOMPILE_ADDRESS).is_none());
        assert!(precompiles.get(&SCHNORR_PRECOMPILE_ADDRESS).is_none());
        assert!(precompiles.get(&u64_to_address(10)).is_none());
        assert!(precompiles.get(&u64_to_address(11)).is_some());
        assert!(precompiles.get(&u64_to_address(256)).is_some());
    }

    fn beneficiary_balance(factory: &AlpenEvmFactory, gas_price: u128) -> U256 {
        let caller = address!("1000000000000000000000000000000000000000");
        let beneficiary = address!("2000000000000000000000000000000000000000");
        let recipient = address!("3000000000000000000000000000000000000000");
        let mut database = CacheDB::<EmptyDB>::default();
        database.insert_account_info(
            caller,
            AccountInfo::new(
                U256::from(1_000_000_000_u64),
                0,
                B256::ZERO,
                Bytecode::new(),
            ),
        );

        let mut input: EvmEnv<SpecId, BlockEnv> = EvmEnv::default();
        input.cfg_env.spec = SpecId::LONDON;
        input.block_env.beneficiary = beneficiary;
        input.block_env.basefee = BASE_FEE;
        input.block_env.gas_limit = 30_000_000;

        let mut evm = factory.create_evm(database, input);
        let transaction = TxEnv::builder()
            .caller(caller)
            .kind(TxKind::Call(recipient))
            .gas_limit(GAS_USED)
            .gas_price(gas_price)
            .build()
            .expect("test transaction must be valid");
        let result = evm
            .transact(transaction)
            .expect("plain value-less transfer must execute");

        result
            .state
            .get(&beneficiary)
            .map_or(U256::ZERO, |account| account.info.balance)
    }

    #[test]
    fn production_rewards_base_fee_to_beneficiary() {
        let balance = beneficiary_balance(&AlpenEvmFactory::default(), u128::from(BASE_FEE));

        assert_eq!(balance, U256::from(BASE_FEE * GAS_USED));
    }

    #[test]
    fn ethereum_reward_policy_only_rewards_priority_fee() {
        let factory = AlpenEvmFactory::default().with_eest_fixture_semantics();
        let zero_tip_balance = beneficiary_balance(&factory, u128::from(BASE_FEE));
        let gas_price_with_tip = u128::from(BASE_FEE + 4);
        let tipped_balance = beneficiary_balance(&factory, gas_price_with_tip);

        assert_eq!(zero_tip_balance, U256::ZERO);
        assert_eq!(tipped_balance, U256::from(4 * GAS_USED));
    }
}
