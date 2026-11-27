use evm2::{Evm, Precompiles, SpecId, TxResult, registry::TxRegistry};
use reth_evm_ethereum::EvmFactory;
use strata_bridge_params::BridgeParams;

use crate::{
    evm2_settlement::{AlpenEvmTypes, tx_registry},
    precompiles::create_precompiles,
};

/// Shareable configuration; rates and fee-coverage results belong to individual executions.
#[derive(Debug, Clone)]
pub struct AlpenEvmFactory {
    bridge_params: BridgeParams,
}

impl AlpenEvmFactory {
    pub(crate) const fn new(bridge_params: BridgeParams) -> Self {
        Self { bridge_params }
    }

    /// Returns the bridge withdrawal policy used for precompile validation.
    pub const fn bridge_params(&self) -> &BridgeParams {
        &self.bridge_params
    }
}

impl EvmFactory for AlpenEvmFactory {
    type Types = AlpenEvmTypes;
    type SpecId = SpecId;

    fn spec_id(&self, spec: SpecId) -> SpecId {
        spec
    }

    fn tx_registry(&self, spec: SpecId) -> TxRegistry<AlpenEvmTypes, TxResult<AlpenEvmTypes>> {
        tx_registry(spec)
    }

    fn precompiles(&self, spec: SpecId) -> Precompiles<AlpenEvmTypes> {
        create_precompiles(spec)
    }

    fn configure_evm(&self, evm: &mut Evm<'_, AlpenEvmTypes>) {
        *evm.ext_mut() = Some(self.bridge_params);
    }
}
