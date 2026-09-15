use std::sync::OnceLock;

use revm::{
    handler::EthPrecompiles,
    precompile::{kzg_point_evaluation, PrecompileSpecId, Precompiles},
};
use revm_primitives::hardfork::SpecId;

mod bridge;
pub mod factory;
mod schnorr;

/// A custom precompile that contains static precompiles.
#[expect(
    missing_debug_implementations,
    reason = "Precompiles struct contains static precompiles that don't need debug implementation"
)]
#[derive(Clone, Default)]
pub struct AlpenEvmPrecompiles {
    pub inner: EthPrecompiles,
}

impl AlpenEvmPrecompiles {
    #[inline]
    pub fn new(spec: SpecId) -> Self {
        let precompiles = load_precompiles(spec);
        Self {
            inner: EthPrecompiles { precompiles, spec },
        }
    }

    #[inline]
    pub fn precompiles(&self) -> &'static Precompiles {
        self.inner.precompiles
    }
}

const PRECOMPILE_SPEC_COUNT: usize = 7;

const fn precompile_spec_index(spec: PrecompileSpecId) -> usize {
    match spec {
        PrecompileSpecId::HOMESTEAD => 0,
        PrecompileSpecId::BYZANTIUM => 1,
        PrecompileSpecId::ISTANBUL => 2,
        PrecompileSpecId::BERLIN => 3,
        PrecompileSpecId::CANCUN => 4,
        PrecompileSpecId::PRAGUE => 5,
        PrecompileSpecId::OSAKA => 6,
    }
}

fn alpen_precompiles(spec: PrecompileSpecId) -> Precompiles {
    let mut unsupported = Precompiles::default();
    unsupported.extend([kzg_point_evaluation::POINT_EVALUATION]);

    let mut precompiles = Precompiles::new(spec).difference(&unsupported);
    precompiles.extend([schnorr::SCHNORR_SIGNATURE_VALIDATION]);
    precompiles
}

/// Returns Alpen's precompiles for the requested EVM spec.
///
/// The table follows Ethereum's fork-aware precompile set, removes only the
/// unsupported EIP-4844 point-evaluation precompile, and adds Alpen's custom
/// Schnorr verifier at every fork.
pub fn load_precompiles(spec: SpecId) -> &'static Precompiles {
    static INSTANCES: OnceLock<[Precompiles; PRECOMPILE_SPEC_COUNT]> = OnceLock::new();
    let instances = INSTANCES.get_or_init(|| {
        [
            alpen_precompiles(PrecompileSpecId::HOMESTEAD),
            alpen_precompiles(PrecompileSpecId::BYZANTIUM),
            alpen_precompiles(PrecompileSpecId::ISTANBUL),
            alpen_precompiles(PrecompileSpecId::BERLIN),
            alpen_precompiles(PrecompileSpecId::CANCUN),
            alpen_precompiles(PrecompileSpecId::PRAGUE),
            alpen_precompiles(PrecompileSpecId::OSAKA),
        ]
    });

    &instances[precompile_spec_index(spec.into())]
}

#[cfg(test)]
mod tests {
    use revm::precompile::u64_to_address;
    use revm_primitives::hardfork::SpecId;

    use super::{load_precompiles, schnorr};

    #[test]
    fn ethereum_precompiles_follow_the_active_fork() {
        let byzantium = load_precompiles(SpecId::BYZANTIUM);
        let istanbul = load_precompiles(SpecId::ISTANBUL);
        let cancun = load_precompiles(SpecId::CANCUN);
        let prague = load_precompiles(SpecId::PRAGUE);

        assert!(!byzantium.contains(&u64_to_address(9)));
        assert!(istanbul.contains(&u64_to_address(9)));
        assert!(!cancun.contains(&u64_to_address(11)));
        assert!(prague.contains(&u64_to_address(11)));
    }

    #[test]
    fn unsupported_point_evaluation_is_absent_after_cancun() {
        for spec in [SpecId::CANCUN, SpecId::PRAGUE, SpecId::OSAKA] {
            assert!(!load_precompiles(spec).contains(&u64_to_address(10)));
        }
    }

    #[test]
    fn schnorr_precompile_is_available_at_every_fork() {
        for spec in [
            SpecId::HOMESTEAD,
            SpecId::BYZANTIUM,
            SpecId::ISTANBUL,
            SpecId::BERLIN,
            SpecId::CANCUN,
            SpecId::PRAGUE,
            SpecId::OSAKA,
        ] {
            assert!(
                load_precompiles(spec).contains(schnorr::SCHNORR_SIGNATURE_VALIDATION.address())
            );
        }
    }
}
