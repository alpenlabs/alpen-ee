use std::sync::OnceLock;

use revm::{
    handler::EthPrecompiles,
    precompile::{bls12_381, kzg_point_evaluation, PrecompileSpecId, Precompiles},
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
#[derive(Clone)]
pub struct AlpenEvmPrecompiles {
    pub inner: EthPrecompiles,
}

impl AlpenEvmPrecompiles {
    #[inline]
    pub fn new(spec: SpecId) -> Self {
        let precompiles = load_precompiles();
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

fn supported_ethereum_precompiles(spec: PrecompileSpecId) -> Precompiles {
    let mut unsupported = Precompiles::default();
    unsupported.extend([kzg_point_evaluation::POINT_EVALUATION]);

    Precompiles::new(spec).difference(&unsupported)
}

/// Returns the pre-#143 Alpen production precompile set at every EVM spec.
///
/// The production STF and proof guests both use Berlin's gas rules plus the
/// BLS12-381 and Alpen Schnorr precompiles, including under Osaka. Changing
/// this set requires a protocol upgrade, not an EEST fixture adjustment.
pub fn load_precompiles() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = Precompiles::berlin().clone();
        precompiles.extend(bls12_381::precompiles());
        precompiles.extend([schnorr::SCHNORR_SIGNATURE_VALIDATION]);
        precompiles
    })
}

/// Returns fork-aware Ethereum precompiles for isolated EEST fixtures.
///
/// EIP-4844 point evaluation is unsupported; Alpen's Schnorr and bridge
/// precompiles are absent. Production and proof paths use [`load_precompiles`].
pub(crate) fn load_supported_ethereum_precompiles(spec: SpecId) -> &'static Precompiles {
    static INSTANCES: OnceLock<[Precompiles; PRECOMPILE_SPEC_COUNT]> = OnceLock::new();
    let instances = INSTANCES.get_or_init(|| {
        [
            supported_ethereum_precompiles(PrecompileSpecId::HOMESTEAD),
            supported_ethereum_precompiles(PrecompileSpecId::BYZANTIUM),
            supported_ethereum_precompiles(PrecompileSpecId::ISTANBUL),
            supported_ethereum_precompiles(PrecompileSpecId::BERLIN),
            supported_ethereum_precompiles(PrecompileSpecId::CANCUN),
            supported_ethereum_precompiles(PrecompileSpecId::PRAGUE),
            supported_ethereum_precompiles(PrecompileSpecId::OSAKA),
        ]
    });

    &instances[precompile_spec_index(spec.into())]
}

#[cfg(test)]
mod tests {
    use revm::precompile::{bls12_381, modexp, u64_to_address, Precompiles};
    use revm_primitives::hardfork::SpecId;

    use super::{load_precompiles, load_supported_ethereum_precompiles, schnorr};

    #[test]
    fn production_precompiles_keep_berlin_modexp_and_exclude_osaka_p256() {
        let production = load_precompiles();
        let mut pre_143 = Precompiles::berlin().clone();
        pre_143.extend(bls12_381::precompiles());
        pre_143.extend([schnorr::SCHNORR_SIGNATURE_VALIDATION]);
        assert_eq!(production.addresses_set(), pre_143.addresses_set());

        let modexp_address = u64_to_address(5);
        let production_result = production
            .get(&modexp_address)
            .expect("production modexp must exist")
            .execute(&[], u64::MAX, 0)
            .expect("empty modexp input must execute");
        let berlin_result = modexp::BERLIN
            .execute(&[], u64::MAX, 0)
            .expect("Berlin modexp must execute");
        let osaka_result = modexp::OSAKA
            .execute(&[], u64::MAX, 0)
            .expect("Osaka modexp must execute");

        assert_eq!(production_result.gas_used, berlin_result.gas_used);
        assert_ne!(production_result.gas_used, osaka_result.gas_used);
        assert!(!production.contains(&u64_to_address(256)));
        assert!(production.contains(&u64_to_address(11)));
        assert!(production.contains(schnorr::SCHNORR_SIGNATURE_VALIDATION.address()));
    }

    #[test]
    fn fixture_precompiles_follow_the_active_fork() {
        let byzantium = load_supported_ethereum_precompiles(SpecId::BYZANTIUM);
        let istanbul = load_supported_ethereum_precompiles(SpecId::ISTANBUL);
        let cancun = load_supported_ethereum_precompiles(SpecId::CANCUN);
        let prague = load_supported_ethereum_precompiles(SpecId::PRAGUE);
        let osaka = load_supported_ethereum_precompiles(SpecId::OSAKA);

        assert!(!byzantium.contains(&u64_to_address(9)));
        assert!(istanbul.contains(&u64_to_address(9)));
        assert!(!cancun.contains(&u64_to_address(11)));
        assert!(prague.contains(&u64_to_address(11)));
        assert!(osaka.contains(&u64_to_address(256)));

        let osaka_modexp = osaka
            .get(&u64_to_address(5))
            .expect("Osaka fixture modexp must exist")
            .execute(&[], u64::MAX, 0)
            .expect("Osaka fixture modexp must execute");
        let canonical_osaka_modexp = modexp::OSAKA
            .execute(&[], u64::MAX, 0)
            .expect("canonical Osaka modexp must execute");
        assert_eq!(osaka_modexp.gas_used, canonical_osaka_modexp.gas_used);
    }

    #[test]
    fn fixture_excludes_unsupported_and_alpen_precompiles() {
        for spec in [SpecId::CANCUN, SpecId::PRAGUE, SpecId::OSAKA] {
            let fixture = load_supported_ethereum_precompiles(spec);
            assert!(!fixture.contains(&u64_to_address(10)));
            assert!(!fixture.contains(schnorr::SCHNORR_SIGNATURE_VALIDATION.address()));
        }
    }
}
