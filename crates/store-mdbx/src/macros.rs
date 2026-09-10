//! Declarative macros for defining MDBX tables and their key/value codecs.
//!
//! These give table definitions a compact, uniform surface. A table is a
//! zero-sized marker type that implements [`Schema`](crate::Schema); codec impls
//! are attached to the key and value types via the `impl_*_codec` macros.
//!
//! A table whose values evolve across releases attaches a version chain with
//! [`impl_versioned_value_codec!`](crate::impl_versioned_value_codec), naming
//! each shipped version and how its payload is encoded; see the
//! [`version`](crate::version) module.

/// Defines a table marker type implementing [`Schema`](crate::Schema).
///
/// Codecs are attached separately (see the `impl_*_codec` macros), or use a
/// bundling macro such as [`define_table_borsh!`](crate::define_table_borsh).
#[macro_export]
macro_rules! define_table {
    ($(#[$docs:meta])* ($name:ident) $key:ty => $value:ty) => {
        $(#[$docs])*
        #[derive(Clone, Copy, Debug, Default)]
        pub(crate) struct $name;

        impl $crate::Schema for $name {
            const NAME: &'static str = ::core::stringify!($name);
            type Key = $key;
            type Value = $value;
        }
    };
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
    ($(#[$docs:meta])* ($name:ident) $key:ty => $value:ty) => {
        $crate::define_table!($(#[$docs])* ($name) $key => $value);
        $crate::impl_borsh_key_codec!($name, $key);
        $crate::impl_borsh_value_codec!($name, $value);
    };
}

/// Defines a table with a big-endian integer key and a borsh value — the
/// default for index/sequence tables that need numeric cursor order.
#[macro_export]
macro_rules! define_table_be_key {
    ($(#[$docs:meta])* ($name:ident) $key:ty => $value:ty) => {
        $crate::define_table!($(#[$docs])* ($name) $key => $value);
        $crate::impl_be_key_codec!($name, $key);
        $crate::impl_borsh_value_codec!($name, $value);
    };
}

/// Defines a table with a big-endian integer or fixed-width key and a
/// bincode-encoded value — for `serde`-only value types such as the reth
/// state-diff records.
#[macro_export]
macro_rules! define_table_bincode_be_key {
    ($(#[$docs:meta])* ($name:ident) $key:ty => $value:ty) => {
        $crate::define_table!($(#[$docs])* ($name) $key => $value);
        $crate::impl_be_key_codec!($name, $key);
        $crate::impl_bincode_value_codec!($name, $value);
    };
}

/// Defines a table with a big-endian key and a raw `Vec<u8>` value stored
/// verbatim — for opaque encoded blobs served directly (e.g. bincode payloads).
#[macro_export]
macro_rules! define_table_raw_be_key {
    ($(#[$docs:meta])* ($name:ident) $key:ty => Vec<u8>) => {
        $crate::define_table!($(#[$docs])* ($name) $key => ::std::vec::Vec<u8>);
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

/// Attaches a table's version chain: its shipped versions, the dispatch that
/// reads them, and the [`ValueCodec`](crate::ValueCodec) that writes the current
/// one.
///
/// Each `tag => Type as codec` entry names a version that has shipped,
/// ascending; the last one is current and must be the table's
/// [`Schema::Value`](crate::Schema::Value). Reading dispatches on the on-disk
/// tag and folds up through [`UpConvert`](crate::UpConvert); writing always
/// emits the current version. A missing `N -> N+1` converter is a compile error.
///
/// The `as` clause names how that version's payload is encoded — one of
/// `borsh`, `bincode`, `cbor`, or `codec` (`strata-codec`) — and generates its
/// [`SchemaVersion`](crate::SchemaVersion) impl, so a version is declared in
/// exactly one place. Drop the clause to write that impl by hand; the tag it
/// declares is then checked against the one bound here.
///
/// The chain belongs to this table alone: adding a version here leaves every
/// other table's stored bytes untouched.
#[macro_export]
macro_rules! impl_versioned_value_codec {
    (
        $schema:ty {
            $( $tag:literal => $ver:ty $(as $codec:ident)? ),+ $(,)?
        }
    ) => {
        // Attach each version's payload codec. An entry without an `as` clause
        // expands to nothing and brings its own `SchemaVersion` impl.
        $(
            $crate::impl_versioned_value_codec!(@payload $schema, $ver, $tag $(, $codec)?);
        )+

        // Each version's declared tag must match the one bound here, so the
        // dispatch table and the encoder can never disagree.
        const _: () = {
            $(
                ::core::assert!(
                    <$ver as $crate::SchemaVersion<$schema>>::VERSION == $tag,
                    "version tag does not match the type's `SchemaVersion::VERSION`",
                );
            )+
        };

        // The table must store the chain's *current* version, never a past one.
        const _: fn($crate::impl_versioned_value_codec!(@last_ty $($ver),+))
            -> <$schema as $crate::Schema>::Value = |value| value;


        $crate::impl_versioned_value_codec!(@chain $schema; $($ver),+);

        impl $crate::VersionedTable for $schema {
            const CURRENT_VERSION: u8 =
                $crate::impl_versioned_value_codec!(@last_tag $($tag),+);
            const VERSIONS: &'static [u8] = &[$($tag),+];

            fn decode_tagged(
                bytes: &[u8],
                ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<Self::Value, $crate::CodecError> {
                let table = <Self as $crate::Schema>::NAME;
                let (tag, payload) = $crate::split_version_tag(table, bytes)?;
                match tag {
                    $(
                        $tag => {
                            let value =
                                <$ver as $crate::SchemaVersion<$schema>>::decode_payload(payload)?;
                            <$ver as $crate::LiftToCurrent<$schema>>::lift_to_current(value, ctx)
                        }
                    )+
                    other => ::core::result::Result::Err($crate::unknown_version_error(
                        table,
                        other,
                        <Self as $crate::VersionedTable>::CURRENT_VERSION,
                    )),
                }
            }

            fn encode_tagged(
                value: &Self::Value,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                let mut out = ::std::vec::Vec::new();
                out.push(<Self as $crate::VersionedTable>::CURRENT_VERSION);
                out.extend_from_slice(
                    &<<$schema as $crate::Schema>::Value
                        as $crate::SchemaVersion<$schema>>::encode_payload(value)?,
                );
                ::core::result::Result::Ok(out)
            }
        }

        impl $crate::ValueCodec<$schema> for <$schema as $crate::Schema>::Value {
            fn encode_value(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                <$schema as $crate::VersionedTable>::encode_tagged(self)
            }

            fn decode_value(
                bytes: &[u8],
                ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<Self, $crate::CodecError> {
                <$schema as $crate::VersionedTable>::decode_tagged(bytes, ctx)
            }
        }
    };

    // --- per-version payload codec, named by the entry's `as` clause ---
    (@payload $schema:ty, $ver:ty, $tag:literal) => {};

    (@payload $schema:ty, $ver:ty, $tag:literal, borsh) => {
        $crate::impl_versioned_value_codec!(@payload_impl $schema, $ver, $tag,
            |bytes| ::borsh::from_slice(bytes),
            |value| ::borsh::to_vec(value)
        );
    };

    (@payload $schema:ty, $ver:ty, $tag:literal, bincode) => {
        $crate::impl_versioned_value_codec!(@payload_impl $schema, $ver, $tag,
            |bytes| ::bincode::deserialize(bytes),
            |value| ::bincode::serialize(value)
        );
    };

    (@payload $schema:ty, $ver:ty, $tag:literal, cbor) => {
        $crate::impl_versioned_value_codec!(@payload_impl $schema, $ver, $tag,
            |bytes| ::ciborium::from_reader(bytes),
            |value| {
                let mut buf = ::std::vec::Vec::new();
                ::ciborium::into_writer(value, &mut buf).map(|()| buf)
            }
        );
    };

    (@payload $schema:ty, $ver:ty, $tag:literal, codec) => {
        $crate::impl_versioned_value_codec!(@payload_impl $schema, $ver, $tag,
            |bytes| {
                use ::strata_codec::{BufDecoder, Codec};
                let mut decoder = BufDecoder::new(bytes);
                Codec::decode(&mut decoder)
            },
            |value| ::strata_codec::encode_to_vec(value)
        );
    };

    (@payload $schema:ty, $ver:ty, $tag:literal, $other:ident) => {
        ::core::compile_error!(::core::concat!(
            "unknown payload codec `",
            ::core::stringify!($other),
            "`: expected one of borsh, bincode, cbor, codec",
        ));
    };

    // The two closures carry the whole codec-specific part: each returns the
    // codec's own `Result`, which the impl maps onto `CodecError` once.
    (@payload_impl $schema:ty, $ver:ty, $tag:literal, $decode:expr, $encode:expr) => {
        impl $crate::SchemaVersion<$schema> for $ver {
            const VERSION: u8 = $tag;

            fn decode_payload(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
                let decode: fn(&[u8]) -> _ = $decode;
                decode(bytes).map_err(|e| {
                    $crate::CodecError::decode(<$schema as $crate::Schema>::NAME, e)
                })
            }

            fn encode_payload(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::CodecError> {
                let encode: fn(&Self) -> _ = $encode;
                encode(self).map_err(|e| {
                    $crate::CodecError::encode(<$schema as $crate::Schema>::NAME, e)
                })
            }
        }
    };

    // --- the last entry of a list: the current version's type and tag ---
    (@last_ty $x:ty) => { $x };
    (@last_ty $x:ty, $($rest:ty),+) => {
        $crate::impl_versioned_value_codec!(@last_ty $($rest),+)
    };

    (@last_tag $x:literal) => { $x };
    (@last_tag $x:literal, $($rest:literal),+) => {
        $crate::impl_versioned_value_codec!(@last_tag $($rest),+)
    };

    // --- the fold to current: one `UpConvert` hop per consecutive pair ---
    (@chain $schema:ty; $last:ty) => {
        impl $crate::LiftToCurrent<$schema> for $last {
            fn lift_to_current(
                self,
                _ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<
                <$schema as $crate::Schema>::Value,
                $crate::CodecError,
            > {
                ::core::result::Result::Ok(self)
            }
        }
    };
    (@chain $schema:ty; $from:ty, $to:ty $(, $rest:ty)*) => {
        impl $crate::LiftToCurrent<$schema> for $from {
            fn lift_to_current(
                self,
                ctx: &$crate::UpgradeCtx<'_>,
            ) -> ::core::result::Result<
                <$schema as $crate::Schema>::Value,
                $crate::CodecError,
            > {
                let next: $to = <$from as $crate::UpConvert<$to>>::up_convert(self, ctx)?;
                <$to as $crate::LiftToCurrent<$schema>>::lift_to_current(next, ctx)
            }
        }
        $crate::impl_versioned_value_codec!(@chain $schema; $to $(, $rest)*);
    };
}

/// Defines a table with a borsh key and a version-dispatched value.
#[macro_export]
macro_rules! define_table_versioned {
    (
        $(#[$docs:meta])* ($name:ident)
        $key:ty => { $( $tag:literal => $ver:ty $(as $codec:ident)? ),+ $(,)? }
    ) => {
        $crate::define_table!(
            $(#[$docs])* ($name)
            $key => $crate::impl_versioned_value_codec!(@last_ty $($ver),+)
        );
        $crate::impl_borsh_key_codec!($name, $key);
        $crate::impl_versioned_value_codec!($name { $($tag => $ver $(as $codec)?),+ });
    };
}

/// Defines a table with a big-endian integer key and a version-dispatched value.
#[macro_export]
macro_rules! define_table_versioned_be_key {
    (
        $(#[$docs:meta])* ($name:ident)
        $key:ty => { $( $tag:literal => $ver:ty $(as $codec:ident)? ),+ $(,)? }
    ) => {
        $crate::define_table!(
            $(#[$docs])* ($name)
            $key => $crate::impl_versioned_value_codec!(@last_ty $($ver),+)
        );
        $crate::impl_be_key_codec!($name, $key);
        $crate::impl_versioned_value_codec!($name { $($tag => $ver $(as $codec)?),+ });
    };
}
