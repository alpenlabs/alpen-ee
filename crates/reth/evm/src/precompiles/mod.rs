use alpen_primitives::BRIDGEOUT_PRECOMPILE_ADDRESS;
use evm2::{
    precompiles::{Precompile, PrecompileId},
    Precompiles, SpecId,
};
use revm_primitives::Address;

use crate::{
    constants::{
        BRIDGEOUT_PRECOMPILE_ID, SCHNORR_PRECOMPILE_ADDRESS, SCHNORR_PRECOMPILE_PRECOMPILE_ID,
    },
    evm2_settlement::AlpenEvmTypes,
};

mod bridge;
mod schnorr;

const KZG_POINT_EVALUATION: Address = Address::with_last_byte(0x0a);

/// Registers Alpen's precompiles and excludes Ethereum's blob commitment precompile.
pub(crate) fn create_precompiles(spec: SpecId) -> Precompiles<AlpenEvmTypes> {
    let mut precompiles = Precompiles::base(spec);
    let map = precompiles.as_map_mut();
    map.remove(KZG_POINT_EVALUATION);
    map.insert(Precompile::new(
        SCHNORR_PRECOMPILE_ADDRESS,
        PrecompileId::custom(SCHNORR_PRECOMPILE_PRECOMPILE_ID),
        schnorr::run,
    ));
    map.insert(Precompile::new(
        BRIDGEOUT_PRECOMPILE_ADDRESS,
        PrecompileId::custom(BRIDGEOUT_PRECOMPILE_ID),
        bridge::bridge_context_call,
    ));
    precompiles
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, iter};

    use alloy_consensus::Header;
    use alpen_params::{AlpenParams, AlpenSpecId, DEV_PARAMS_JSON};
    use evm2::{
        evm::EmptyDB,
        interpreter::{GasTracker, Message},
        precompiles::PrecompileResult,
        Evm, PrecompileError, PrecompileHalt,
    };
    use reth_evm::{BlockExecutorFactory, ConfigureEvm, EvmEnv};
    use revm_primitives::{hex, B256};

    use super::*;
    use crate::config::AlpenEvmConfig;

    const MODEXP_ADDRESS: Address = Address::with_last_byte(5);
    const P256VERIFY_ADDRESS: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]);

    /// A valid P256VERIFY input from revm's test vectors: message hash, r, s, x, y.
    const VALID_P256_SIGNATURE: [u8; 160] = hex!(
        "4cee90eb86eaa050036147a12d49004b6b9c72bd725d39d4785011fe190f0b4da73bd4903f0ce3b639bbbf6e8e80d16931ff4bcf5993d58468e8fb19086e8cac36dbcd03009df8c59286b162af3bd7fcc0450c9aa81be5d10d312af6c66b1d604aebd3099c618202fcfe16ae7770b0c49ab5eadf74b754204a3bb6060e44eff37618b065f9832de4ca6ca971a7a1adc826d0f7c00181a5fb2ddf79ae00b4e10e"
    );

    /// What sets one version's precompiles apart from another's.
    struct VersionPrecompiles {
        /// The least gas a modexp call costs.
        modexp_floor: u64,
        /// Whether modexp refuses inputs over 1024 bytes.
        caps_modexp_input: bool,
        /// Whether P256VERIFY exists at 0x100.
        has_p256verify: bool,
    }

    fn evm_and_precompiles(
        version: AlpenSpecId,
    ) -> (Evm<'static, AlpenEvmTypes>, Precompiles<AlpenEvmTypes>) {
        let params: AlpenParams =
            serde_json::from_str(DEV_PARAMS_JSON).expect("dev params should parse");
        let config = AlpenEvmConfig::new(&params, version);
        let env = config.evm_env(&Header::default()).expect("evm_env");
        let precompiles = create_precompiles(env.spec_id());
        let evm = config
            .block_executor_factory()
            .evm_with_env(EmptyDB::default(), env);
        (evm, precompiles)
    }

    fn call(
        evm: &mut Evm<'_, AlpenEvmTypes>,
        precompiles: &Precompiles<AlpenEvmTypes>,
        address: Address,
        data: &[u8],
    ) -> (u64, PrecompileResult) {
        const LIMIT: u64 = 1_000_000;
        let mut gas = GasTracker::new(LIMIT);
        let msg = Message::<AlpenEvmTypes> {
            destination: address,
            call_target: address,
            code_address: address,
            gas_limit: LIMIT,
            input: revm_primitives::Bytes::copy_from_slice(data).into(),
            ..Default::default()
        };
        let output = precompiles
            .as_map()
            .get(&address)
            .unwrap()
            .data()
            .execute(evm, &msg, &mut gas);
        (LIMIT - gas.remaining(), output)
    }

    /// Pins the precompiles of every version. Adding a version breaks the `match` below until
    /// its set is written down, which is the point to check the proving cost of any precompile
    /// its fork brings in.
    ///
    /// The set is picked by the Ethereum fork the version runs, so a version cannot change it
    /// without moving to a new fork. A version that needs to would have to install its set in
    /// `create_executor` instead, the way the DA rate is applied.
    #[test]
    fn every_version_has_its_precompiles() {
        // 0x01-0x09, BLS12-381 at 0x0b-0x11, and the two Alpen precompiles. KZG point
        // evaluation (0x0a) is left out of every version.
        let shared_addresses: BTreeSet<Address> = (0x01..=0x09)
            .chain(0x0b..=0x11)
            .map(Address::with_last_byte)
            .chain([BRIDGEOUT_PRECOMPILE_ADDRESS, SCHNORR_PRECOMPILE_ADDRESS])
            .collect();

        // A modexp header that declares a 1025-byte base and empty exponent and modulus.
        let mut oversized_modexp = [0u8; 96];
        oversized_modexp[30..32].copy_from_slice(&1025u16.to_be_bytes());

        for version in iter::successors(Some(AlpenSpecId::V0), |version| version.successor().ok()) {
            let expected = match version {
                // Prague: Berlin modexp and no P256VERIFY.
                AlpenSpecId::V0 => VersionPrecompiles {
                    modexp_floor: 200,
                    caps_modexp_input: false,
                    has_p256verify: false,
                },
                // Osaka: EIP-7883 raises the modexp floor, EIP-7823 caps its inputs, and
                // EIP-7951 adds P256VERIFY. P256VERIFY needs the chunk guest's SP1 patch for
                // `p256`, or a call costs far more to prove than its gas suggests. V2 stays on
                // Osaka.
                AlpenSpecId::V1 | AlpenSpecId::V2 => VersionPrecompiles {
                    modexp_floor: 500,
                    caps_modexp_input: true,
                    has_p256verify: true,
                },
            };

            let (mut evm, precompiles) = evm_and_precompiles(version);

            let mut expected_addresses = shared_addresses.clone();
            if expected.has_p256verify {
                expected_addresses.insert(P256VERIFY_ADDRESS);
            }
            let addresses: BTreeSet<Address> = precompiles.as_map().addresses().collect();
            assert_eq!(addresses, expected_addresses, "{version:?}");

            assert_eq!(
                call(&mut evm, &precompiles, MODEXP_ADDRESS, &[]).0,
                expected.modexp_floor,
                "{version:?}"
            );
            assert_eq!(
                call(&mut evm, &precompiles, MODEXP_ADDRESS, &oversized_modexp)
                    .1
                    .err()
                    .map(|err| match err {
                        PrecompileError::Halt(halt) => halt,
                        err => panic!("unexpected precompile error: {err}"),
                    }),
                expected
                    .caps_modexp_input
                    .then_some(PrecompileHalt::ModexpEip7823LimitSize),
                "{version:?}"
            );

            if expected.has_p256verify {
                // Osaka prices a call at 6900 gas, twice what RIP-7212 charged.
                let (gas, output) = call(
                    &mut evm,
                    &precompiles,
                    P256VERIFY_ADDRESS,
                    &VALID_P256_SIGNATURE,
                );
                assert_eq!(gas, 6900, "{version:?}");
                assert_eq!(
                    output.unwrap().bytes(),
                    B256::with_last_byte(1).as_slice(),
                    "{version:?}"
                );
            }
        }
    }
}
