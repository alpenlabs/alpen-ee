//! The versioned layout of the EE block header's [`Header::extra_data`]
//! field.
//!
//! The layout is a fixed prefix followed by a version-defined body. The
//! prefix is the governing spec version as a big-endian integer, exactly
//! [`AlpenSpecId`]-wide and the only version-independent part; the body that
//! follows is defined by that version's layout. [`AlpenSpecId`] is thus also
//! the version of the layout itself, so the two version spaces coincide.
//!
//! Every version defined so far carries one body field: the block's DA rate
//! in wei per byte, as a big-endian `u64`. The sequencer freezes the live
//! rate into it per block, and re-execution reads it back so the in-EVM DA
//! fee charge always sees the rate the block actually committed to.
//!
//! Decoding is strict: a header claiming a version this binary has no
//! variant for must fail rather than run under stale rules. Two inputs are
//! exempt, both standing for "no stamp was ever written":
//!
//! - Empty `extra_data` decodes as [`AlpenSpecId::V0`] with a zero DA rate. V0 is the pre-stamp
//!   state of the chain, so an unstamped header is a V0 header. Only a genuinely empty field
//!   qualifies — a short but non-empty one is a truncated or corrupt stamp and is rejected, since
//!   reading it as V0 would run a malformed block under default rules.
//! - The genesis header, whose `extra_data` is authored by the genesis document and predates the
//!   layout, is fixed at [`AlpenSpecId::V0`] whatever it holds.

use std::mem::size_of;

use alloy_consensus::{constants::MAXIMUM_EXTRA_DATA_SIZE, Header};
use thiserror::Error;

use crate::AlpenSpecId;

/// Length of the version-independent spec version prefix.
const SPEC_VERSION_LEN: usize = size_of::<AlpenSpecId>();

/// Length of the DA rate body field.
const DA_RATE_LEN: usize = size_of::<u64>();

/// Total length of the layout every version defines so far: the version
/// prefix followed by the DA rate.
const LAYOUT_LEN: usize = SPEC_VERSION_LEN + DA_RATE_LEN;

/// The decoded contents of a header's `extra_data`.
///
/// Carries the fields the chain commits in the header beyond the standard
/// EVM ones. Which fields exist is a per-version fact defined by this type's
/// codec; today every version carries only the governing spec version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderExtra {
    /// The spec version whose rules govern the block.
    spec_version: AlpenSpecId,
    /// The DA rate (wei per byte) the block charges under.
    da_rate: u64,
}

/// An `extra_data` value that does not decode under any layout this binary
/// knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum HeaderExtraError {
    /// Shorter than the version prefix, so no layout can even be selected.
    #[error("extra_data is {len} bytes, shorter than the spec version prefix")]
    TooShort {
        /// The rejected `extra_data` length.
        len: usize,
    },

    /// The version prefix names a spec version this binary has no variant
    /// for — newer software produced the block.
    #[error("no spec version with id {0} in this binary")]
    UnknownVersion(u16),

    /// The length does not match the named version's layout.
    #[error("extra_data is {len} bytes, but {version:?} defines a {expected}-byte layout")]
    WrongLength {
        /// The version whose layout was violated.
        version: AlpenSpecId,
        /// The layout length that version defines.
        expected: usize,
        /// The rejected `extra_data` length.
        len: usize,
    },
}

impl HeaderExtra {
    /// Creates the `extra_data` contents of a block governed by
    /// `spec_version` and charging `da_rate` wei per byte.
    pub fn new(spec_version: AlpenSpecId, da_rate: u64) -> Self {
        Self {
            spec_version,
            da_rate,
        }
    }

