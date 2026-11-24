//! The EIP-2537 vectors, and a comparison with revm's default backend.

use revm::precompile::{
    bls12_381::{G1Point, G2Point},
    DefaultCrypto,
};

use super::*;

/// Strips EIP-2537's 16-byte zero padding from each 64-byte field element,
/// which is what the precompile layer does before calling the backend.
/// Input already `raw` bytes long is returned as is.
fn unpad(bytes: &[u8], raw: usize) -> Vec<u8> {
    if bytes.len() == raw {
        return bytes.to_vec();
    }
    assert_eq!(bytes.len(), raw / 48 * 64, "padded or raw encoding");
    bytes
        .chunks(64)
        .flat_map(|chunk| {
            assert!(chunk[..16].iter().all(|&b| b == 0), "padding must be zero");
            chunk[16..].to_vec()
        })
        .collect()
}

/// Pads a backend output back to the vectors' 64-byte field encoding.
fn padded(bytes: &[u8]) -> String {
    let out: Vec<u8> = bytes
        .chunks(48)
        .flat_map(|chunk| [&[0u8; 16][..], chunk].concat())
        .collect();
    hex::encode(out)
}

fn fp(hex: &str) -> [u8; 48] {
    unpad(&hex::decode(hex).unwrap(), 48).try_into().unwrap()
}

fn g1(bytes: &[u8]) -> G1Point {
    let bytes = unpad(bytes, 96);
    (
        bytes[..48].try_into().unwrap(),
        bytes[48..96].try_into().unwrap(),
    )
}

fn g2(bytes: &[u8]) -> G2Point {
    let bytes = unpad(bytes, 192);
    (
        bytes[..48].try_into().unwrap(),
        bytes[48..96].try_into().unwrap(),
        bytes[96..144].try_into().unwrap(),
        bytes[144..192].try_into().unwrap(),
    )
}

fn scalar(bytes: &[u8]) -> [u8; 32] {
    bytes.try_into().unwrap()
}

/// The EIP-2537 generator points, `G1` then `G2`, as the test vectors
/// encode them.
const G1_GEN: &str = "0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb0000000000000000000000000000000008b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e1";
const G2_GEN: &str = "00000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb80000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e000000000000000000000000000000000ce5d527727d6e118cc9cdc6da2e351aadfd9baa8cbdd3a76d429a695160d12c923ac9cc3baca289e193548608b82801000000000000000000000000000000000606c4a02ea734cc32acd2b02bc28b99cb3e287e85a763af267492ab572e99ab3f370d275cec1da1aaa9075ff05f79be";

fn g1_gen() -> G1Point {
    g1(&hex::decode(G1_GEN).unwrap())
}

fn g2_gen() -> G2Point {
    g2(&hex::decode(G2_GEN).unwrap())
}

/// `n * G1` through the backend under test, as a point.
fn g1_mul(n: u64) -> G1Point {
    let mut s = [0u8; 32];
    s[24..].copy_from_slice(&n.to_be_bytes());
    let out = Bls12381Crypto
        .bls12_381_g1_msm(&mut [Ok((g1_gen(), s))].into_iter())
        .unwrap();
    g1(&out)
}

fn g2_mul(n: u64) -> G2Point {
    let mut s = [0u8; 32];
    s[24..].copy_from_slice(&n.to_be_bytes());
    let out = Bls12381Crypto
        .bls12_381_g2_msm(&mut [Ok((g2_gen(), s))].into_iter())
        .unwrap();
    g2(&out)
}

// The EIP-2537 vectors in functional-tests/.../test_bls_precompiles.py.

