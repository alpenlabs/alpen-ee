//! The precompile calls of the `precompiles` workload.
//!
//! One call to every precompile Alpen's EVM exposes, each with a fixed input
//! and the output it must return. The BLS12-381 calls use the official
//! EIP-2537 vectors, the ones
//! `functional-tests/tests/alpen_client/precompiles/test_bls_precompiles.py`
//! checks against the node. The others take inputs derived from the workload
//! seed, with the expected output computed by the same `revm` precompile
//! code the node runs, so the fixture pins the node's answer and the guest
//! must reproduce it.
//!
//! The workload sends each call through the checker contract, which reverts
//! unless the precompile returns the expected output. A prover that gets a
//! precompile wrong then fails the block's receipts root rather than proving
//! a wrong answer. The Schnorr precompile is called directly, as in the mixed
//! workload; the bridge-out precompile is a state change, not a computation,
//! and the mixed workload already covers it.

use alloy_primitives::{hex, keccak256, Address, Bytes, B256, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use eyre::{ensure, eyre, WrapErr};
use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature as P256Signature, SigningKey};
use revm::precompile::{blake2, bn254, hash, identity, modexp, secp256r1, EthPrecompileResult};

/// A precompile the workload calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PrecompileOp {
    EcRecover,
    Sha256,
    Ripemd160,
    Identity,
    ModExp,
    Bn254Add,
    Bn254Mul,
    Bn254Pairing,
    Blake2F,
    BlsG1Add,
    BlsG1Msm,
    BlsG2Add,
    BlsG2Msm,
    BlsPairingCheck,
    BlsMapFpToG1,
    BlsMapFp2ToG2,
    P256Verify,
}

/// Every precompile the workload calls, in address order.
pub(crate) const ALL: [PrecompileOp; 17] = [
    PrecompileOp::EcRecover,
    PrecompileOp::Sha256,
    PrecompileOp::Ripemd160,
    PrecompileOp::Identity,
    PrecompileOp::ModExp,
    PrecompileOp::Bn254Add,
    PrecompileOp::Bn254Mul,
    PrecompileOp::Bn254Pairing,
    PrecompileOp::Blake2F,
    PrecompileOp::BlsG1Add,
    PrecompileOp::BlsG1Msm,
    PrecompileOp::BlsG2Add,
    PrecompileOp::BlsG2Msm,
    PrecompileOp::BlsPairingCheck,
    PrecompileOp::BlsMapFpToG1,
    PrecompileOp::BlsMapFp2ToG2,
    PrecompileOp::P256Verify,
];

/// A precompile call: its calldata and the output it must return.
pub(crate) struct Call {
    pub input: Bytes,
    pub expected: Bytes,
}