    /// Returns the governing spec version.
    pub fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }

    /// Returns the DA rate (wei per byte) the block charges under.
    pub fn da_rate(&self) -> u64 {
        self.da_rate
    }

    /// Encodes into the `extra_data` bytes under the version's layout.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = u16::from(self.spec_version).to_be_bytes().to_vec();
        match self.spec_version {
            AlpenSpecId::V0 | AlpenSpecId::V1 => {
                buf.extend_from_slice(&self.da_rate.to_be_bytes());
            }
        }
        // The whole layout must fit Ethereum's `extra_data` cap, else the
        // block can't round-trip through an engine payload.
        debug_assert!(
            buf.len() <= MAXIMUM_EXTRA_DATA_SIZE,
            "{:?} extra_data layout is {} bytes, over the {}-byte cap",
            self.spec_version,
            buf.len(),
            MAXIMUM_EXTRA_DATA_SIZE
        );
        buf
    }

    /// Decodes `extra_data` under the layout its version prefix names.
    ///
    /// The full strict parse — trailing or missing body bytes are a layout
    /// violation. Callers that only route by version can use the cheaper
    /// [`peek_spec_version`].
    pub fn decode(extra_data: &[u8]) -> Result<Self, HeaderExtraError> {
        if extra_data.is_empty() {
            return Ok(Self::new(AlpenSpecId::V0, 0));
        }
        let spec_version = peek_spec_version(extra_data)?;
        let body = &extra_data[SPEC_VERSION_LEN..];
        let da_rate = match spec_version {
            AlpenSpecId::V0 | AlpenSpecId::V1 => {
                let bytes: [u8; DA_RATE_LEN] =
                    body.try_into().map_err(|_| HeaderExtraError::WrongLength {
                        version: spec_version,
                        expected: LAYOUT_LEN,
                        len: extra_data.len(),
                    })?;
                u64::from_be_bytes(bytes)
            }
        };
        Ok(Self {
            spec_version,
            da_rate,
        })
    }
}

/// Reads the governing spec version from `extra_data`'s version prefix.
///
/// Only the prefix: the rest of the layout is not validated (that is
/// [`HeaderExtra::decode`]'s job, exercised by consensus header validation),
/// so version dispatch keeps working on fields a later layout adds.
pub fn peek_spec_version(extra_data: &[u8]) -> Result<AlpenSpecId, HeaderExtraError> {
    // An unstamped header is a V0 header; see the module docs.
    if extra_data.is_empty() {
        return Ok(AlpenSpecId::V0);
    }
    let prefix = extra_data
        .get(..SPEC_VERSION_LEN)
        .ok_or(HeaderExtraError::TooShort {
            len: extra_data.len(),
        })?;
    let raw = u16::from_be_bytes(prefix.try_into().expect("prefix is SPEC_VERSION_LEN bytes"));
    AlpenSpecId::try_from(raw).map_err(HeaderExtraError::UnknownVersion)
}

/// Returns the spec version governing the block at `number` with
/// `extra_data`.
///
/// The genesis block is [`AlpenSpecId::V0`] by definition and its
/// `extra_data` (authored by the genesis document, predating the layout) is
/// not decoded; every other block carries its version in the prefix.
pub fn spec_version_for_block(
    number: u64,
    extra_data: &[u8],
) -> Result<AlpenSpecId, HeaderExtraError> {
    if number == 0 {
        return Ok(AlpenSpecId::V0);
    }
    peek_spec_version(extra_data)
}

