//! Differential tests: [`Bls12381Crypto`] against revm's default backend.
//!
//! Replayed blocks were executed with the default backend, so for every input
//! both must either return the same bytes or both halt. The halt reason may
//! differ; it does not reach consensus.
//!
//! Inputs mix edge values (zero, one, p - 1, p, the identity, the generator,
//! points on the curve but outside the subgroup, off-curve points, scalars at
//! and above the group order) with random ones, so that each operation sees
//! valid and invalid input.

use std::fmt::Debug;

use alpen_reth_bls_crypto::Bls12381Crypto;
use bls12_381::{
    G1Affine, G1Projective, G2Affine, G2Projective, Scalar, hash_to_curve::MapToCurve,
};
use evm2 as _;
use hex as _;
use proptest::{collection::vec, prelude::*};
use revm::precompile::{
    Crypto, DefaultCrypto, PrecompileHalt,
    bls12_381::{G1Point, G2Point},
};
use serde_json as _;

type Fp = <G1Projective as MapToCurve>::Field;
type Fp2 = <G2Projective as MapToCurve>::Field;
type Bytes48 = [u8; 48];

/// The base field modulus, big-endian.
const P: Bytes48 = [
    0x1a, 0x01, 0x11, 0xea, 0x39, 0x7f, 0xe6, 0x9a, 0x4b, 0x1b, 0xa7, 0xb6, 0x43, 0x4b, 0xac, 0xd7,
    0x64, 0x77, 0x4b, 0x84, 0xf3, 0x85, 0x12, 0xbf, 0x67, 0x30, 0xd2, 0xa0, 0xf6, 0xb0, 0xf6, 0x24,
    0x1e, 0xab, 0xff, 0xfe, 0xb1, 0x53, 0xff, 0xff, 0xb9, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xaa, 0xab,
];

/// The group order, big-endian.
const R: [u8; 32] = [
    0x73, 0xed, 0xa7, 0x53, 0x29, 0x9d, 0x7d, 0x48, 0x33, 0x39, 0xd8, 0x08, 0x09, 0xa1, 0xd8, 0x05,
    0x53, 0xbd, 0xa4, 0x02, 0xff, 0xfe, 0x5b, 0xfe, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01,
];

fn sub_one(mut bytes: Bytes48) -> Bytes48 {
    for byte in bytes.iter_mut().rev() {
        let (next, borrow) = byte.overflowing_sub(1);
        *byte = next;
        if !borrow {
            break;
        }
    }
    bytes
}

/// Agrees when both return the same bytes or both halt.
fn same<T: PartialEq + Debug>(
    ours: Result<T, PrecompileHalt>,
    theirs: Result<T, PrecompileHalt>,
) -> Result<(), TestCaseError> {
    match (ours, theirs) {
        (Ok(a), Ok(b)) => prop_assert_eq!(a, b),
        (Err(_), Err(_)) => {}
        (ours, theirs) => {
            return Err(TestCaseError::fail(format!(
                "ours {ours:?}, default {theirs:?}"
            )));
        }
    }
    Ok(())
}

// ---- Field elements ----

/// Field-element encodings: edge values, random canonical values, and
/// arbitrary bytes (usually non-canonical).
fn fp_bytes() -> impl Strategy<Value = Bytes48> {
    prop_oneof![
        1 => Just([0u8; 48]),
        1 => Just(Fp::one().to_bytes()),
        1 => Just(sub_one(P)),
        1 => Just(P),
        1 => Just([0xff; 48]),
        // A top byte below 0x1a keeps the value below p.
        3 => vec(any::<u8>(), 48).prop_map(|b| {
            let mut out: Bytes48 = b.try_into().unwrap();
            out[0] %= 0x1a;
            out
        }),
        1 => any::<[u8; 32]>().prop_map(|tail| {
            let mut out = [0u8; 48];
            out[16..].copy_from_slice(&tail);
            out
        }),
        1 => vec(any::<u8>(), 48).prop_map(|b| b.try_into().unwrap()),
    ]
}

// ---- Points ----

fn g1_from_scalar(seed: [u8; 64]) -> G1Point {
    split_g1(
        G1Affine::from(G1Affine::generator() * Scalar::from_bytes_wide(&seed)).to_uncompressed(),
    )
}

fn g2_from_scalar(seed: [u8; 64]) -> G2Point {
    to_eip_g2(G2Affine::from(
        G2Affine::generator() * Scalar::from_bytes_wide(&seed),
    ))
}

fn split_g1(raw: [u8; 96]) -> G1Point {
    (raw[..48].try_into().unwrap(), raw[48..].try_into().unwrap())
}

/// zkcrypto encodes Fp2 as `c1 || c0`; EIP-2537 uses `c0 || c1`.
fn to_eip_g2(point: G2Affine) -> G2Point {
    if bool::from(point.is_identity()) {
        return ([0; 48], [0; 48], [0; 48], [0; 48]);
    }
    let raw = point.to_uncompressed();
    (
        raw[48..96].try_into().unwrap(),
        raw[..48].try_into().unwrap(),
        raw[144..].try_into().unwrap(),
        raw[96..144].try_into().unwrap(),
    )
}

fn wide_seed() -> impl Strategy<Value = [u8; 64]> {
    vec(any::<u8>(), 64).prop_map(|v| v.try_into().unwrap())
}

/// G1 points: identity, generator, subgroup points, points on the curve
/// outside the subgroup, and arbitrary coordinate pairs.
fn g1_point() -> impl Strategy<Value = G1Point> {
    prop_oneof![
        1 => Just(([0u8; 48], [0u8; 48])),
        1 => Just(split_g1(G1Affine::generator().to_uncompressed())),
        3 => wide_seed().prop_map(g1_from_scalar),
        2 => fp_bytes().prop_filter_map("u is non-canonical or maps to the identity", |u| {
            let u = Option::<Fp>::from(Fp::from_bytes(&u))?;
            let point = G1Affine::from(G1Projective::map_to_curve(&u));
            (!bool::from(point.is_identity())).then(|| split_g1(point.to_uncompressed()))
        }),
        1 => (fp_bytes(), fp_bytes()),
    ]
}

