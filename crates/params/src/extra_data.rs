//! The versioned layout of the EE block header's [`Header::extra_data`]
//! field.
//!
//! V0's layout is empty. The deployed V0 binary leaves `extra_data` empty, so
//! a V0 block is the same whichever binary built it, and the deployed V0
//! guest can prove it.
//!
//! From V1 on, the layout is a fixed prefix followed by a version-defined
//! body. The prefix is the governing spec version as a big-endian integer,
//! exactly [`AlpenSpecId`]-wide. The body that follows is defined by that
//! version's layout. [`AlpenSpecId`] is thus also the version of the layout
//! itself, so the two version spaces coincide.
//!
//! V1's body has one field: the block's DA rate in wei per byte, as a
//! big-endian `u64`. The sequencer freezes the live rate into it per block,
//! and re-execution reads it back so the in-EVM DA fee charge always sees the
//! rate the block actually committed to. V0 has no rate field, so V0 blocks
//! charge no DA fee.
//!
//! Decoding is strict: a header claiming a version this binary has no
//! variant for must fail rather than run under stale rules. A short but
//! non-empty field is a truncated or corrupt stamp, and a non-empty field
//! that names V0 breaks V0's empty layout, so both are rejected. The one
//! exemption is the genesis header: its `extra_data` is authored by the
//! genesis document and predates the layout, so it is fixed at
//! [`AlpenSpecId::V0`] whatever it holds.

use std::mem::size_of;

use alloy_consensus::{constants::MAXIMUM_EXTRA_DATA_SIZE, Header};
use thiserror::Error;

use crate::AlpenSpecId;

/// Length of the spec version prefix that every layout from V1 on starts
/// with.
const SPEC_VERSION_LEN: usize = size_of::<AlpenSpecId>();

/// Length of the DA rate body field.
const DA_RATE_LEN: usize = size_of::<u64>();

/// Total length of the V1 layout: the version prefix followed by the DA rate.
const V1_LAYOUT_LEN: usize = SPEC_VERSION_LEN + DA_RATE_LEN;

/// The decoded contents of a header's `extra_data`.
///
/// One variant per spec version. Each carries the fields its version commits
/// in the header beyond the standard EVM ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderExtra {
    /// The V0 layout: empty.
    V0,
    /// The V1 layout: the version prefix followed by the DA rate.
    V1 {
        /// The DA rate (wei per byte) the block charges under.
        da_rate: u64,
    },
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
    ///
    /// The rate is kept only if the version's layout has a rate field. V0's
    /// does not, so a V0 block charges no DA fee whatever `da_rate` is.
    pub fn new(spec_version: AlpenSpecId, da_rate: u64) -> Self {
        match spec_version {
            AlpenSpecId::V0 => Self::V0,
            AlpenSpecId::V1 => Self::V1 { da_rate },
        }
    }

    /// Returns the governing spec version.
    pub fn spec_version(&self) -> AlpenSpecId {
        match self {
            Self::V0 => AlpenSpecId::V0,
            Self::V1 { .. } => AlpenSpecId::V1,
        }
    }

    /// Returns the DA rate (wei per byte) the block charges under, or `None`
    /// if its version's layout has no rate field.
    pub fn da_rate(&self) -> Option<u64> {
        match self {
            Self::V0 => None,
            Self::V1 { da_rate } => Some(*da_rate),
        }
    }

    /// Encodes into the `extra_data` bytes under the version's layout.
    pub fn encode(&self) -> Vec<u8> {
        let buf = match self {
            Self::V0 => Vec::new(),
            Self::V1 { da_rate } => {
                let mut buf = u16::from(self.spec_version()).to_be_bytes().to_vec();
                buf.extend_from_slice(&da_rate.to_be_bytes());
                buf
            }
        };
        // The whole layout must fit Ethereum's `extra_data` cap, else the
        // block can't round-trip through an engine payload.
        debug_assert!(
            buf.len() <= MAXIMUM_EXTRA_DATA_SIZE,
            "{:?} extra_data layout is {} bytes, over the {}-byte cap",
            self.spec_version(),
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
        let spec_version = peek_spec_version(extra_data)?;
        let wrong_length = |expected| HeaderExtraError::WrongLength {
            version: spec_version,
            expected,
            len: extra_data.len(),
        };
        match spec_version {
            AlpenSpecId::V0 => {
                if !extra_data.is_empty() {
                    return Err(wrong_length(0));
                }
                Ok(Self::V0)
            }
            AlpenSpecId::V1 => {
                let body: [u8; DA_RATE_LEN] = extra_data[SPEC_VERSION_LEN..]
                    .try_into()
                    .map_err(|_| wrong_length(V1_LAYOUT_LEN))?;
                Ok(Self::V1 {
                    da_rate: u64::from_be_bytes(body),
                })
            }
        }
    }
}

