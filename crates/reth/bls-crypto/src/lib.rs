//! BLS12-381 backend for revm's EIP-2537 precompiles.
//!
//! revm computes the seven BLS12-381 precompiles through its [`Crypto`]
//! trait, and its default backend is arkworks. This crate implements the
//! same seven operations over the zkcrypto [`bls12_381`] crate, so that the
//! SP1 guest can replace that crate with `sp1-patches/bls12_381` through
//! `[patch.crates-io]` and run the field arithmetic on SP1 syscalls. Outside
//! the zkVM the crate is plain Rust.
//!
//! The node and the guest both execute with this backend. It still has to
//! match revm's default backend bit for bit, on valid and invalid input
//! alike, because blocks from before the switch were executed with the
//! default backend and are replayed with this one. Where zkcrypto gets an
//! input wrong, this crate returns the default backend's result instead (see
//! `MAP_FP2_TO_G2_OF_ZERO`). `tests/differential.rs` checks the agreement.
//!
//! [`install`] registers the backend with evm2 and the legacy REVM test path. It must run before
//! the first precompile call of the process; revm keeps the first backend
//! installed.
//!
//! Encodings follow EIP-2537: field elements are 48-byte big-endian and
//! must be canonical, `Fp2` elements are `c0 || c1`, the point at infinity
//! is all zeros, and scalars are 32-byte big-endian integers reduced modulo
//! the group order. Points are checked to be on the curve everywhere, and in
//! the subgroup for multi-scalar multiplication and pairing, the way revm's
//! default backend does.

use bls12_381::{
    hash_to_curve::MapToCurve, multi_miller_loop, G1Affine, G1Projective, G2Affine, G2Prepared,
    G2Projective, Gt, Scalar,
};

/// The base field. The crate keeps its field modules private; the
/// hash-to-curve trait is the public path to the types.
type Fp = <G1Projective as MapToCurve>::Field;
/// The quadratic extension field.
type Fp2 = <G2Projective as MapToCurve>::Field;
use revm::precompile::{
    bls12_381::{G1Point, G1PointScalar, G2Point, G2PointScalar},
    bls12_381_const::{FP_LENGTH, G1_LENGTH, G2_LENGTH, SCALAR_LENGTH},
    install_crypto, Crypto, PrecompileHalt,
};

/// revm [`Crypto`] backend computing the BLS12-381 precompiles with the
/// zkcrypto `bls12_381` crate. Every other primitive keeps revm's default.
#[derive(Clone, Copy, Debug, Default)]
pub struct Bls12381Crypto;

/// Installs [`Bls12381Crypto`] as the process's revm crypto backend.
///
/// Returns `false` when a backend was already installed, in which case the
/// earlier one stays. Call it at program start, before any EVM execution.
pub fn install() -> bool {
    let installed = evm2::install_crypto(Bls12381Crypto);
    install_crypto(Bls12381Crypto);
    installed
}

impl Crypto for Bls12381Crypto {
    fn bls12_381_g1_add(&self, a: G1Point, b: G1Point) -> Result<[u8; 96], PrecompileHalt> {
        let a = read_g1(&a, false)?;
        let b = read_g1(&b, false)?;
        Ok(encode_g1(&G1Affine::from(G1Projective::from(a) + b)))
    }

    fn bls12_381_g1_msm(
        &self,
        pairs: &mut dyn Iterator<Item = Result<G1PointScalar, PrecompileHalt>>,
    ) -> Result<[u8; 96], PrecompileHalt> {
        let mut acc = G1Projective::identity();
        for pair in pairs {
            let (point, scalar) = pair?;
            let point = read_g1(&point, true)?;
            if let Some(scalar) = read_scalar(&scalar) {
                acc += g1_mul(&point, &scalar);
            }
        }
        Ok(encode_g1(&G1Affine::from(acc)))
    }

    fn bls12_381_g2_add(&self, a: G2Point, b: G2Point) -> Result<[u8; 192], PrecompileHalt> {
        let a = read_g2(&a, false)?;
        let b = read_g2(&b, false)?;
        Ok(encode_g2(&G2Affine::from(G2Projective::from(a) + b)))
    }