/// G2 points, built the same way as [`g1_point`].
fn g2_point() -> impl Strategy<Value = G2Point> {
    prop_oneof![
        1 => Just(([0u8; 48], [0u8; 48], [0u8; 48], [0u8; 48])),
        1 => Just(to_eip_g2(G2Affine::generator())),
        3 => wide_seed().prop_map(g2_from_scalar),
        2 => (fp_bytes(), fp_bytes()).prop_filter_map("u is non-canonical or maps to the identity", |(c0, c1)| {
            let c0 = Option::<Fp>::from(Fp::from_bytes(&c0))?;
            let c1 = Option::<Fp>::from(Fp::from_bytes(&c1))?;
            let point = G2Affine::from(G2Projective::map_to_curve(&Fp2 { c0, c1 }));
            (!bool::from(point.is_identity())).then(|| to_eip_g2(point))
        }),
        1 => (fp_bytes(), fp_bytes(), fp_bytes(), fp_bytes()),
    ]
}

/// Scalars: zero, one, r - 1, r, r + 1, all ones, and random.
fn scalar() -> impl Strategy<Value = [u8; 32]> {
    let mut r_minus_one = R;
    r_minus_one[31] -= 1;
    let mut r_plus_one = R;
    r_plus_one[31] += 1;
    prop_oneof![
        1 => Just([0u8; 32]),
        1 => Just({
            let mut one = [0u8; 32];
            one[31] = 1;
            one
        }),
        1 => Just(r_minus_one),
        1 => Just(R),
        1 => Just(r_plus_one),
        1 => Just([0xff; 32]),
        4 => any::<[u8; 32]>(),
    ]
}

fn neg_g1(point: G1Point) -> Option<G1Point> {
    let mut raw = [0u8; 96];
    raw[..48].copy_from_slice(&point.0);
    raw[48..].copy_from_slice(&point.1);
    let p = Option::<G1Affine>::from(G1Affine::from_uncompressed_unchecked(&raw))?;
    Some(split_g1((-p).to_uncompressed()))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn g1_add(a in g1_point(), b in g1_point()) {
        same(Bls12381Crypto.bls12_381_g1_add(a, b), DefaultCrypto.bls12_381_g1_add(a, b))?;
        // Doubling and P + (-P).
        same(Bls12381Crypto.bls12_381_g1_add(a, a), DefaultCrypto.bls12_381_g1_add(a, a))?;
        if let Some(neg) = neg_g1(a) {
            same(Bls12381Crypto.bls12_381_g1_add(a, neg), DefaultCrypto.bls12_381_g1_add(a, neg))?;
        }
    }

    #[test]
    fn g2_add(a in g2_point(), b in g2_point()) {
        same(Bls12381Crypto.bls12_381_g2_add(a, b), DefaultCrypto.bls12_381_g2_add(a, b))?;
        same(Bls12381Crypto.bls12_381_g2_add(a, a), DefaultCrypto.bls12_381_g2_add(a, a))?;
    }

    #[test]
    fn map_fp_to_g1(u in fp_bytes()) {
        same(Bls12381Crypto.bls12_381_fp_to_g1(&u), DefaultCrypto.bls12_381_fp_to_g1(&u))?;
    }

    #[test]
    fn map_fp2_to_g2(c0 in fp_bytes(), c1 in fp_bytes()) {
        same(
            Bls12381Crypto.bls12_381_fp2_to_g2((c0, c1)),
            DefaultCrypto.bls12_381_fp2_to_g2((c0, c1)),
        )?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn g1_msm(pairs in vec((g1_point(), scalar()), 1..4)) {
        let ours = Bls12381Crypto.bls12_381_g1_msm(&mut pairs.clone().into_iter().map(Ok));
        let theirs = DefaultCrypto.bls12_381_g1_msm(&mut pairs.into_iter().map(Ok));
        same(ours, theirs)?;
    }

    #[test]
    fn g2_msm(pairs in vec((g2_point(), scalar()), 1..4)) {
        let ours = Bls12381Crypto.bls12_381_g2_msm(&mut pairs.clone().into_iter().map(Ok));
        let theirs = DefaultCrypto.bls12_381_g2_msm(&mut pairs.into_iter().map(Ok));
        same(ours, theirs)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn pairing_check(pairs in vec((g1_point(), g2_point()), 1..3)) {
        same(
            Bls12381Crypto.bls12_381_pairing_check(&pairs),
            DefaultCrypto.bls12_381_pairing_check(&pairs),
        )?;
    }
}

/// A product of pairings that equals one, so the check passes only when the
/// backend computes it correctly: e(aG1, bG2) * e(-abG1, G2) = 1.
#[test]
fn pairing_check_true_case() {
    let a = Scalar::from(7u64);
    let b = Scalar::from(11u64);
    let p1 = split_g1(G1Affine::from(G1Affine::generator() * a).to_uncompressed());
    let q1 = to_eip_g2(G2Affine::from(G2Affine::generator() * b));
    let p2 = split_g1(G1Affine::from(-(G1Affine::generator() * (a * b))).to_uncompressed());
    let q2 = to_eip_g2(G2Affine::generator());
    let pairs = [(p1, q1), (p2, q2)];
    assert!(Bls12381Crypto.bls12_381_pairing_check(&pairs).unwrap());
    assert!(DefaultCrypto.bls12_381_pairing_check(&pairs).unwrap());
}