/// Reads the governing spec version from `extra_data`'s version prefix.
///
/// Only the prefix: the rest of the layout is not validated (that is
/// [`HeaderExtra::decode`]'s job, exercised by consensus header validation),
/// so version dispatch keeps working on fields a later layout adds.
pub fn peek_spec_version(extra_data: &[u8]) -> Result<AlpenSpecId, HeaderExtraError> {
    // V0's layout is empty; see the module docs.
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
            assert_eq!(HeaderExtra::decode(&bytes), Ok(extra), "{version:?}");
            assert_eq!(peek_spec_version(&bytes), Ok(version), "{version:?}");
        }
    }

    /// V0 has no rate field: the rate is dropped and the field stays empty.
    #[test]
    fn v0_layout_is_empty() {
        let extra = HeaderExtra::new(AlpenSpecId::V0, 1_234_567);
        assert_eq!(extra, HeaderExtra::V0);
        assert_eq!(extra.da_rate(), None);
        assert_eq!(extra.encode(), Vec::<u8>::new());
        assert_eq!(HeaderExtra::decode(&[]), Ok(HeaderExtra::V0));
        assert_eq!(peek_spec_version(&[]), Ok(AlpenSpecId::V0));
    }

    #[test]
    fn v1_layout_is_the_prefix_then_the_rate() {
        let bytes = HeaderExtra::new(AlpenSpecId::V1, 1_234_567).encode();
        assert_eq!(bytes.len(), V1_LAYOUT_LEN);
        assert_eq!(&bytes[..SPEC_VERSION_LEN], 1u16.to_be_bytes());
        assert_eq!(&bytes[SPEC_VERSION_LEN..], 1_234_567u64.to_be_bytes());
        assert_eq!(
            HeaderExtra::decode(&bytes).unwrap().da_rate(),
            Some(1_234_567)
        );
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

    /// V0 is empty, so any bytes that name it break its layout. The peek
    /// still routes them to V0.
    #[test]
    fn decode_rejects_a_v0_prefix() {
        let with_rate = [0x0000u16.to_be_bytes().as_slice(), &9u64.to_be_bytes()].concat();
        for extra_data in [&0x0000u16.to_be_bytes()[..], &with_rate] {
            assert_eq!(
                HeaderExtra::decode(extra_data),
                Err(HeaderExtraError::WrongLength {
                    version: AlpenSpecId::V0,
                    expected: 0,
                    len: extra_data.len(),
                })
            );
            assert_eq!(peek_spec_version(extra_data), Ok(AlpenSpecId::V0));
        }
    }

    /// The version prefix alone, with the body missing, is a layout violation.
    #[test]
    fn decode_rejects_a_missing_body() {
        let extra_data = 0x0001u16.to_be_bytes();
        assert_eq!(
            HeaderExtra::decode(&extra_data),
            Err(HeaderExtraError::WrongLength {
                version: AlpenSpecId::V1,
                expected: V1_LAYOUT_LEN,
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
                expected: V1_LAYOUT_LEN,
                len: V1_LAYOUT_LEN + 1,
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

    /// The deployed V0 binary leaves `extra_data` empty (the repo's block 1-4
    /// witnesses are exactly this). Those headers must resolve to V0, or this
    /// binary can't sync or re-execute the deployed chain.
    #[test]
    fn deployed_v0_headers_resolve_to_v0() {
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
        // And the full parse agrees, so consensus validate_header passes too.
        assert_eq!(HeaderExtra::decode(&[]), Ok(HeaderExtra::V0));
    }
}
