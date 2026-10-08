use std::sync::OnceLock;

use reth_evm::precompiles::{DynPrecompile, PrecompilesMap};
use revm::precompile::{bls12_381, PrecompileId, Precompiles};
use strata_bridge_params::BridgeParams;

use crate::constants::{BRIDGEOUT_PRECOMPILE_ADDRESS, BRIDGEOUT_PRECOMPILE_ID};

mod bridge;
mod schnorr;

/// Creates the precompiles of a new EVM.
pub fn create_precompiles_map(bridge_params: BridgeParams) -> PrecompilesMap {
    let mut precompiles = PrecompilesMap::from_static(static_precompiles());

    // Bridge-out is added per EVM, not kept in the static set: it is stateful, and the closure
    // captures the withdrawal params it validates amounts against.
    precompiles.apply_precompile(&BRIDGEOUT_PRECOMPILE_ADDRESS, |_| {
        Some(DynPrecompile::new_stateful(
            PrecompileId::custom(BRIDGEOUT_PRECOMPILE_ID),
            move |input| bridge::bridge_context_call(input, bridge_params),
        ))
    });

    precompiles
}

/// Returns the precompiles that are the same for every EVM.
fn static_precompiles() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        // Alpen EVM supports all Ethereum precompiles up to the Pectra fork.
        // However, we want to disable the point evaluation precompile introduced in the Cancun
        // fork. Therefore, we start with the Berlin precompiles and manually add the ones
        // needed for Pectra.
        let mut precompiles = Precompiles::berlin().clone();

        // EIP-2537: Precompile for BLS12-381
        precompiles.extend(bls12_381::precompiles());

        // Custom precompile.
        precompiles.extend([schnorr::SCHNORR_SIGNATURE_VALIDATION]);

        precompiles
    })
}