    fn bls12_381_g2_msm(
        &self,
        pairs: &mut dyn Iterator<Item = Result<G2PointScalar, PrecompileHalt>>,
    ) -> Result<[u8; 192], PrecompileHalt> {
        let mut acc = G2Projective::identity();
        for pair in pairs {
            let (point, scalar) = pair?;
            let point = read_g2(&point, true)?;
            if let Some(scalar) = read_scalar(&scalar) {
                acc += point * scalar;
            }
        }
        Ok(encode_g2(&G2Affine::from(acc)))
    }

    fn bls12_381_pairing_check(
        &self,
        pairs: &[(G1Point, G2Point)],
    ) -> Result<bool, PrecompileHalt> {
        let mut terms = Vec::with_capacity(pairs.len());
        for (g1, g2) in pairs {
            // Every point is validated; a pair with an identity contributes
            // the unit to the product and is left out of the Miller loop.
            let g1 = read_g1(g1, true)?;
            let g2 = read_g2(g2, true)?;
            if bool::from(g1.is_identity() | g2.is_identity()) {
                continue;
            }
            terms.push((g1, G2Prepared::from(g2)));
        }
        if terms.is_empty() {
            return Ok(true);
        }
        let terms: Vec<(&G1Affine, &G2Prepared)> = terms.iter().map(|(a, b)| (a, b)).collect();
        Ok(multi_miller_loop(&terms).final_exponentiation() == Gt::identity())
    }

    fn bls12_381_fp_to_g1(&self, fp: &[u8; 48]) -> Result<[u8; 96], PrecompileHalt> {
        let fp = read_fp(fp)?;
        let point = G1Projective::map_to_curve(&fp).clear_h();
        Ok(encode_g1(&G1Affine::from(point)))
    }

    fn bls12_381_fp2_to_g2(&self, fp2: ([u8; 48], [u8; 48])) -> Result<[u8; 192], PrecompileHalt> {
        let fp2 = read_fp2(&fp2.0, &fp2.1)?;
        if bool::from(fp2.is_zero()) {
            return Ok(MAP_FP2_TO_G2_OF_ZERO);
        }
        let point = G2Projective::map_to_curve(&fp2).clear_h();
        Ok(encode_g2(&G2Affine::from(point)))
    }
}

// Both engines use the same point representation. Keep the proven arithmetic shared.
fn evm2_halt(error: PrecompileHalt) -> evm2::PrecompileHalt {
    use evm2::PrecompileHalt as Out;
    match error {
        PrecompileHalt::Bls12381G1NotInSubgroup => Out::Bls12381G1NotInSubgroup,
        PrecompileHalt::Bls12381G1NotOnCurve => Out::Bls12381G1NotOnCurve,
        PrecompileHalt::Bls12381G2NotInSubgroup => Out::Bls12381G2NotInSubgroup,
        PrecompileHalt::Bls12381G2NotOnCurve => Out::Bls12381G2NotOnCurve,
        _ => Out::NonCanonicalFp,
    }
}

impl evm2::Crypto for Bls12381Crypto {
    fn bls12_381_g1_add(&self, a: G1Point, b: G1Point) -> Result<[u8; 96], evm2::PrecompileHalt> {
        Crypto::bls12_381_g1_add(self, a, b).map_err(evm2_halt)
    }

    fn bls12_381_g1_msm(
        &self,
        pairs: &mut dyn Iterator<Item = Result<G1PointScalar, evm2::PrecompileHalt>>,
    ) -> Result<[u8; 96], evm2::PrecompileHalt> {
        let mut error = None;
        let result = Crypto::bls12_381_g1_msm(
            self,
            &mut pairs.map(|pair| {
                pair.map_err(|halt| {
                    error = Some(halt);
                    PrecompileHalt::NonCanonicalFp
                })
            }),
        );
        result.map_err(|halt| error.unwrap_or_else(|| evm2_halt(halt)))
    }

    fn bls12_381_g2_add(&self, a: G2Point, b: G2Point) -> Result<[u8; 192], evm2::PrecompileHalt> {
        Crypto::bls12_381_g2_add(self, a, b).map_err(evm2_halt)
    }

    fn bls12_381_g2_msm(
        &self,
        pairs: &mut dyn Iterator<Item = Result<G2PointScalar, evm2::PrecompileHalt>>,
    ) -> Result<[u8; 192], evm2::PrecompileHalt> {
        let mut error = None;
        let result = Crypto::bls12_381_g2_msm(
            self,
            &mut pairs.map(|pair| {
                pair.map_err(|halt| {
                    error = Some(halt);
                    PrecompileHalt::NonCanonicalFp
                })
            }),
        );
        result.map_err(|halt| error.unwrap_or_else(|| evm2_halt(halt)))
    }

