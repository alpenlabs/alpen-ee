use reth_evm::precompiles::{DynPrecompile, PrecompilesMap};
use revm::precompile::{kzg_point_evaluation, PrecompileId, PrecompileSpecId, Precompiles};
use revm_primitives::{hardfork::SpecId, Address};
use strata_bridge_params::BridgeParams;

use crate::constants::{
    BRIDGEOUT_PRECOMPILE_ADDRESS, BRIDGEOUT_PRECOMPILE_ID, SCHNORR_PRECOMPILE_ADDRESS,
};

mod bridge;
mod schnorr;

/// Ethereum precompiles that Alpen leaves out of every fork.
///
/// KZG point evaluation checks blob commitments, and Alpen rejects blob transactions.
const EXCLUDED_ETHEREUM_PRECOMPILES: [Address; 1] = [kzg_point_evaluation::ADDRESS];

/// Creates the precompiles of a new EVM running the Ethereum fork `spec`.
///
/// Every fork gets the same treatment: Ethereum's precompiles for that fork, minus
/// `EXCLUDED_ETHEREUM_PRECOMPILES`, plus Alpen's own. A version that moves to a new fork picks
/// up that fork's new precompiles on its own. Each one still needs its proving cost checked in
/// the chunk guest, which is why the tests list every version's precompiles.
///
/// The set is picked by fork because that is the only part of the Alpen spec version that
/// reaches EVM creation (see [`AlpenEvmFactory`](crate::evm::AlpenEvmFactory)).
pub fn create_precompiles_map(spec: SpecId, bridge_params: BridgeParams) -> PrecompilesMap {
    let mut precompiles =
        PrecompilesMap::from_static(Precompiles::new(PrecompileSpecId::from_spec_id(spec)));

    for address in EXCLUDED_ETHEREUM_PRECOMPILES {
        precompiles.apply_precompile(&address, |_| None);
    }

    precompiles.extend_precompiles([
        (
            SCHNORR_PRECOMPILE_ADDRESS,
            schnorr::schnorr_signature_validation(),
        ),
        // Bridge-out is stateful, and the closure captures the withdrawal params it validates
        // amounts against.
        (
            BRIDGEOUT_PRECOMPILE_ADDRESS,
            DynPrecompile::new_stateful(
                PrecompileId::custom(BRIDGEOUT_PRECOMPILE_ID),
                move |input| bridge::bridge_context_call(input, bridge_params),
            ),
        ),
    ]);

    precompiles
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, iter};

    use alloy_consensus::Header;
    use alpen_params::{AlpenParams, AlpenSpecId, DEV_PARAMS_JSON};
    use reth_evm::{
        precompiles::{Precompile as _, PrecompileInput},
        ConfigureEvm, Evm, EvmInternals,
    };
    use revm::{
        context::{BlockEnv, CfgEnv, Journal, JournalEntry, JournalTr, TxEnv},
        database::EmptyDB,
        precompile::{u64_to_address, PrecompileHalt, PrecompileOutput},
    };
    use revm_primitives::{hex, Address, B256, U256};

    use super::*;
    use crate::config::AlpenEvmConfig;

    const MODEXP_ADDRESS: Address = u64_to_address(0x05);
    const P256VERIFY_ADDRESS: Address = u64_to_address(0x100);

    /// A valid P256VERIFY input from revm's test vectors: message hash, r, s, x, y.
    const VALID_P256_SIGNATURE: [u8; 160] = hex!("4cee90eb86eaa050036147a12d49004b6b9c72bd725d39d4785011fe190f0b4da73bd4903f0ce3b639bbbf6e8e80d16931ff4bcf5993d58468e8fb19086e8cac36dbcd03009df8c59286b162af3bd7fcc0450c9aa81be5d10d312af6c66b1d604aebd3099c618202fcfe16ae7770b0c49ab5eadf74b754204a3bb6060e44eff37618b065f9832de4ca6ca971a7a1adc826d0f7c00181a5fb2ddf79ae00b4e10e");

    /// What sets one version's precompiles apart from another's.
    struct VersionPrecompiles {
        /// The least gas a modexp call costs.
        modexp_floor: u64,
        /// Whether modexp refuses inputs over 1024 bytes.
        caps_modexp_input: bool,
        /// Whether P256VERIFY exists at 0x100.
        has_p256verify: bool,
    }

    /// Returns the precompiles of an EVM built under `version`, the way the node and the
    /// provers build it.
    fn precompiles_of(version: AlpenSpecId) -> PrecompilesMap {
        let params: AlpenParams =
            serde_json::from_str(DEV_PARAMS_JSON).expect("dev params should parse");
        let config = AlpenEvmConfig::new(&params, version);
        let evm_env = config
            .evm_env(&Header::default())
            .expect("evm_env is infallible");
        config
            .evm_with_env(EmptyDB::new(), evm_env)
            .precompiles()
            .clone()
    }

    /// Calls the precompile at `address` with enough gas for any input used here.
    fn call(precompiles: &PrecompilesMap, address: Address, data: &[u8]) -> PrecompileOutput {
        let mut journal: Journal<EmptyDB, JournalEntry> = Journal::new(EmptyDB::new());
        let block_env = BlockEnv::default();
        let cfg_env: CfgEnv = CfgEnv::default();
        let tx_env = TxEnv::default();
        let input = PrecompileInput {
            data,
            gas: 1_000_000,
            reservoir: 0,
            is_static: false,
            caller: Address::ZERO,
            value: U256::ZERO,
            target_address: address,
            bytecode_address: address,
            internals: EvmInternals::new(&mut journal, &block_env, &cfg_env, &tx_env),
        };

        precompiles
            .get(&address)
            .expect("the precompile exists")
            .call(input)
            .expect("the precompile has no fatal errors")
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
            .map(u64_to_address)
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
                // `p256`, or a call costs far more to prove than its gas suggests.
                AlpenSpecId::V1 => VersionPrecompiles {
                    modexp_floor: 500,
                    caps_modexp_input: true,
                    has_p256verify: true,
                },
            };

            let precompiles = precompiles_of(version);

            let mut expected_addresses = shared_addresses.clone();
            if expected.has_p256verify {
                expected_addresses.insert(P256VERIFY_ADDRESS);
            }
            let addresses: BTreeSet<Address> = precompiles.addresses().copied().collect();
            assert_eq!(addresses, expected_addresses, "{version:?}");

            assert_eq!(
                call(&precompiles, MODEXP_ADDRESS, &[]).gas_used,
                expected.modexp_floor,
                "{version:?}"
            );
            assert_eq!(
                call(&precompiles, MODEXP_ADDRESS, &oversized_modexp)
                    .halt_reason()
                    .cloned(),
                expected
                    .caps_modexp_input
                    .then_some(PrecompileHalt::ModexpEip7823LimitSize),
                "{version:?}"
            );

            if expected.has_p256verify {
                // Osaka prices a call at 6900 gas, twice what RIP-7212 charged.
                let output = call(&precompiles, P256VERIFY_ADDRESS, &VALID_P256_SIGNATURE);
                assert_eq!(output.gas_used, 6900, "{version:?}");
                assert_eq!(output.bytes[..], B256::with_last_byte(1)[..], "{version:?}");
            }
        }
    }
}
