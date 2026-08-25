//! Borsh table codecs for the EE sled schemas.
//!
//! `strata-db-store-sled` dropped its borsh codec macros, but the EE database
//! stores borsh-encoded records, so these keep the on-disk format unchanged.
//! They are copies of the upstream macros that were removed.

/// Implements a borsh [`ValueCodec`](typed_sled::codec::ValueCodec) for a table.
macro_rules! impl_borsh_value_codec {
    ($table_name:ident, $value:ty) => {
        impl ::typed_sled::codec::ValueCodec<$table_name> for $value {
            type Decoded = Self;

            fn encode_value(
                &self,
            ) -> ::std::result::Result<::std::vec::Vec<u8>, ::typed_sled::codec::CodecError> {
                ::borsh::to_vec(self).map_err(|err| {
                    ::typed_sled::codec::CodecError::SerializationFailed {
                        schema: $table_name::tree_name(),
                        source: err.into(),
                    }
                })
            }

            fn decode_value(
                data: ::sled::IVec,
            ) -> ::std::result::Result<Self::Decoded, ::typed_sled::codec::CodecError> {
                ::borsh::BorshDeserialize::deserialize_reader(&mut data.as_ref()).map_err(|err| {
                    ::typed_sled::codec::CodecError::DeserializationFailed {
                        schema: $table_name::tree_name(),
                        source: err.into(),
                    }
                })
            }
        }
    };
}

/// Defines a table with borsh codecs for both key and value.
macro_rules! define_table_with_default_codec {
    ($(#[$docs:meta])+ ($table_name:ident) $key:ty => $value:ty) => {
        ::strata_db_store_sled::define_table_without_codec!($(#[$docs])+ ( $table_name ) $key => $value);

        impl ::typed_sled::codec::KeyCodec<$table_name> for $key {
            fn encode_key(&self) -> ::std::result::Result<::std::vec::Vec<u8>, ::typed_sled::codec::CodecError> {
                ::borsh::to_vec(self).map_err(Into::into)
            }

            fn decode_key(data: &[u8]) -> ::std::result::Result<Self, ::typed_sled::codec::CodecError> {
                ::borsh::BorshDeserialize::deserialize_reader(&mut &data[..]).map_err(Into::into)
            }
        }

        impl_borsh_value_codec!($table_name, $value);
    };
}

pub(crate) use define_table_with_default_codec;
pub(crate) use impl_borsh_value_codec;
