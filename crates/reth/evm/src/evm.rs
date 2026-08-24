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
/// Carries only the bridge withdrawal policy used for precompile validation — it is a pure,
/// shareable config object with no interior-mutable per-execution state.
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
}

// Manual instead of derived: `BridgeParams` has no `Default` (denomination
// zero is invalid).
impl Default for AlpenEvmFactory {
    /// Placeholder withdrawal policy for tests and benchmarks that construct
    /// an `AlpenEvmFactory` but don't exercise bridge-out validation. Not
    /// valid params for any real network.
    fn default() -> Self {
        Self {
            bridge_params: BridgeParams::new_with_descriptor_limit(
                100_000_000,
                Some(1_000_000_000),
                81,
            )
            .expect("valid bridge params"),
        }
    }
}

impl AlpenEvmFactory {
    pub fn new(denomination_wei: U256, max_withdrawal_wei: Option<U256>) -> Self {
        let denomination = wei_to_sats_exact(denomination_wei, "denomination_wei");
        let max_withdrawal_amount =
            max_withdrawal_wei.map(|max| wei_to_sats_exact(max, "max_withdrawal_wei"));

        Self {
            bridge_params: BridgeParams::new_with_descriptor_limit(
                denomination,
                max_withdrawal_amount,
                DEFAULT_MAX_WITHDRAWAL_DESCRIPTOR_LEN,
            )
            .expect("withdrawal policy constructed from wei must be valid"),
        }
    }

    pub fn max_withdrawal_descriptor_len(&self) -> u32 {
        self.bridge_params.max_withdrawal_descriptor_len()
    }

    pub fn bridge_params(&self) -> &BridgeParams {
        &self.bridge_params
    }

    /// Creates an [`AlpenEvmFactory`] from [`BridgeParams`].
    pub fn from_bridge_params(bp: &BridgeParams) -> Self {
        Self { bridge_params: *bp }
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

        let precompiles = factory::create_precompiles_map(input.cfg_env.spec, self.bridge_params);

        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(precompiles);

        // The DA rate is stamped per block by `AlpenEvmConfig`; a freshly built EVM starts
        // dormant (rate 0) until then. Each EVM owns its own DA-coverage report cell.
        AlpenAlloyEvm::new(evm, false, U256::ZERO, new_da_report_cell())
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
        )
    }
}