#[test]
fn vector_g1_add() {
    let input = hex::decode(format!("{G1_GEN}00000000000000000000000000000000112b98340eee2777cc3c14163dea3ec97977ac3dc5c70da32e6e87578f44912e902ccef9efe28d4a78b8999dfbca942600000000000000000000000000000000186b28d92356c4dfec4b5201ad099dbdede3781f8998ddf929b4cd7756192185ca7b8f4ef7088f813270ac3d48868a21")).unwrap();
    let out = Bls12381Crypto
        .bls12_381_g1_add(g1(&input[..128]), g1(&input[128..]))
        .unwrap();
    assert_eq!(padded(&out), "000000000000000000000000000000000a40300ce2dec9888b60690e9a41d3004fda4886854573974fab73b046d3147ba5b7a5bde85279ffede1b45b3918d82d0000000000000000000000000000000006d3d887e9f53b9ec4eb6cedf5607226754b07c01ace7834f57f3e7315faefb739e59018e22c492006190fba4a870025");
}

#[test]
fn vector_g1_msm() {
    let s = scalar(
        &hex::decode("0000000000000000000000000000000000000000000000000000000000000002").unwrap(),
    );
    let out = Bls12381Crypto
        .bls12_381_g1_msm(&mut [Ok((g1_gen(), s))].into_iter())
        .unwrap();
    assert_eq!(padded(&out), "000000000000000000000000000000000572cbea904d67468808c8eb50a9450c9721db309128012543902d0ac358a62ae28f75bb8f1c7c42c39a8c5529bf0f4e00000000000000000000000000000000166a9d8cabc673a322fda673779d8e3822ba3ecb8670e461f73bb9021d5fd76a4c56d9d4cd16bd1bba86881979749d28");
}

#[test]
fn vector_g2_add() {
    let b = hex::decode("00000000000000000000000000000000103121a2ceaae586d240843a398967325f8eb5a93e8fea99b62b9f88d8556c80dd726a4b30e84a36eeabaf3592937f2700000000000000000000000000000000086b990f3da2aeac0a36143b7d7c824428215140db1bb859338764cb58458f081d92664f9053b50b3fbd2e4723121b68000000000000000000000000000000000f9e7ba9a86a8f7624aa2b42dcc8772e1af4ae115685e60abc2c9b90242167acef3d0be4050bf935eed7c3b6fc7ba77e000000000000000000000000000000000d22c3652d0dc6f0fc9316e14268477c2049ef772e852108d269d9c38dba1d4802e8dae479818184c08f9a569d878451").unwrap();
    let out = Bls12381Crypto.bls12_381_g2_add(g2_gen(), g2(&b)).unwrap();
    assert_eq!(padded(&out), "000000000000000000000000000000000b54a8a7b08bd6827ed9a797de216b8c9057b3a9ca93e2f88e7f04f19accc42da90d883632b9ca4dc38d013f71ede4db00000000000000000000000000000000077eba4eecf0bd764dce8ed5f45040dd8f3b3427cb35230509482c14651713282946306247866dfe39a8e33016fcbe520000000000000000000000000000000014e60a76a29ef85cbd69f251b9f29147b67cfe3ed2823d3f9776b3a0efd2731941d47436dc6d2b58d9e65f8438bad073000000000000000000000000000000001586c3c910d95754fef7a732df78e279c3d37431c6a2b77e67a00c7c130a8fcd4d19f159cbeb997a178108fffffcbd20");
}

#[test]
fn vector_g2_msm() {
    let s = scalar(
        &hex::decode("0000000000000000000000000000000000000000000000000000000000000002").unwrap(),
    );
    let out = Bls12381Crypto
        .bls12_381_g2_msm(&mut [Ok((g2_gen(), s))].into_iter())
        .unwrap();
    assert_eq!(padded(&out), "000000000000000000000000000000001638533957d540a9d2370f17cc7ed5863bc0b995b8825e0ee1ea1e1e4d00dbae81f14b0bf3611b78c952aacab827a053000000000000000000000000000000000a4edef9c1ed7f729f520e47730a124fd70662a904ba1074728114d1031e1572c6c886f6b57ec72a6178288c47c33577000000000000000000000000000000000468fb440d82b0630aeb8dca2b5256789a66da69bf91009cbfe6bd221e47aa8ae88dece9764bf3bd999d95d71e4c9899000000000000000000000000000000000f6d4552fa65dd2638b361543f887136a43253d9c66c411697003f7a13c308f5422e1aa0a59c8967acdefd8b6e36ccf3");
}

