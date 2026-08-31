//! Declarative macros for defining MDBX tables and their key/value codecs.
//!
//! These give table definitions a compact, uniform surface. A table is a
//! zero-sized marker type that implements [`Schema`](crate::Schema); codec impls
//! are attached to the key and value types via the `impl_*_codec` macros.
//!
//! Versioned values — the ones that evolve across releases — additionally use
//! [`versioned_value!`](crate::versioned_value) and the `impl_schema_version_*`
//! macros; see the [`version`](crate::version) module.

/// Defines a table marker type implementing [`Schema`](crate::Schema).
///
/// Codecs are attached separately (see the `impl_*_codec` macros), or use a
/// bundling macro such as [`define_table_borsh!`](crate::define_table_borsh).
///
/// Flags may follow the table name, in any order:
///
/// - `dup_sort` — open the table with MDBX `DUP_SORT`.
/// - `immutable` — the table's values are fixed once written, so an existing key may never be
///   overwritten with different bytes (see [`Regime`](crate::Regime)). Tables are
///   [`Regime::Mutable`](crate::Regime::Mutable) by default.
#[macro_export]
macro_rules! define_table {
    ($(#[$docs:meta])* ($name:ident $(, $flag:ident)*) $key:ty => $value:ty) => {
        $(#[$docs])*
        #[derive(Clone, Copy, Debug, Default)]
        pub(crate) struct $name;

        impl $crate::Schema for $name {
            const NAME: &'static str = ::core::stringify!($name);
            const DUP_SORT: bool = $crate::define_table!(@dup_sort $($flag)*);
            const REGIME: $crate::Regime = $crate::define_table!(@regime $($flag)*);
            type Key = $key;
            type Value = $value;
        }
    };

    // --- flag lookups: match the flag ident literally, else keep scanning ---
    (@dup_sort) => { false };
    (@dup_sort dup_sort $($rest:ident)*) => { true };
    (@dup_sort $other:ident $($rest:ident)*) => { $crate::define_table!(@dup_sort $($rest)*) };

    (@regime) => { $crate::Regime::Mutable };
    (@regime immutable $($rest:ident)*) => { $crate::Regime::Immutable };
    (@regime $other:ident $($rest:ident)*) => { $crate::define_table!(@regime $($rest)*) };
}

/// Builds a `Vec<TableSpec>` from a list of [`Schema`](crate::Schema) types, for
/// passing to [`MdbxEnv::open`](crate::MdbxEnv::open).
#[macro_export]
macro_rules! tables {
    ($($schema:ty),+ $(,)?) => {
        ::std::vec![ $( $crate::TableSpec::of::<$schema>() ),+ ]
    };
}

/// borsh [`KeyCodec`](crate::KeyCodec). Note: borsh encodes integers
/// little-endian, so this does **not** preserve numeric cursor order for
/// integer keys — use [`impl_be_key_codec!`](crate::impl_be_key_codec) there.
#[macro_export]
macro_rules! impl_borsh_key_codec {
    ($schema:ty, $key:ty) => {
        impl $crate::KeyCodec<$schema> for $key {
            fn encode_key(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::borsh::to_vec(self)
                    .map_err(|e| $crate::CodecError::encode(<$schema as $crate::Schema>::NAME, e))
            }

            fn decode_key(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
                ::borsh::from_slice(bytes)
                    .map_err(|e| $crate::CodecError::decode(<$schema as $crate::Schema>::NAME, e))
            }
        }
    };
}

/// borsh [`ValueCodec`](crate::ValueCodec).
#[macro_export]
macro_rules! impl_borsh_value_codec {
    ($schema:ty, $value:ty) => {
        impl $crate::ValueCodec<$schema> for $value {
            fn encode_value(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::borsh::to_vec(self)
                    .map_err(|e| $crate::CodecError::encode(<$schema as $crate::Schema>::NAME, e))
            }

            fn decode_value(
                bytes: &[u8],
                _ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<Self, $crate::CodecError> {
                ::borsh::from_slice(bytes)
                    .map_err(|e| $crate::CodecError::decode(<$schema as $crate::Schema>::NAME, e))
            }
        }
    };
}

/// Raw-bytes [`ValueCodec`](crate::ValueCodec) for `Vec<u8>`: the bytes are
/// stored verbatim, with no length prefix or framing. Use for values that are
/// already an opaque encoded blob (e.g. bincode payloads served directly).
#[macro_export]
macro_rules! impl_raw_value_codec {
    ($schema:ty) => {
        impl $crate::ValueCodec<$schema> for ::std::vec::Vec<u8> {
            fn encode_value(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::core::result::Result::Ok(self.clone())
            }

            fn decode_value(
                bytes: &[u8],
                _ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<Self, $crate::CodecError> {
                ::core::result::Result::Ok(bytes.to_vec())
            }
        }
    };
}

/// bincode [`ValueCodec`](crate::ValueCodec), using bincode's default
/// configuration, for `serde`-serializable values that are not borsh.
#[macro_export]
macro_rules! impl_bincode_value_codec {
    ($schema:ty, $value:ty) => {
        impl $crate::ValueCodec<$schema> for $value {
            fn encode_value(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::bincode::serialize(self)
                    .map_err(|e| $crate::CodecError::encode(<$schema as $crate::Schema>::NAME, e))
            }

            fn decode_value(
                bytes: &[u8],
                _ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<Self, $crate::CodecError> {
                ::bincode::deserialize(bytes)
                    .map_err(|e| $crate::CodecError::decode(<$schema as $crate::Schema>::NAME, e))
            }
        }
    };
}

/// Big-endian, fixed-width [`KeyCodec`](crate::KeyCodec) via bincode. Preserves
/// numeric ordering under MDBX's lexicographic key comparison, so use it for
/// integer keys queried by range or `first`/`last`.
#[macro_export]
macro_rules! impl_be_key_codec {
    ($schema:ty, $key:ty) => {
        impl $crate::KeyCodec<$schema> for $key {
            fn encode_key(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                use ::bincode::Options as _;
                ::bincode::options()
                    .with_fixint_encoding()
                    .with_big_endian()
                    .serialize(self)
                    .map_err(|e| $crate::CodecError::encode(<$schema as $crate::Schema>::NAME, e))
            }

            fn decode_key(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
                use ::bincode::Options as _;
                ::bincode::options()
                    .with_fixint_encoding()
                    .with_big_endian()
                    .deserialize(bytes)
                    .map_err(|e| $crate::CodecError::decode(<$schema as $crate::Schema>::NAME, e))
            }
        }
    };
}

/// `strata-codec` [`KeyCodec`](crate::KeyCodec).
#[macro_export]
macro_rules! impl_codec_key_codec {
    ($schema:ty, $key:ty) => {
        impl $crate::KeyCodec<$schema> for $key {
            fn encode_key(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::strata_codec::encode_to_vec(self)
                    .map_err(|e| $crate::CodecError::encode(<$schema as $crate::Schema>::NAME, e))
            }

            fn decode_key(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
                use ::strata_codec::{BufDecoder, Codec};
                let mut decoder = BufDecoder::new(bytes);
                Codec::decode(&mut decoder)
                    .map_err(|e| $crate::CodecError::decode(<$schema as $crate::Schema>::NAME, e))
            }
        }
    };
}

/// `strata-codec` [`ValueCodec`](crate::ValueCodec).
#[macro_export]
macro_rules! impl_codec_value_codec {
    ($schema:ty, $value:ty) => {
        impl $crate::ValueCodec<$schema> for $value {
            fn encode_value(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::strata_codec::encode_to_vec(self)
                    .map_err(|e| $crate::CodecError::encode(<$schema as $crate::Schema>::NAME, e))
            }

            fn decode_value(
                bytes: &[u8],
                _ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<Self, $crate::CodecError> {
                use ::strata_codec::{BufDecoder, Codec};
                let mut decoder = BufDecoder::new(bytes);
                Codec::decode(&mut decoder)
                    .map_err(|e| $crate::CodecError::decode(<$schema as $crate::Schema>::NAME, e))
            }
        }
    };
}

/// Defines a table with borsh codecs on both key and value.
#[macro_export]
macro_rules! define_table_borsh {
    ($(#[$docs:meta])* ($name:ident $(, $flag:ident)*) $key:ty => $value:ty) => {
        $crate::define_table!($(#[$docs])* ($name $(, $flag)*) $key => $value);
        $crate::impl_borsh_key_codec!($name, $key);
        $crate::impl_borsh_value_codec!($name, $value);
    };
}

/// Defines a table with a big-endian integer key and a borsh value — the
/// default for index/sequence tables that need numeric cursor order.
#[macro_export]
macro_rules! define_table_be_key {
    ($(#[$docs:meta])* ($name:ident $(, $flag:ident)*) $key:ty => $value:ty) => {
        $crate::define_table!($(#[$docs])* ($name $(, $flag)*) $key => $value);
        $crate::impl_be_key_codec!($name, $key);
        $crate::impl_borsh_value_codec!($name, $value);
    };
}

/// Defines a table with a big-endian integer or fixed-width key and a
/// bincode-encoded value — for `serde`-only value types such as the reth
/// state-diff records.
#[macro_export]
macro_rules! define_table_bincode_be_key {
    ($(#[$docs:meta])* ($name:ident $(, $flag:ident)*) $key:ty => $value:ty) => {
        $crate::define_table!($(#[$docs])* ($name $(, $flag)*) $key => $value);
        $crate::impl_be_key_codec!($name, $key);
        $crate::impl_bincode_value_codec!($name, $value);
    };
}

/// Defines a table with a big-endian key and a raw `Vec<u8>` value stored
/// verbatim — for opaque encoded blobs served directly (e.g. bincode payloads).
#[macro_export]
macro_rules! define_table_raw_be_key {
    ($(#[$docs:meta])* ($name:ident $(, $flag:ident)*) $key:ty => Vec<u8>) => {
        $crate::define_table!($(#[$docs])* ($name $(, $flag)*) $key => ::std::vec::Vec<u8>);
        $crate::impl_be_key_codec!($name, $key);
        $crate::impl_raw_value_codec!($name);
    };
}

/// CBOR [`ValueCodec`](crate::ValueCodec) via `ciborium`, for values that are
/// `serde`-serializable but not borsh.
///
/// CBOR's self-describing map encoding also tolerates fields being added to a
/// record later, which matters for the broadcast/envelope entries shared with
/// the upstream stores.
#[macro_export]
macro_rules! impl_cbor_value_codec {
    ($schema:ty, $value:ty) => {
        impl $crate::ValueCodec<$schema> for $value {
            fn encode_value(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                let mut buf = ::std::vec::Vec::new();
                ::ciborium::into_writer(self, &mut buf).map_err(|e| {
                    $crate::CodecError::encode(<$schema as $crate::Schema>::NAME, e)
                })?;
                ::core::result::Result::Ok(buf)
            }

            fn decode_value(
                bytes: &[u8],
                _ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<Self, $crate::CodecError> {
                ::ciborium::from_reader(bytes)
                    .map_err(|e| $crate::CodecError::decode(<$schema as $crate::Schema>::NAME, e))
            }
        }
    };
}

/// Raw-bytes [`KeyCodec`](crate::KeyCodec) for `Vec<u8>`: the key is stored
/// verbatim, so any tag prefix it carries sorts as written. A length-prefixed
/// encoding would sort by length first.
#[macro_export]
macro_rules! impl_raw_key_codec {
    ($schema:ty) => {
        impl $crate::KeyCodec<$schema> for ::std::vec::Vec<u8> {
            fn encode_key(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::core::result::Result::Ok(self.clone())
            }

            fn decode_key(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
                ::core::result::Result::Ok(bytes.to_vec())
            }
        }
    };
}

// --- Schema versioning ---------------------------------------------------

/// Binds a chain of shipped versions into one versioned value.
///
/// Each `tag => Type` entry names a version that has shipped, ascending; the
/// last one is current, and the macro emits a type alias of the family name
/// pointing at it. Reading dispatches on the on-disk tag and folds the value up
/// to current through [`UpConvert`](crate::UpConvert); writing always emits the
/// current version. A missing `N -> N+1` converter is a compile error.
///
/// Bumping a version means *adding* an entry and *adding* one converter — the
/// already-shipped structs and converters are never edited, because bytes
/// carrying their tags are still on disk.
///
/// See the [`version`](crate::version) module for a worked example.
#[macro_export]
macro_rules! versioned_value {
    (
        $(#[$docs:meta])*
        $vis:vis $family:ident {
            $( $tag:literal => $ver:ty ),+ $(,)?
        }
    ) => {
        $(#[$docs])*
        $vis type $family = $crate::versioned_value!(@last_ty $($ver),+);

        // Each version's declared tag must match the one bound here, so the
        // dispatch table and the encoder can never disagree.
        const _: () = {
            $(
                ::core::assert!(
                    <$ver as $crate::SchemaVersion>::VERSION == $tag,
                    "version tag does not match the type's `SchemaVersion::VERSION`",
                );
            )+
        };

        $crate::versioned_value!(@chain $family; $($ver),+);

        impl $crate::VersionedValue for $family {
            const FAMILY: &'static str = ::core::stringify!($family);
            const CURRENT_VERSION: u8 = $crate::versioned_value!(@last_tag $($tag),+);
            const VERSIONS: &'static [u8] = &[$($tag),+];

            fn decode_tagged(
                bytes: &[u8],
                ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<Self, $crate::CodecError> {
                let family = <Self as $crate::VersionedValue>::FAMILY;
                let (tag, payload) = $crate::split_version_tag(family, bytes)?;
                match tag {
                    $(
                        $tag => {
                            let value = <$ver as $crate::SchemaVersion>::decode_payload(payload)?;
                            <$ver as $crate::LiftToCurrent<Self>>::lift_to_current(value, ctx)
                        }
                    )+
                    other => ::core::result::Result::Err($crate::unknown_version_error(
                        family,
                        other,
                        <Self as $crate::VersionedValue>::CURRENT_VERSION,
                    )),
                }
            }

            fn encode_tagged(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                let mut out = ::std::vec::Vec::new();
                out.push(<Self as $crate::VersionedValue>::CURRENT_VERSION);
                out.extend_from_slice(
                    &<Self as $crate::SchemaVersion>::encode_payload(self)?,
                );
                ::core::result::Result::Ok(out)
            }
        }
    };

    // --- the last entry of a list: the current version's type and tag ---
    (@last_ty $x:ty) => { $x };
    (@last_ty $x:ty, $($rest:ty),+) => { $crate::versioned_value!(@last_ty $($rest),+) };

    (@last_tag $x:literal) => { $x };
    (@last_tag $x:literal, $($rest:literal),+) => {
        $crate::versioned_value!(@last_tag $($rest),+)
    };

    // --- the fold to current: one `UpConvert` hop per consecutive pair ---
    (@chain $current:ty; $last:ty) => {
        impl $crate::LiftToCurrent<$current> for $last {
            fn lift_to_current(
                self,
                _ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<$current, $crate::CodecError> {
                ::core::result::Result::Ok(self)
            }
        }
    };
    (@chain $current:ty; $from:ty, $to:ty $(, $rest:ty)*) => {
        impl $crate::LiftToCurrent<$current> for $from {
            fn lift_to_current(
                self,
                ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<$current, $crate::CodecError> {
                let next: $to = <$from as $crate::UpConvert<$to>>::up_convert(self, ctx)?;
                <$to as $crate::LiftToCurrent<$current>>::lift_to_current(next, ctx)
            }
        }
        $crate::versioned_value!(@chain $current; $to $(, $rest)*);
    };
}

/// borsh [`SchemaVersion`](crate::SchemaVersion) for one shipped version.
#[macro_export]
macro_rules! impl_schema_version_borsh {
    ($family:ident, $ver:ty, $tag:literal) => {
        impl $crate::SchemaVersion for $ver {
            const FAMILY: &'static str = ::core::stringify!($family);
            const VERSION: u8 = $tag;

            fn decode_payload(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
                ::borsh::from_slice(bytes)
                    .map_err(|e| $crate::CodecError::decode(::core::stringify!($family), e))
            }

            fn encode_payload(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::borsh::to_vec(self)
                    .map_err(|e| $crate::CodecError::encode(::core::stringify!($family), e))
            }
        }
    };
}

/// `strata-codec` [`SchemaVersion`](crate::SchemaVersion) for one shipped
/// version.
#[macro_export]
macro_rules! impl_schema_version_codec {
    ($family:ident, $ver:ty, $tag:literal) => {
        impl $crate::SchemaVersion for $ver {
            const FAMILY: &'static str = ::core::stringify!($family);
            const VERSION: u8 = $tag;

            fn decode_payload(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
                use ::strata_codec::{BufDecoder, Codec};
                let mut decoder = BufDecoder::new(bytes);
                Codec::decode(&mut decoder)
                    .map_err(|e| $crate::CodecError::decode(::core::stringify!($family), e))
            }

            fn encode_payload(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::strata_codec::encode_to_vec(self)
                    .map_err(|e| $crate::CodecError::encode(::core::stringify!($family), e))
            }
        }
    };
}

/// CBOR [`SchemaVersion`](crate::SchemaVersion) for one shipped version.
#[macro_export]
macro_rules! impl_schema_version_cbor {
    ($family:ident, $ver:ty, $tag:literal) => {
        impl $crate::SchemaVersion for $ver {
            const FAMILY: &'static str = ::core::stringify!($family);
            const VERSION: u8 = $tag;

            fn decode_payload(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
                ::ciborium::from_reader(bytes)
                    .map_err(|e| $crate::CodecError::decode(::core::stringify!($family), e))
            }

            fn encode_payload(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                let mut buf = ::std::vec::Vec::new();
                ::ciborium::into_writer(self, &mut buf)
                    .map_err(|e| $crate::CodecError::encode(::core::stringify!($family), e))?;
                ::core::result::Result::Ok(buf)
            }
        }
    };
}

/// bincode [`SchemaVersion`](crate::SchemaVersion) for one shipped version.
#[macro_export]
macro_rules! impl_schema_version_bincode {
    ($family:ident, $ver:ty, $tag:literal) => {
        impl $crate::SchemaVersion for $ver {
            const FAMILY: &'static str = ::core::stringify!($family);
            const VERSION: u8 = $tag;

            fn decode_payload(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
                ::bincode::deserialize(bytes)
                    .map_err(|e| $crate::CodecError::decode(::core::stringify!($family), e))
            }

            fn encode_payload(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                ::bincode::serialize(self)
                    .map_err(|e| $crate::CodecError::encode(::core::stringify!($family), e))
            }
        }
    };
}

/// [`ValueCodec`](crate::ValueCodec) delegating to a value's
/// [`VersionedValue`](crate::VersionedValue) impl — the version-dispatching read
/// path, and the current format on write.
#[macro_export]
macro_rules! impl_versioned_value_codec {
    ($schema:ty, $value:ty) => {
        impl $crate::ValueCodec<$schema> for $value {
            fn encode_value(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                <$value as $crate::VersionedValue>::encode_tagged(self)
            }

            fn decode_value(
                bytes: &[u8],
                ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<Self, $crate::CodecError> {
                <$value as $crate::VersionedValue>::decode_tagged(bytes, ctx)
            }
        }
    };
}

/// Defines a table with a borsh key and a version-dispatched value.
#[macro_export]
macro_rules! define_table_versioned {
    ($(#[$docs:meta])* ($name:ident $(, $flag:ident)*) $key:ty => $value:ty) => {
        $crate::define_table!($(#[$docs])* ($name $(, $flag)*) $key => $value);
        $crate::impl_borsh_key_codec!($name, $key);
        $crate::impl_versioned_value_codec!($name, $value);
    };
}

/// Defines a table with a big-endian integer key and a version-dispatched value.
#[macro_export]
macro_rules! define_table_versioned_be_key {
    ($(#[$docs:meta])* ($name:ident $(, $flag:ident)*) $key:ty => $value:ty) => {
        $crate::define_table!($(#[$docs])* ($name $(, $flag)*) $key => $value);
        $crate::impl_be_key_codec!($name, $key);
        $crate::impl_versioned_value_codec!($name, $value);
    };
}