    fn bls12_381_pairing_check(
        &self,
        pairs: &[(G1Point, G2Point)],
    ) -> Result<bool, evm2::PrecompileHalt> {
        Crypto::bls12_381_pairing_check(self, pairs).map_err(evm2_halt)
    }

    fn bls12_381_fp_to_g1(&self, fp: &[u8; 48]) -> Result<[u8; 96], evm2::PrecompileHalt> {
        Crypto::bls12_381_fp_to_g1(self, fp).map_err(evm2_halt)
    }

    fn bls12_381_fp2_to_g2(
        &self,
        fp2: ([u8; 48], [u8; 48]),
    ) -> Result<[u8; 192], evm2::PrecompileHalt> {
        Crypto::bls12_381_fp2_to_g2(self, fp2).map_err(evm2_halt)
    }
}

/// `MAP_FP2_TO_G2(0)`, encoded as [`encode_g2`] does.
///
/// zkcrypto's simplified SWU map for G2 gets this one input wrong, in every
/// release up to 0.9.2 and in the SP1 fork. At `u = 0` both `g(x1)` and the
/// square-root candidate for it are zero, so the check that picks `x1` passes,
/// `x` comes out as zero, and the cleared point is the identity. For any other
/// `u`, exactly one of `g(x0)` and `g(x1)` is a square, so the map is right.
/// The value here is the point revm's default backend returns, which is the
/// one the chain recorded for the calls that sent zero.
const MAP_FP2_TO_G2_OF_ZERO: [u8; G2_LENGTH] = hex_literal(concat!(
    "018320896ec9eef9d5e619848dc29ce266f413d02dd31d9b9d44ec0c79cd61f18b075ddba6d7bd20b7ff27a4b324bfce",
    "0a67d12118b5a35bb02d2e86b3ebfa7e23410db93de39fb06d7025fa95e96ffa428a7a27c3ae4dd4b40bd251ac658892",
    "0260e03644d1a2c321256b3246bad2b895cad13890cbe6f85df55106a0d334604fb143c7a042d878006271865bc35941",
    "04c69777a43f0bda07679d5805e63f18cf4e0e7c6112ac7f70266d199b4f76ae27c6269a3ceebdae30806e9a76aadf5c",
));

/// Decodes a hex string of exactly `N` bytes at compile time.
const fn hex_literal<const N: usize>(hex: &str) -> [u8; N] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("lowercase hex expected"),
        }
    }
    let hex = hex.as_bytes();
    assert!(hex.len() == 2 * N, "hex length");
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = (nibble(hex[2 * i]) << 4) | nibble(hex[2 * i + 1]);
        i += 1;
    }
    out
}

/// Decodes a canonical big-endian field element.
fn read_fp(bytes: &[u8; FP_LENGTH]) -> Result<Fp, PrecompileHalt> {
    Option::from(Fp::from_bytes(bytes)).ok_or(PrecompileHalt::NonCanonicalFp)
}

/// Decodes an `Fp2` element given as `c0 || c1`.
fn read_fp2(c0: &[u8; FP_LENGTH], c1: &[u8; FP_LENGTH]) -> Result<Fp2, PrecompileHalt> {
    Ok(Fp2 {
        c0: read_fp(c0)?,
        c1: read_fp(c1)?,
    })
}

/// Decodes a G1 point: canonical coordinates, all zeros for infinity, on
/// the curve, and in the subgroup when `subgroup` is set.
fn read_g1((x, y): &G1Point, subgroup: bool) -> Result<G1Affine, PrecompileHalt> {
    read_fp(x)?;
    read_fp(y)?;
    if is_zero(x) && is_zero(y) {
        return Ok(G1Affine::identity());
    }
    // The coordinates are canonical, so their flag bits are clear and the
    // uncompressed decoding below cannot fail.
    let mut encoded = [0u8; G1_LENGTH];
    encoded[..FP_LENGTH].copy_from_slice(x);
    encoded[FP_LENGTH..].copy_from_slice(y);
    let point: G1Affine = Option::from(G1Affine::from_uncompressed_unchecked(&encoded))
        .expect("canonical coordinates decode");
    if !bool::from(point.is_on_curve()) {
        return Err(PrecompileHalt::Bls12381G1NotOnCurve);
    }
    if subgroup && !bool::from(point.is_torsion_free()) {
        return Err(PrecompileHalt::Bls12381G1NotInSubgroup);
    }
    Ok(point)
}