#[test]
fn vector_pairing_check() {
    // e(G1, G2) * e(G1, -G2) == 1
    let neg_g2 = hex::decode("00000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb80000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e000000000000000000000000000000000d1b3cc2c7027888be51d9ef691d77bcb679afda66c73f17f9ee3837a55024f78c71363275a75d75d86bab79f74782aa0000000000000000000000000000000013fa4d4a0ad8b1ce186ed5061789213d993923066dddaf1040bc3ff59f825c78df74f2d75467e25e0f55f8a00fa030ed").unwrap();
    let pairs = [(g1_gen(), g2_gen()), (g1_gen(), g2(&neg_g2))];
    assert!(Bls12381Crypto.bls12_381_pairing_check(&pairs).unwrap());
    let pairs = [(g1_gen(), g2_gen()), (g1_gen(), g2_gen())];
    assert!(!Bls12381Crypto.bls12_381_pairing_check(&pairs).unwrap());
}

#[test]
fn vector_map_fp_to_g1() {
    let out = Bls12381Crypto
        .bls12_381_fp_to_g1(&fp("00000000000000000000000000000000156c8a6a2c184569d69a76be144b5cdc5141d2d2ca4fe341f011e25e3969c55ad9e9b9ce2eb833c81a908e5fa4ac5f03"))
        .unwrap();
    assert_eq!(padded(&out), "00000000000000000000000000000000184bb665c37ff561a89ec2122dd343f20e0f4cbcaec84e3c3052ea81d1834e192c426074b02ed3dca4e7676ce4ce48ba0000000000000000000000000000000004407b8d35af4dacc809927071fc0405218f1401a6d15af775810e4e460064bcc9468beeba82fdc751be70476c888bf3");
}

#[test]
fn vector_map_fp2_to_g2() {
    let out = Bls12381Crypto
        .bls12_381_fp2_to_g2((
            fp("0000000000000000000000000000000007355d25caf6e7f2f0cb2812ca0e513bd026ed09dda65b177500fa31714e09ea0ded3a078b526bed3307f804d4b93b04"),
            fp("0000000000000000000000000000000002829ce3c021339ccb5caf3e187f6370e1e2a311dec9b75363117063ab2015603ff52c3d3b98f19c2f65575e99e8b78c"),
        ))
        .unwrap();
    assert_eq!(padded(&out), "0000000000000000000000000000000000e7f4568a82b4b7dc1f14c6aaa055edf51502319c723c4dc2688c7fe5944c213f510328082396515734b6612c4e7bb700000000000000000000000000000000126b855e9e69b1f691f816e48ac6977664d24d99f8724868a184186469ddfd4617367e94527d4b74fc86413483afb35b000000000000000000000000000000000caead0fd7b6176c01436833c79d305c78be307da5f6af6c133c47311def6ff1e0babf57a0fb5539fce7ee12407b0a42000000000000000000000000000000001498aadcf7ae2b345243e281ae076df6de84455d766ab6fcdaad71fab60abb2e8b980a440043cd305db09d283c895e3d");
}

// Agreement with revm's default (arkworks) backend.

/// Deterministic 32-byte scalars, some above the group order.
fn scalars() -> Vec<[u8; 32]> {
    let mut out = Vec::new();
    for i in 0u64..24 {
        let mut s = [0u8; 32];
        let v = i
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .rotate_left(i as u32 % 64);
        s[..8].copy_from_slice(&v.to_be_bytes());
        s[8..16].copy_from_slice(&(v ^ i).to_be_bytes());
        s[24..].copy_from_slice(&(i + 1).to_be_bytes());
        if i % 3 == 0 {
            s[0] = 0xff; // above the order: must be reduced
        }
        out.push(s);
    }
    out.push([0u8; 32]);
    out
}