impl PrecompileOp {
    /// Name the summary counts the calls under.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::EcRecover => "ecrecover",
            Self::Sha256 => "sha256",
            Self::Ripemd160 => "ripemd160",
            Self::Identity => "identity",
            Self::ModExp => "modexp",
            Self::Bn254Add => "bn254_add",
            Self::Bn254Mul => "bn254_mul",
            Self::Bn254Pairing => "bn254_pairing",
            Self::Blake2F => "blake2f",
            Self::BlsG1Add => "bls_g1_add",
            Self::BlsG1Msm => "bls_g1_msm",
            Self::BlsG2Add => "bls_g2_add",
            Self::BlsG2Msm => "bls_g2_msm",
            Self::BlsPairingCheck => "bls_pairing_check",
            Self::BlsMapFpToG1 => "bls_map_fp_to_g1",
            Self::BlsMapFp2ToG2 => "bls_map_fp2_to_g2",
            Self::P256Verify => "p256_verify",
        }
    }

    /// The precompile's address.
    pub(crate) fn address(self) -> Address {
        let index: u64 = match self {
            Self::EcRecover => 0x01,
            Self::Sha256 => 0x02,
            Self::Ripemd160 => 0x03,
            Self::Identity => 0x04,
            Self::ModExp => 0x05,
            Self::Bn254Add => 0x06,
            Self::Bn254Mul => 0x07,
            Self::Bn254Pairing => 0x08,
            Self::Blake2F => 0x09,
            Self::BlsG1Add => 0x0b,
            Self::BlsG1Msm => 0x0c,
            Self::BlsG2Add => 0x0d,
            Self::BlsG2Msm => 0x0e,
            Self::BlsPairingCheck => 0x0f,
            Self::BlsMapFpToG1 => 0x10,
            Self::BlsMapFp2ToG2 => 0x11,
            Self::P256Verify => 0x100,
        };
        Address::from_word(B256::from(U256::from(index).to_be_bytes()))
    }

    /// The call for a workload built from `seed`.
    pub(crate) fn call(self, seed: u64) -> eyre::Result<Call> {
        match self {
            Self::EcRecover => ecrecover(seed),
            Self::Sha256 => computed(derived(seed, b"sha256", 256), |input| {
                hash::sha256_run(input, u64::MAX)
            }),
            Self::Ripemd160 => computed(derived(seed, b"ripemd160", 128), |input| {
                hash::ripemd160_run(input, u64::MAX)
            }),
            Self::Identity => computed(derived(seed, b"identity", 512), |input| {
                identity::identity_run(input, u64::MAX)
            }),
            Self::ModExp => computed(modexp_input(seed), |input| {
                modexp::osaka_run(input, u64::MAX)
            }),
            Self::Bn254Add => computed(bn254_add_input(), |input| {
                bn254::run_add(input, bn254::add::ISTANBUL_ADD_GAS_COST, u64::MAX)
            }),
            Self::Bn254Mul => computed(bn254_mul_input(seed), |input| {
                bn254::run_mul(input, bn254::mul::ISTANBUL_MUL_GAS_COST, u64::MAX)
            }),
            Self::Bn254Pairing => {
                let call = computed(bn254_pairing_input(), |input| {
                    bn254::run_pair(
                        input,
                        bn254::pair::ISTANBUL_PAIR_PER_POINT,
                        bn254::pair::ISTANBUL_PAIR_BASE,
                        u64::MAX,
                    )
                })?;
                ensure!(
                    call.expected[..] == B256::with_last_byte(1)[..],
                    "the bn254 pairing input is not a valid pairing"
                );
                Ok(call)
            }
            Self::Blake2F => computed(blake2f_input(seed), |input| blake2::run(input, u64::MAX)),
            Self::BlsG1Add => vector(BLS12_G1ADD_INPUT, BLS12_G1ADD_OUTPUT),
            Self::BlsG1Msm => vector(BLS12_G1MSM_INPUT, BLS12_G1MSM_OUTPUT),
            Self::BlsG2Add => vector(BLS12_G2ADD_INPUT, BLS12_G2ADD_OUTPUT),
            Self::BlsG2Msm => vector(BLS12_G2MSM_INPUT, BLS12_G2MSM_OUTPUT),
            Self::BlsPairingCheck => vector(BLS12_PAIRING_CHECK_INPUT, BLS12_PAIRING_CHECK_OUTPUT),
            Self::BlsMapFpToG1 => vector(BLS12_MAP_FP_TO_G1_INPUT, BLS12_MAP_FP_TO_G1_OUTPUT),
            Self::BlsMapFp2ToG2 => vector(BLS12_MAP_FP2_TO_G2_INPUT, BLS12_MAP_FP2_TO_G2_OUTPUT),
            Self::P256Verify => p256_verify(seed),
        }
    }
}

/// A call with a fixed input whose expected output `run` computes.
fn computed(input: Vec<u8>, run: impl FnOnce(&[u8]) -> EthPrecompileResult) -> eyre::Result<Call> {
    let output = run(&input).map_err(|halt| eyre!("precompile halted: {halt:?}"))?;
    Ok(Call {
        input: input.into(),
        expected: output.bytes,
    })
}

/// A call from a published test vector.
fn vector(input: &str, expected: &str) -> eyre::Result<Call> {
    Ok(Call {
        input: hex::decode(input).wrap_err("vector input is hex")?.into(),
        expected: hex::decode(expected)
            .wrap_err("vector output is hex")?
            .into(),
    })
}

/// `len` bytes derived from `seed` under `domain`.
fn derived(seed: u64, domain: &[u8], len: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(len.next_multiple_of(32));
    for counter in 0u64.. {
        let mut preimage = b"alpen-evm-workload/precompiles/".to_vec();
        preimage.extend_from_slice(domain);
        preimage.extend_from_slice(&seed.to_be_bytes());
        preimage.extend_from_slice(&counter.to_be_bytes());
        bytes.extend_from_slice(keccak256(preimage).as_slice());
        if bytes.len() >= len {
            break;
        }
    }
    bytes.truncate(len);
    bytes
}

