//! BLS12-381 backend for revm's EIP-2537 precompiles.
//!
//! revm computes the seven BLS12-381 precompiles through its [`Crypto`]
//! trait, and its default backend is arkworks. This crate implements the
//! same seven operations over the zkcrypto [`bls12_381`] crate, so that the
//! SP1 guest can replace that crate with `sp1-patches/bls12_381` through
//! `[patch.crates-io]` and run the field arithmetic on SP1 syscalls. Outside
//! the zkVM the crate is plain Rust and the backend is simply another
//! correct implementation of EIP-2537.
//!
//! [`install`] registers the backend with revm. It must run before the
//! first precompile call of the process; revm keeps the first backend
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
    install_crypto(Bls12381Crypto)
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
        let point = G2Projective::map_to_curve(&fp2).clear_h();
        Ok(encode_g2(&G2Affine::from(point)))
    }
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