#[test]
fn matches_default_backend_g1() {
    let points: Vec<G1Point> = (1..=8).map(g1_mul).collect();
    for (i, a) in points.iter().enumerate() {
        let b = &points[(i * 5 + 3) % points.len()];
        assert_eq!(
            Bls12381Crypto.bls12_381_g1_add(*a, *b).unwrap(),
            DefaultCrypto.bls12_381_g1_add(*a, *b).unwrap()
        );
    }
    let pairs: Vec<_> = points
        .iter()
        .cycle()
        .zip(scalars())
        .map(|(p, s)| (*p, s))
        .collect();
    for n in [0, 1, 2, 7, pairs.len()] {
        let input: Vec<_> = pairs[..n].iter().map(|p| Ok(*p)).collect();
        assert_eq!(
            Bls12381Crypto
                .bls12_381_g1_msm(&mut input.clone().into_iter())
                .unwrap(),
            DefaultCrypto
                .bls12_381_g1_msm(&mut input.into_iter())
                .unwrap(),
            "msm of {n} pairs"
        );
    }
}

#[test]
fn matches_default_backend_g2() {
    let points: Vec<G2Point> = (1..=6).map(g2_mul).collect();
    for (i, a) in points.iter().enumerate() {
        let b = &points[(i * 5 + 3) % points.len()];
        assert_eq!(
            Bls12381Crypto.bls12_381_g2_add(*a, *b).unwrap(),
            DefaultCrypto.bls12_381_g2_add(*a, *b).unwrap()
        );
    }
    let pairs: Vec<_> = points
        .iter()
        .cycle()
        .zip(scalars())
        .map(|(p, s)| (*p, s))
        .collect();
    for n in [0, 1, 2, 5, pairs.len()] {
        let input: Vec<_> = pairs[..n].iter().map(|p| Ok(*p)).collect();
        assert_eq!(
            Bls12381Crypto
                .bls12_381_g2_msm(&mut input.clone().into_iter())
                .unwrap(),
            DefaultCrypto
                .bls12_381_g2_msm(&mut input.into_iter())
                .unwrap(),
            "msm of {n} pairs"
        );
    }
}

#[test]
fn matches_default_backend_pairing_and_maps() {
    let zero_g1: G1Point = ([0; 48], [0; 48]);
    let zero_g2: G2Point = ([0; 48], [0; 48], [0; 48], [0; 48]);
    // e(aG1, bG2) * e(-abG1, G2) == 1, with an identity pair in the middle.
    let mut neg = [0u8; 32];
    neg[..].copy_from_slice(
        &hex::decode("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000001").unwrap(),
    );
    // -(6) mod r = r - 6, with the borrow carried through the low limb.
    let low = u64::from_be_bytes(neg[24..].try_into().unwrap()) - 6;
    neg[24..].copy_from_slice(&low.to_be_bytes());
    let neg_six = g1(&Bls12381Crypto
        .bls12_381_g1_msm(&mut [Ok((g1_gen(), neg))].into_iter())
        .unwrap());
    let cases: Vec<Vec<(G1Point, G2Point)>> = vec![
        vec![],
        vec![(zero_g1, g2_gen())],
        vec![
            (g1_mul(2), g2_mul(3)),
            (zero_g1, zero_g2),
            (neg_six, g2_gen()),
        ],
        vec![(g1_mul(2), g2_mul(3)), (g1_mul(6), g2_gen())],
        vec![(g1_mul(4), g2_mul(5)), (g1_gen(), g2_gen())],
    ];
    for (i, pairs) in cases.iter().enumerate() {
        assert_eq!(
            Bls12381Crypto.bls12_381_pairing_check(pairs).unwrap(),
            DefaultCrypto.bls12_381_pairing_check(pairs).unwrap(),
            "case {i}"
        );
    }
    assert!(Bls12381Crypto.bls12_381_pairing_check(&cases[2]).unwrap());

    for i in 0u8..12 {
        let mut u = [0u8; 48];
        u[47] = i;
        u[1] = i.wrapping_mul(37);
        let mut v = [0u8; 48];
        v[46] = i.wrapping_add(9);
        assert_eq!(
            Bls12381Crypto.bls12_381_fp_to_g1(&u).unwrap(),
            DefaultCrypto.bls12_381_fp_to_g1(&u).unwrap()
        );
        assert_eq!(
            Bls12381Crypto.bls12_381_fp2_to_g2((u, v)).unwrap(),
            DefaultCrypto.bls12_381_fp2_to_g2((u, v)).unwrap()
        );
    }
}