fn word(value: u64) -> [u8; 32] {
    U256::from(value).to_be_bytes()
}

/// Recovers the signer of a hash from a secp256k1 signature:
/// `hash || v || r || s` in, the signer's address padded to 32 bytes out.
fn ecrecover(seed: u64) -> eyre::Result<Call> {
    let key = B256::from_slice(&derived(seed, b"ecrecover/key", 32));
    let signer = PrivateKeySigner::from_bytes(&key).wrap_err("derived secp256k1 key")?;
    let message = keccak256(derived(seed, b"ecrecover/message", 64));
    let signature = signer
        .sign_hash_sync(&message)
        .wrap_err("sign with a local key")?;
    // `as_bytes` is `r || s || v`, with `v` already 27 or 28.
    let signature = signature.as_bytes();

    let mut input = message.to_vec();
    input.extend_from_slice(&word(u64::from(signature[64])));
    input.extend_from_slice(&signature[..64]);

    Ok(Call {
        input: input.into(),
        expected: B256::left_padding_from(signer.address().as_slice())
            .to_vec()
            .into(),
    })
}

/// An RSA-sized exponentiation, what signature checks in contracts do: a
/// 2048-bit base and modulus with the exponent 65537.
fn modexp_input(seed: u64) -> Vec<u8> {
    let base = derived(seed, b"modexp/base", 256);
    let mut modulus = derived(seed, b"modexp/modulus", 256);
    // Full-width and odd, as an RSA modulus is.
    modulus[0] |= 0x80;
    modulus[255] |= 0x01;

    let mut input = Vec::with_capacity(96 + 256 + 32 + 256);
    input.extend_from_slice(&word(256));
    input.extend_from_slice(&word(32));
    input.extend_from_slice(&word(256));
    input.extend_from_slice(&base);
    input.extend_from_slice(&word(65_537));
    input.extend_from_slice(&modulus);
    input
}

/// The BN254 G1 generator, `(1, 2)`.
fn bn254_g1() -> Vec<u8> {
    [word(1), word(2)].concat()
}

/// The BN254 G2 generator, as EIP-197 encodes it: each `F_p2` coordinate as
/// its imaginary part then its real part.
const BN254_G2: [&str; 4] = [
    "198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2",
    "1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed",
    "090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b",
    "12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa",
];

/// The y coordinate of the negated BN254 G1 generator, `p - 2`.
const BN254_NEG_G1_Y: &str = "30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd45";

/// `G1 + G1`.
fn bn254_add_input() -> Vec<u8> {
    [bn254_g1(), bn254_g1()].concat()
}

/// `G1 * k` for a seed-derived scalar below the group order.
fn bn254_mul_input(seed: u64) -> Vec<u8> {
    let mut scalar = derived(seed, b"bn254/scalar", 32);
    // The order is just above 2^253; clearing the top byte keeps `k` below it.
    scalar[0] = 0;
    let mut input = bn254_g1();
    input.extend_from_slice(&scalar);
    input
}

/// `e(G1, G2) * e(-G1, G2) == 1`, the two-pair check a signature
/// verification makes.
fn bn254_pairing_input() -> Vec<u8> {
    let g2: Vec<u8> = BN254_G2
        .iter()
        .flat_map(|coordinate| hex::decode(coordinate).expect("G2 generator is hex"))
        .collect();
    let mut input = bn254_g1();
    input.extend_from_slice(&g2);
    input.extend_from_slice(&word(1));
    input.extend_from_slice(&hex::decode(BN254_NEG_G1_Y).expect("negated y is hex"));
    input.extend_from_slice(&g2);
    input
}

/// One BLAKE2b compression with the standard 12 rounds over a full block:
/// `rounds || h || m || t || f`.
fn blake2f_input(seed: u64) -> Vec<u8> {
    let mut input = Vec::with_capacity(213);
    input.extend_from_slice(&12u32.to_be_bytes());
    input.extend_from_slice(&derived(seed, b"blake2f/h", 64));
    input.extend_from_slice(&derived(seed, b"blake2f/m", 128));
    input.extend_from_slice(&128u64.to_le_bytes());
    input.extend_from_slice(&0u64.to_le_bytes());
    input.push(1);
    input
}