/// [`spec_version_for_block`] read off a header.
pub fn header_spec_version(header: &Header) -> Result<AlpenSpecId, HeaderExtraError> {
    spec_version_for_block(header.number, &header.extra_data)
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;

    use super::*;
    use crate::spec_activations::known_versions;

    #[test]
    fn encode_decode_roundtrip_for_every_version() {
        for version in known_versions() {
            let extra = HeaderExtra::new(version, 1_234_567);
            let bytes = extra.encode();
            assert_eq!(bytes.len(), LAYOUT_LEN, "{version:?}");
            assert_eq!(&bytes[..SPEC_VERSION_LEN], u16::from(version).to_be_bytes());
            assert_eq!(&bytes[SPEC_VERSION_LEN..], 1_234_567u64.to_be_bytes());
            assert_eq!(HeaderExtra::decode(&bytes), Ok(extra), "{version:?}");
            assert_eq!(peek_spec_version(&bytes), Ok(version), "{version:?}");
            assert_eq!(
                HeaderExtra::decode(&bytes).unwrap().da_rate(),
                1_234_567,
                "{version:?}"
            );
        }
    }

    /// An unstamped header is the pre-stamp state of the chain: V0, no rate.
    #[test]
    fn empty_extra_data_is_v0() {
        assert_eq!(
            HeaderExtra::decode(&[]),
            Ok(HeaderExtra::new(AlpenSpecId::V0, 0))
        );
        assert_eq!(peek_spec_version(&[]), Ok(AlpenSpecId::V0));
    }

    /// A short but non-empty prefix is a truncated stamp, not an absent one.
    #[test]
    fn decode_rejects_short_prefixes() {
        let extra_data = &[0x00][..];
        let err = HeaderExtraError::TooShort {
            len: extra_data.len(),
        };
        assert_eq!(HeaderExtra::decode(extra_data), Err(err));
        assert_eq!(peek_spec_version(extra_data), Err(err));
    }

    /// The version prefix alone, with the body missing, is a layout violation.
    #[test]
    fn decode_rejects_a_missing_body() {
        let extra_data = 0x0001u16.to_be_bytes();
        assert_eq!(
            HeaderExtra::decode(&extra_data),
            Err(HeaderExtraError::WrongLength {
                version: AlpenSpecId::V1,
                expected: LAYOUT_LEN,
                len: SPEC_VERSION_LEN,
            })
        );
    }

    #[test]
    fn decode_rejects_unknown_versions() {
        // The dev genesis document's `extraData` ("SC") happens to be exactly
        // the spec version prefix's width — proof that operator-authored bytes
        // must never reach decode, only the genesis exemption.
        for (extra_data, raw) in [(*b"SC", 0x5343), (0x0002u16.to_be_bytes(), 2)] {
            let err = HeaderExtraError::UnknownVersion(raw);
            assert_eq!(HeaderExtra::decode(&extra_data), Err(err));
            assert_eq!(peek_spec_version(&extra_data), Err(err));
        }
    }

    /// The full parse rejects bytes past the version's layout; the peek,
    /// which must keep routing on layouts a later version widens, does not.
    #[test]
    fn decode_rejects_trailing_bytes_but_peek_does_not() {
        let mut extra_data = HeaderExtra::new(AlpenSpecId::V1, 9).encode();
        extra_data.push(0xFF);
        assert_eq!(
            HeaderExtra::decode(&extra_data),
            Err(HeaderExtraError::WrongLength {
                version: AlpenSpecId::V1,
                expected: LAYOUT_LEN,
                len: LAYOUT_LEN + 1,
            })
        );
        assert_eq!(peek_spec_version(&extra_data), Ok(AlpenSpecId::V1));
    }

    #[test]
    fn genesis_is_v0_without_decoding() {
        // The dev chain's operator-authored genesis extra_data.
        assert_eq!(spec_version_for_block(0, b"SC"), Ok(AlpenSpecId::V0));
        assert_eq!(spec_version_for_block(0, &[]), Ok(AlpenSpecId::V0));

        // Past genesis the prefix is authoritative.
        assert_eq!(
            spec_version_for_block(1, b"SC"),
            Err(HeaderExtraError::UnknownVersion(0x5343))
        );
        assert_eq!(
            spec_version_for_block(1, &HeaderExtra::new(AlpenSpecId::V1, 0).encode()),
            Ok(AlpenSpecId::V1)
        );
    }

    #[test]
    fn header_spec_version_reads_number_and_extra_data() {
        let header = Header {
            number: 7,
            extra_data: HeaderExtra::new(AlpenSpecId::V1, 42).encode().into(),
            ..Default::default()
        };
        assert_eq!(header_spec_version(&header), Ok(AlpenSpecId::V1));

        let genesis = Header {
            number: 0,
            extra_data: b"SC".as_slice().into(),
            ..Default::default()
        };
        assert_eq!(header_spec_version(&genesis), Ok(AlpenSpecId::V0));
    }

    /// Headers produced before the layout existed carry empty `extra_data` (the
    /// repo's block 1-4 witnesses are exactly this). They must resolve to v0, or
    /// upgrading an existing datadir breaks historical sync and re-execution.
    #[test]
    fn legacy_unstamped_headers_resolve_to_v0() {
        // Non-genesis headers with empty extra_data.
        for number in 1..=4u64 {
            let h = Header {
                number,
                extra_data: Default::default(),
                ..Default::default()
            };
            assert_eq!(
                header_spec_version(&h),
                Ok(AlpenSpecId::V0),
                "block {number}"
            );
        }
        // Genesis "SC" still exempt.
        assert_eq!(spec_version_for_block(0, b"SC"), Ok(AlpenSpecId::V0));
        // And the full parse agrees, so consensus validate_header passes too.
        assert_eq!(
            HeaderExtra::decode(&[]),
            Ok(HeaderExtra::new(AlpenSpecId::V0, 0))
        );
    }
}