/// Zero is the one input zkcrypto's G2 map gets wrong; see
/// [`MAP_FP2_TO_G2_OF_ZERO`]. Staging-v2 has V1 blocks that call the
/// precompile with it, so a node that returned the identity there could not
/// replay them.
#[test]
fn maps_zero_like_the_default_backend() {
    let zero = [0u8; 48];
    let mut one = [0u8; 48];
    one[47] = 1;
    for fp2 in [(zero, zero), (zero, one), (one, zero)] {
        assert_eq!(
            Bls12381Crypto.bls12_381_fp2_to_g2(fp2).unwrap(),
            DefaultCrypto.bls12_381_fp2_to_g2(fp2).unwrap()
        );
    }
    assert_eq!(
        Bls12381Crypto.bls12_381_fp_to_g1(&zero).unwrap(),
        DefaultCrypto.bls12_381_fp_to_g1(&zero).unwrap()
    );
}

/// The upstream map still returns the identity for zero, so
/// [`MAP_FP2_TO_G2_OF_ZERO`] stays needed. When this fails, upstream fixed the
/// map and the special case can go.
#[test]
fn upstream_still_maps_zero_to_identity() {
    let zero = read_fp2(&[0u8; 48], &[0u8; 48]).unwrap();
    let point = G2Affine::from(G2Projective::map_to_curve(&zero).clear_h());
    assert!(bool::from(point.is_identity()));
    assert_ne!(MAP_FP2_TO_G2_OF_ZERO, [0u8; G2_LENGTH]);
}

// Rejections, with the same halt reasons as the default backend.

fn halt<T>(r: Result<T, PrecompileHalt>) -> PrecompileHalt {
    r.err().expect("must be rejected")
}

