use core::marker::PhantomData;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use revm::{
    context::{
        result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
        Block, ContextTr, JournalTr, Transaction,
    },
    handler::{
        instructions::InstructionProvider, EvmTr, FrameResult, FrameTr, Handler, MainnetHandler,
        PrecompileProvider,
    },
    inspector::{InspectorEvmTr, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInit, InterpreterResult},
    state::EvmState,
    Database, Inspector,
};
use revm_primitives::U256;

use crate::{
    apis::validation,
    constants::DA_FEE_VAULT_ADDRESS,
    da_fee::{
        calc_diff_size, DaStateAccess, DA_COVERAGE_CAPPED, DA_COVERAGE_OK, DA_COVERAGE_UNKNOWN,
    },
};

#[expect(
    missing_debug_implementations,
    reason = "Handler struct contains phantom data and doesn't need debug implementation"
)]
pub struct AlpenRevmHandler<EVM> {
    /// Per-block DA rate (wei per byte) used to charge the DA fee.
    da_rate: U256,
    /// Shared cell into which the handler records whether the transaction's DA fee was
    /// capped by its unused authorized gas. The payload builder reads this to skip
    /// under-covered transactions; re-execution ignores it. The cell is owned by the
    /// per-block EVM (`AlpenAlloyEvm`).
    da_report: Arc<AtomicU64>,
    pub _phantom: PhantomData<EVM>,
}

impl<EVM> AlpenRevmHandler<EVM> {
    /// Creates a handler that charges the DA fee at the given per-block rate and records
    /// per-transaction coverage into `da_report`.
    pub fn new(da_rate: U256, da_report: Arc<AtomicU64>) -> Self {
        Self {
            da_rate,
            da_report,
            _phantom: PhantomData,
        }
    }
}

impl<EVM> Default for AlpenRevmHandler<EVM> {
    fn default() -> Self {
        Self {
            da_rate: U256::ZERO,
            da_report: Arc::new(AtomicU64::new(DA_COVERAGE_UNKNOWN)),
            _phantom: PhantomData,
        }
    }
}

impl<EVM> Handler for AlpenRevmHandler<EVM>
where
    EVM: EvmTr<
        Context: ContextTr<Journal: JournalTr<State = EvmState>> + DaStateAccess,
        Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
        Instructions: InstructionProvider<
            Context = EVM::Context,
            InterpreterTypes = EthInterpreter,
        >,
        Frame: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
    >,
{
    type Evm = EVM;
    type Error = EVMError<<<EVM::Context as ContextTr>::Db as Database>::Error, InvalidTransaction>;
    type HaltReason = HaltReason;

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        let context = evm.ctx();
        let block = context.block();
        let tx = context.tx();
        let beneficiary = block.beneficiary();
        let basefee = block.basefee() as u128;
        let effective_gas_price = tx.effective_gas_price(basefee);

        let gas = exec_result.gas();
        let gas_used = (gas.spent() - gas.refunded() as u64) as u128;

        // Credit all gas fees to the beneficiary (base fee + priority fee).
        context
            .journal_mut()
            .load_account_mut(beneficiary)?
            .incr_balance(U256::from(effective_gas_price * gas_used));

        Ok(())
    }

    /// Charges the per-transaction DA fee, then runs the default output handling.
    ///
    /// The DA fee is applied here — the final post-execution step, after the gas refund
    /// (`reimburse_caller`) and the beneficiary reward — so it is drawn from the caller's
    /// unused, already-refunded gas budget. This keeps the DA logic separate from the
    /// gas-reward hook and mirrors the split Citrea uses (base fee vs L1 fee). The default
    /// output handling (which commits the transaction) then runs via [`MainnetHandler`].
    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        if self.da_rate != U256::ZERO {
            let context = evm.ctx();
            let caller = context.tx().caller();
            let basefee = context.block().basefee() as u128;
            let effective_gas_price = context.tx().effective_gas_price(basefee);

            // Skip system/zero-fee calls (effective_gas_price == 0).
            if effective_gas_price != 0 {
                let gas = result.gas();
                // Value of the gas budget the caller authorized but did not consume — it
                // was just refunded to the caller, so a DA fee bounded by it is always
                // covered and never over-charges what the signature authorized.
                let remaining_gas = (gas.remaining() as u128) + (gas.refunded() as u128);
                let remaining_value = U256::from(remaining_gas) * U256::from(effective_gas_price);
                let diff_size = calc_diff_size(context.evm_state());
                let uncapped_da_fee = self.da_rate.saturating_mul(U256::from(diff_size));

                // Cap the fee at the unused authorized gas (never fail, never over-charge).
                // Record whether the cap bound so the sequencer's payload builder can skip
                // under-covered (would-be-subsidized) transactions; re-execution ignores it.
                let da_fee = uncapped_da_fee.min(remaining_value);
                let coverage = if uncapped_da_fee > remaining_value {
                    DA_COVERAGE_CAPPED
                } else {
                    DA_COVERAGE_OK
                };
                self.da_report.store(coverage, Ordering::Relaxed);

                if da_fee != U256::ZERO {
                    // Debit caller and credit the vault via the journal so both accounts
                    // are loaded/journaled (a direct state-map insert panics bundle
                    // assembly). Done before the default handler commits the transaction.
                    context
                        .journal_mut()
                        .load_account_mut(caller)?
                        .decr_balance(da_fee);
                    context
                        .journal_mut()
                        .load_account_mut(DA_FEE_VAULT_ADDRESS)?
                        .incr_balance(da_fee);
                }
            }
        }

        MainnetHandler::<EVM, Self::Error, <EVM as EvmTr>::Frame>::default()
            .execution_result(evm, result)
    }

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        // uses the validation module to validate the environment with disables the 4844 transaction
        validation::validate_env(evm.ctx())
    }
}

impl<EVM> InspectorHandler for AlpenRevmHandler<EVM>
where
    EVM: InspectorEvmTr<
        Inspector: Inspector<<<Self as Handler>::Evm as EvmTr>::Context, EthInterpreter>,
        Context: ContextTr<Journal: JournalTr<State = EvmState>> + DaStateAccess,
        Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
        Instructions: InstructionProvider<
            Context = EVM::Context,
            InterpreterTypes = EthInterpreter,
        >,
    >,
{
    type IT = EthInterpreter;
}
