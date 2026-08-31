//! Declarative macros for defining MDBX tables and their key/value codecs.
//!
//! These give table definitions a compact, uniform surface. A table is a
//! zero-sized marker type that
//! implements [`Schema`](crate::Schema); codec impls are attached to the key and
//! value types via the `impl_*_codec` macros.

/// Defines a table marker type implementing [`Schema`](crate::Schema).
///
/// Codecs are attached separately (see the `impl_*_codec` macros), or use a
/// bundling macro such as [`define_table_borsh!`](crate::define_table_borsh).
/// Append `, dup_sort` to open the table with MDBX `DUP_SORT`.
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
    ($(#[$docs:meta])* ($name:ident, dup_sort) $key:ty => $value:ty) => {
        $(#[$docs])*
        #[derive(Clone, Copy, Debug, Default)]
        pub(crate) struct $name;

        impl $crate::Schema for $name {
            const NAME: &'static str = ::core::stringify!($name);
            const DUP_SORT: bool = true;
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

            fn decode_value(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
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

            fn decode_value(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
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

            fn decode_value(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
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

            fn decode_value(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
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
/// the upstream sled stores.
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

            fn decode_value(bytes: &[u8]) -> ::core::result::Result<Self, $crate::CodecError> {
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