/// Decodes a G2 point given as `x.c0 || x.c1 || y.c0 || y.c1`, with the
/// same checks as [`read_g1`].
fn read_g2((x0, x1, y0, y1): &G2Point, subgroup: bool) -> Result<G2Affine, PrecompileHalt> {
    for coordinate in [x0, x1, y0, y1] {
        read_fp(coordinate)?;
    }
    if is_zero(x0) && is_zero(x1) && is_zero(y0) && is_zero(y1) {
        return Ok(G2Affine::identity());
    }
    // zkcrypto's uncompressed encoding puts `c1` before `c0`.
    let mut encoded = [0u8; G2_LENGTH];
    encoded[..FP_LENGTH].copy_from_slice(x1);
    encoded[FP_LENGTH..2 * FP_LENGTH].copy_from_slice(x0);
    encoded[2 * FP_LENGTH..3 * FP_LENGTH].copy_from_slice(y1);
    encoded[3 * FP_LENGTH..].copy_from_slice(y0);
    let point: G2Affine = Option::from(G2Affine::from_uncompressed_unchecked(&encoded))
        .expect("canonical coordinates decode");
    if !bool::from(point.is_on_curve()) {
        return Err(PrecompileHalt::Bls12381G2NotOnCurve);
    }
    if subgroup && !bool::from(point.is_torsion_free()) {
        return Err(PrecompileHalt::Bls12381G2NotInSubgroup);
    }
    Ok(point)
}

/// `scalar * point`.
///
/// Outside the zkVM this is the crate's projective double-and-add. Inside
/// SP1 the patched crate exposes whole-point affine addition and doubling
/// as syscalls, and a double-and-add over those costs one syscall per step
/// instead of a dozen field syscalls per projective operation. The scalar
/// is public input, so the variable-time scan over its bits is fine.
#[cfg(not(target_os = "zkvm"))]
fn g1_mul(point: &G1Affine, scalar: &Scalar) -> G1Projective {
    point * scalar
}

#[cfg(target_os = "zkvm")]
fn g1_mul(point: &G1Affine, scalar: &Scalar) -> G1Projective {
    let bytes = scalar.to_bytes(); // little-endian
    let mut acc = G1Affine::identity();
    let mut started = false;
    for bit in (0..256).rev() {
        if started {
            acc = acc.double();
        }
        if (bytes[bit / 8] >> (bit % 8)) & 1 == 1 {
            acc = acc.add_affine(point);
            started = true;
        }
    }
    G1Projective::from(acc)
}

/// Reduces a 32-byte big-endian integer modulo the group order. `None` for
/// zero, which contributes nothing to a multi-scalar multiplication.
fn read_scalar(bytes: &[u8; SCALAR_LENGTH]) -> Option<Scalar> {
    if is_zero(bytes) {
        return None;
    }
    let mut wide = [0u8; 64];
    for (le, be) in wide[..SCALAR_LENGTH].iter_mut().zip(bytes.iter().rev()) {
        *le = *be;
    }
    Some(Scalar::from_bytes_wide(&wide))
}

/// Encodes a G1 point as `x || y`, or all zeros for infinity.
fn encode_g1(point: &G1Affine) -> [u8; G1_LENGTH] {
    if bool::from(point.is_identity()) {
        return [0u8; G1_LENGTH];
    }
    point.to_uncompressed()
}

/// Encodes a G2 point as `x.c0 || x.c1 || y.c0 || y.c1`, or all zeros for
/// infinity.
fn encode_g2(point: &G2Affine) -> [u8; G2_LENGTH] {
    let mut out = [0u8; G2_LENGTH];
    if bool::from(point.is_identity()) {
        return out;
    }
    let encoded = point.to_uncompressed();
    out[..FP_LENGTH].copy_from_slice(&encoded[FP_LENGTH..2 * FP_LENGTH]);
    out[FP_LENGTH..2 * FP_LENGTH].copy_from_slice(&encoded[..FP_LENGTH]);
    out[2 * FP_LENGTH..3 * FP_LENGTH].copy_from_slice(&encoded[3 * FP_LENGTH..]);
    out[3 * FP_LENGTH..].copy_from_slice(&encoded[2 * FP_LENGTH..3 * FP_LENGTH]);
    out
}

fn is_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests;