/// Verifies a P-256 signature: `hash || r || s || x || y` in, a 32-byte 1
/// out.
fn p256_verify(seed: u64) -> eyre::Result<Call> {
    let key = derived(seed, b"p256/key", 32);
    let signer = SigningKey::from_slice(&key).wrap_err("derived P-256 key")?;
    let message = keccak256(derived(seed, b"p256/message", 64));
    let signature: P256Signature = signer
        .sign_prehash(message.as_slice())
        .wrap_err("sign with a P-256 key")?;
    let public_key = signer.verifying_key().to_encoded_point(false);

    let mut input = message.to_vec();
    input.extend_from_slice(&signature.to_bytes());
    input.extend_from_slice(public_key.x().ok_or_else(|| eyre!("P-256 key has no x"))?);
    input.extend_from_slice(public_key.y().ok_or_else(|| eyre!("P-256 key has no y"))?);

    let call = computed(input, |input| secp256r1::p256_verify_osaka(input, u64::MAX))?;
    ensure!(
        call.expected[..] == B256::with_last_byte(1)[..],
        "the P-256 signature does not verify"
    );
    Ok(call)
}

const BLS12_G1ADD_INPUT: &str = "0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb0000000000000000000000000000000008b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e100000000000000000000000000000000112b98340eee2777cc3c14163dea3ec97977ac3dc5c70da32e6e87578f44912e902ccef9efe28d4a78b8999dfbca942600000000000000000000000000000000186b28d92356c4dfec4b5201ad099dbdede3781f8998ddf929b4cd7756192185ca7b8f4ef7088f813270ac3d48868a21";
const BLS12_G1ADD_OUTPUT: &str = "000000000000000000000000000000000a40300ce2dec9888b60690e9a41d3004fda4886854573974fab73b046d3147ba5b7a5bde85279ffede1b45b3918d82d0000000000000000000000000000000006d3d887e9f53b9ec4eb6cedf5607226754b07c01ace7834f57f3e7315faefb739e59018e22c492006190fba4a870025";
const BLS12_G1MSM_INPUT: &str = "0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb0000000000000000000000000000000008b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e10000000000000000000000000000000000000000000000000000000000000002";
const BLS12_G1MSM_OUTPUT: &str = "000000000000000000000000000000000572cbea904d67468808c8eb50a9450c9721db309128012543902d0ac358a62ae28f75bb8f1c7c42c39a8c5529bf0f4e00000000000000000000000000000000166a9d8cabc673a322fda673779d8e3822ba3ecb8670e461f73bb9021d5fd76a4c56d9d4cd16bd1bba86881979749d28";
const BLS12_G2ADD_INPUT: &str = "00000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb80000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e000000000000000000000000000000000ce5d527727d6e118cc9cdc6da2e351aadfd9baa8cbdd3a76d429a695160d12c923ac9cc3baca289e193548608b82801000000000000000000000000000000000606c4a02ea734cc32acd2b02bc28b99cb3e287e85a763af267492ab572e99ab3f370d275cec1da1aaa9075ff05f79be00000000000000000000000000000000103121a2ceaae586d240843a398967325f8eb5a93e8fea99b62b9f88d8556c80dd726a4b30e84a36eeabaf3592937f2700000000000000000000000000000000086b990f3da2aeac0a36143b7d7c824428215140db1bb859338764cb58458f081d92664f9053b50b3fbd2e4723121b68000000000000000000000000000000000f9e7ba9a86a8f7624aa2b42dcc8772e1af4ae115685e60abc2c9b90242167acef3d0be4050bf935eed7c3b6fc7ba77e000000000000000000000000000000000d22c3652d0dc6f0fc9316e14268477c2049ef772e852108d269d9c38dba1d4802e8dae479818184c08f9a569d878451";
const BLS12_G2ADD_OUTPUT: &str = "000000000000000000000000000000000b54a8a7b08bd6827ed9a797de216b8c9057b3a9ca93e2f88e7f04f19accc42da90d883632b9ca4dc38d013f71ede4db00000000000000000000000000000000077eba4eecf0bd764dce8ed5f45040dd8f3b3427cb35230509482c14651713282946306247866dfe39a8e33016fcbe520000000000000000000000000000000014e60a76a29ef85cbd69f251b9f29147b67cfe3ed2823d3f9776b3a0efd2731941d47436dc6d2b58d9e65f8438bad073000000000000000000000000000000001586c3c910d95754fef7a732df78e279c3d37431c6a2b77e67a00c7c130a8fcd4d19f159cbeb997a178108fffffcbd20";
const BLS12_G2MSM_INPUT: &str = "00000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb80000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e000000000000000000000000000000000ce5d527727d6e118cc9cdc6da2e351aadfd9baa8cbdd3a76d429a695160d12c923ac9cc3baca289e193548608b82801000000000000000000000000000000000606c4a02ea734cc32acd2b02bc28b99cb3e287e85a763af267492ab572e99ab3f370d275cec1da1aaa9075ff05f79be0000000000000000000000000000000000000000000000000000000000000002";
const BLS12_G2MSM_OUTPUT: &str = "000000000000000000000000000000001638533957d540a9d2370f17cc7ed5863bc0b995b8825e0ee1ea1e1e4d00dbae81f14b0bf3611b78c952aacab827a053000000000000000000000000000000000a4edef9c1ed7f729f520e47730a124fd70662a904ba1074728114d1031e1572c6c886f6b57ec72a6178288c47c33577000000000000000000000000000000000468fb440d82b0630aeb8dca2b5256789a66da69bf91009cbfe6bd221e47aa8ae88dece9764bf3bd999d95d71e4c9899000000000000000000000000000000000f6d4552fa65dd2638b361543f887136a43253d9c66c411697003f7a13c308f5422e1aa0a59c8967acdefd8b6e36ccf3";
const BLS12_PAIRING_CHECK_INPUT: &str = "0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb0000000000000000000000000000000008b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e100000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb80000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e000000000000000000000000000000000ce5d527727d6e118cc9cdc6da2e351aadfd9baa8cbdd3a76d429a695160d12c923ac9cc3baca289e193548608b82801000000000000000000000000000000000606c4a02ea734cc32acd2b02bc28b99cb3e287e85a763af267492ab572e99ab3f370d275cec1da1aaa9075ff05f79be0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb0000000000000000000000000000000008b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e100000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb80000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e000000000000000000000000000000000d1b3cc2c7027888be51d9ef691d77bcb679afda66c73f17f9ee3837a55024f78c71363275a75d75d86bab79f74782aa0000000000000000000000000000000013fa4d4a0ad8b1ce186ed5061789213d993923066dddaf1040bc3ff59f825c78df74f2d75467e25e0f55f8a00fa030ed";
const BLS12_PAIRING_CHECK_OUTPUT: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";
const BLS12_MAP_FP_TO_G1_INPUT: &str = "00000000000000000000000000000000156c8a6a2c184569d69a76be144b5cdc5141d2d2ca4fe341f011e25e3969c55ad9e9b9ce2eb833c81a908e5fa4ac5f03";
const BLS12_MAP_FP_TO_G1_OUTPUT: &str = "00000000000000000000000000000000184bb665c37ff561a89ec2122dd343f20e0f4cbcaec84e3c3052ea81d1834e192c426074b02ed3dca4e7676ce4ce48ba0000000000000000000000000000000004407b8d35af4dacc809927071fc0405218f1401a6d15af775810e4e460064bcc9468beeba82fdc751be70476c888bf3";
const BLS12_MAP_FP2_TO_G2_INPUT: &str = "0000000000000000000000000000000007355d25caf6e7f2f0cb2812ca0e513bd026ed09dda65b177500fa31714e09ea0ded3a078b526bed3307f804d4b93b040000000000000000000000000000000002829ce3c021339ccb5caf3e187f6370e1e2a311dec9b75363117063ab2015603ff52c3d3b98f19c2f65575e99e8b78c";
const BLS12_MAP_FP2_TO_G2_OUTPUT: &str = "0000000000000000000000000000000000e7f4568a82b4b7dc1f14c6aaa055edf51502319c723c4dc2688c7fe5944c213f510328082396515734b6612c4e7bb700000000000000000000000000000000126b855e9e69b1f691f816e48ac6977664d24d99f8724868a184186469ddfd4617367e94527d4b74fc86413483afb35b000000000000000000000000000000000caead0fd7b6176c01436833c79d305c78be307da5f6af6c133c47311def6ff1e0babf57a0fb5539fce7ee12407b0a42000000000000000000000000000000001498aadcf7ae2b345243e281ae076df6de84455d766ab6fcdaad71fab60abb2e8b980a440043cd305db09d283c895e3d";
