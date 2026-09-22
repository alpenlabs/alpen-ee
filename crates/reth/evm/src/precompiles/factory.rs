use reth_evm::precompiles::{DynPrecompile, PrecompilesMap};
use revm::precompile::PrecompileId;
use revm_primitives::hardfork::SpecId;
use strata_bridge_params::BridgeParams;

use crate::{
    constants::{BRIDGEOUT_PRECOMPILE_ADDRESS, BRIDGEOUT_PRECOMPILE_ID},
    precompiles::{
        bridge::bridge_context_call, load_supported_ethereum_precompiles, AlpenEvmPrecompiles,
    },
};

/// Creates a precompiles map with Alpen-specific precompiles, including the bridge precompile.
pub fn create_precompiles_map(spec: SpecId, bridge_params: BridgeParams) -> PrecompilesMap {
    let mut precompiles = PrecompilesMap::from_static(AlpenEvmPrecompiles::new(spec).precompiles());

    // Add bridge precompile using DynPrecompile for compatibility.
    // The closure captures withdrawal params so the precompile can validate amounts.
    precompiles.apply_precompile(&BRIDGEOUT_PRECOMPILE_ADDRESS, |_| {
        Some(DynPrecompile::new_stateful(
            PrecompileId::custom(BRIDGEOUT_PRECOMPILE_ID),
            move |input| bridge_context_call(input, bridge_params),
        ))
    });

    precompiles
}

/// Creates the fork-aware canonical map only for an isolated EEST fixture node.
pub(crate) fn create_eest_fixture_precompiles_map(spec: SpecId) -> PrecompilesMap {
    PrecompilesMap::from_static(load_supported_ethereum_precompiles(spec))
}
