//! The versioned layout of the EE block header's [`Header::extra_data`]
//! field.
//!
//! The layout is a fixed prefix followed by a version-defined body. The
//! prefix is the governing spec version as a big-endian integer, exactly
//! [`AlpenSpecId`]-wide and the only version-independent part; the body that
//! follows is defined by that version's layout. [`AlpenSpecId`] is thus also
//! the version of the layout itself, so the two version spaces coincide.
//!
//! Decoding is strict: a header claiming a version this binary has no
//! variant for must fail rather than run under stale rules. The genesis
//! header is the one exemption — its `extra_data` is authored by the genesis
//! document and predates the layout — so genesis is fixed at
//! [`AlpenSpecId::V0`].

use alloy_consensus::{constants::MAXIMUM_EXTRA_DATA_SIZE, Header};
use thiserror::Error;

use crate::AlpenSpecId;

/// Length of the version-independent spec version prefix.
const SPEC_VERSION_LEN: usize = std::mem::size_of::<AlpenSpecId>();

/// The decoded contents of a header's `extra_data`.
///
/// Carries the fields the chain commits in the header beyond the standard
/// EVM ones. Which fields exist is a per-version fact defined by this type's
/// codec; today every version carries only the governing spec version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderExtra {
    /// The spec version whose rules govern the block.
    spec_version: AlpenSpecId,
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
    /// `spec_version`.
    pub fn new(spec_version: AlpenSpecId) -> Self {
        Self { spec_version }
    }

    /// Returns the governing spec version.
    pub fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }

    /// Encodes into the `extra_data` bytes under the version's layout.
    pub fn encode(&self) -> Vec<u8> {
        let buf = u16::from(self.spec_version).to_be_bytes().to_vec();
        match self.spec_version {
            // No body fields defined yet.
            AlpenSpecId::V0 | AlpenSpecId::V1 => {}
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
        let spec_version = peek_spec_version(extra_data)?;
        let body = &extra_data[SPEC_VERSION_LEN..];
        match spec_version {
            // No body fields defined yet.
            AlpenSpecId::V0 | AlpenSpecId::V1 => {
                if !body.is_empty() {
                    return Err(HeaderExtraError::WrongLength {
                        version: spec_version,
                        expected: SPEC_VERSION_LEN,
                        len: extra_data.len(),
                    });
                }
            }
        }
        Ok(Self { spec_version })
    }
}

/// Reads the governing spec version from `extra_data`'s version prefix.
///
/// Only the prefix: the rest of the layout is not validated (that is
/// [`HeaderExtra::decode`]'s job, exercised by consensus header validation),
/// so version dispatch keeps working on fields a later layout adds.
pub fn peek_spec_version(extra_data: &[u8]) -> Result<AlpenSpecId, HeaderExtraError> {
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
            let extra = HeaderExtra::new(version);
            let bytes = extra.encode();
            assert_eq!(&bytes[..SPEC_VERSION_LEN], u16::from(version).to_be_bytes());
            assert_eq!(HeaderExtra::decode(&bytes), Ok(extra), "{version:?}");
            assert_eq!(peek_spec_version(&bytes), Ok(version), "{version:?}");
        }
    }

    #[test]
    fn decode_rejects_short_prefixes() {
        for extra_data in [&[][..], &[0x00][..]] {
            let err = HeaderExtraError::TooShort {
                len: extra_data.len(),
            };
            assert_eq!(HeaderExtra::decode(extra_data), Err(err));
            assert_eq!(peek_spec_version(extra_data), Err(err));
        }
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
        let extra_data = [0x00, 0x01, 0xFF];
        assert_eq!(
            HeaderExtra::decode(&extra_data),
            Err(HeaderExtraError::WrongLength {
                version: AlpenSpecId::V1,
                expected: SPEC_VERSION_LEN,
                len: 3,
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
            spec_version_for_block(1, &HeaderExtra::new(AlpenSpecId::V1).encode()),
            Ok(AlpenSpecId::V1)
        );
    }

    #[test]
    fn header_spec_version_reads_number_and_extra_data() {
        let header = Header {
            number: 7,
            extra_data: HeaderExtra::new(AlpenSpecId::V1).encode().into(),
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
}