#[test]
fn rejects_what_the_default_backend_rejects() {
    // A coordinate at or above the modulus.
    let mut big = [0u8; 48];
    big[0] = 0x1a;
    big[1] = 0x01;
    big[2] = 0x11;
    big[3] = 0xea;
    big[4] = 0x39;
    big[5] = 0x7f;
    big[6] = 0xe6;
    big[7] = 0x9a;
    big[8] = 0x4b;
    big[9] = 0x1b;
    big[10] = 0xa7;
    big[11] = 0xb6;
    big[12] = 0x43;
    big[13] = 0x4b;
    big[14] = 0xac;
    big[15] = 0xd7;
    big[16] = 0x64;
    big[17] = 0x77;
    big[18] = 0x4b;
    big[19] = 0x84;
    big[20] = 0xf3;
    big[21] = 0x85;
    big[22] = 0x12;
    big[23] = 0xbf;
    big[24] = 0x67;
    big[25] = 0x30;
    big[26] = 0xd2;
    big[27] = 0xa0;
    big[28] = 0xf6;
    big[29] = 0xb0;
    big[30] = 0xf6;
    big[31] = 0x24;
    big[32] = 0x1e;
    big[33] = 0xab;
    big[34] = 0xff;
    big[35] = 0xfe;
    big[36] = 0xb1;
    big[37] = 0x53;
    big[38] = 0xff;
    big[39] = 0xff;
    big[40] = 0xb9;
    big[41] = 0xfe;
    big[42] = 0xff;
    big[43] = 0xff;
    big[44] = 0xff;
    big[45] = 0xff;
    big[46] = 0xaa;
    big[47] = 0xab; // == p
    let (gx, gy) = g1_gen();
    assert_eq!(
        halt(Bls12381Crypto.bls12_381_g1_add((big, gy), g1_gen())),
        halt(DefaultCrypto.bls12_381_g1_add((big, gy), g1_gen()))
    );
    assert_eq!(
        halt(Bls12381Crypto.bls12_381_fp_to_g1(&big)),
        PrecompileHalt::NonCanonicalFp
    );
    let mut top = [0u8; 48];
    top[0] = 0x80; // flag bit set: above the modulus too
    assert_eq!(
        halt(Bls12381Crypto.bls12_381_g1_add((top, gy), g1_gen())),
        PrecompileHalt::NonCanonicalFp
    );

    // Not on the curve.
    let mut y = gy;
    y[47] ^= 1;
    assert_eq!(
        halt(Bls12381Crypto.bls12_381_g1_add((gx, y), g1_gen())),
        PrecompileHalt::Bls12381G1NotOnCurve
    );
    let (x0, x1, y0, mut y1) = g2_gen();
    y1[47] ^= 1;
    assert_eq!(
        halt(Bls12381Crypto.bls12_381_g2_add((x0, x1, y0, y1), g2_gen())),
        PrecompileHalt::Bls12381G2NotOnCurve
    );

    // On the curve but outside the subgroup: a mapped point before cofactor
    // clearing. Addition accepts it, MSM and pairing do not.
    let mut u = [0u8; 48];
    u[47] = 5;
    let raw = G1Affine::from(G1Projective::map_to_curve(&Fp::from_bytes(&u).unwrap()));
    assert!(
        !bool::from(raw.is_torsion_free()),
        "test point must be outside G1"
    );
    let raw = g1(&raw.to_uncompressed());
    assert!(Bls12381Crypto.bls12_381_g1_add(raw, g1_gen()).is_ok());
    assert_eq!(
        halt(Bls12381Crypto.bls12_381_g1_msm(&mut [Ok((raw, [1u8; 32]))].into_iter())),
        PrecompileHalt::Bls12381G1NotInSubgroup
    );
    assert_eq!(
        halt(Bls12381Crypto.bls12_381_pairing_check(&[(raw, g2_gen())])),
        PrecompileHalt::Bls12381G1NotInSubgroup
    );
    assert_eq!(
        halt(DefaultCrypto.bls12_381_pairing_check(&[(raw, g2_gen())])),
        PrecompileHalt::Bls12381G1NotInSubgroup
    );
    let raw2 = G2Affine::from(G2Projective::map_to_curve(&Fp2 {
        c0: Fp::from_bytes(&u).unwrap(),
        c1: Fp::zero(),
    }));
    assert!(
        !bool::from(raw2.is_torsion_free()),
        "test point must be outside G2"
    );
    let enc = raw2.to_uncompressed();
    let raw2: G2Point = (
        enc[48..96].try_into().unwrap(),
        enc[..48].try_into().unwrap(),
        enc[144..].try_into().unwrap(),
        enc[96..144].try_into().unwrap(),
    );
    assert!(Bls12381Crypto.bls12_381_g2_add(raw2, g2_gen()).is_ok());
    assert_eq!(
        halt(Bls12381Crypto.bls12_381_g2_msm(&mut [Ok((raw2, [1u8; 32]))].into_iter())),
        PrecompileHalt::Bls12381G2NotInSubgroup
    );
    // An identity pair still gets its partner validated.
    assert_eq!(
        halt(Bls12381Crypto.bls12_381_pairing_check(&[(([0; 48], [0; 48]), raw2)])),
        PrecompileHalt::Bls12381G2NotInSubgroup
    );
}
