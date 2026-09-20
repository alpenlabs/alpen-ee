//! Conversion between a table's stored value and its [`FieldValue`] form, both
//! ways.
//!
//! Reflection is a *strategy* rather than a trait on the value type. A table
//! names the [`ValueReflector`] it wants, and the reflector decides how to read
//! the value's shape — today through `serde`, tomorrow through borsh schemas,
//! `strata-codec`, or a hand-written mapping for one awkward table.
//!
//! Choosing at the table rather than at the value type is what makes both
//! halves possible at once: [`SerdeReflector`] can blanket-cover every
//! `Serialize`/`Deserialize` value with no per-table code, *and* a specific
//! table can still opt into its own strategy, because the two never compete for
//! the same impl. A trait implemented on the value type could only have one or
//! the other, since Rust has no specialization.
//!
//! # Round-trip fidelity
//!
//! Writing a field is a read-modify-write: decode, change one field, encode the
//! whole value back. So `to_value` followed by `from_value` must reproduce the
//! original *exactly*, or an edit would silently rewrite fields it was never
//! asked to touch. [`FieldValue`] mirrors serde's model precisely for that
//! reason, and the tests below check the property over generated values rather
//! than assuming it.

use std::{fmt, marker::PhantomData};

use serde::{
    de::{
        self, DeserializeOwned, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess,
        SeqAccess, VariantAccess, Visitor,
    },
    ser::{
        Error as SerError, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant,
        SerializeTuple, SerializeTupleStruct, SerializeTupleVariant,
    },
    Deserializer, Serialize, Serializer,
};

use super::value::FieldValue;

/// Converts a table's stored value of type `V` to [`FieldValue`] and back.
///
/// Implementations are strategy markers, not values: the reflector is chosen by
/// the table declaration and never constructed. A reflector deals only in
/// values — where a value is stored is the record's business, not its own.
pub trait ValueReflector<V> {
    /// Reflects `value` into its decoded form.
    fn to_value(value: &V) -> Result<FieldValue, ReflectError>;

    /// Rebuilds a value from a form produced by [`Self::to_value`].
    fn from_value(value: &FieldValue) -> Result<V, ReflectError>;
}

/// Reflects any `Serialize` + `DeserializeOwned` value through serde.
///
/// The default, covering every table whose value implements both — which, for
/// the EE store, is all of them — with no per-table code. The serde shape is
/// preserved as-is: nested structs stay nested and enums keep their variant
/// identity, so what the console shows matches how the Rust type is written.
#[derive(Debug)]
pub struct SerdeReflector;

impl<V: Serialize + DeserializeOwned> ValueReflector<V> for SerdeReflector {
    fn to_value(value: &V) -> Result<FieldValue, ReflectError> {
        value.serialize(ValueSerializer)
    }

    fn from_value(value: &FieldValue) -> Result<V, ReflectError> {
        V::deserialize(FieldDeserializer(value))
    }
}

/// Reflects a table whose whole value is a byte vector as a single byte string.
///
/// Through serde a `Vec<u8>` is a *sequence*, which would reflect as one value
/// per byte — for a megabyte payload, a million allocations twice over before
/// a predicate runs. This reflector is the table's own statement that the value
/// is bytes, so no guess is involved and the round trip is exact.
#[derive(Debug)]
pub struct BytesReflector;

impl ValueReflector<Vec<u8>> for BytesReflector {
    fn to_value(value: &Vec<u8>) -> Result<FieldValue, ReflectError> {
        Ok(FieldValue::Bytes(value.clone()))
    }

    fn from_value(value: &FieldValue) -> Result<Vec<u8>, ReflectError> {
        match value {
            FieldValue::Bytes(bytes) => Ok(bytes.clone()),
            other => Err(mismatch("bytes", other)),
        }
    }
}

/// A serde-shaped stand-in for a value whose own serde shape is unsuitable.
///
/// The usual reason is a byte vector inside a type the repository does not
/// own, which serde sees as a sequence. A mirror redeclares the same fields
/// under the same names, marking such fields as bytes, and converts to and
/// from the real value. It is the console-side counterpart of the store's own
/// `DB*` mirror types, and the same rule applies: the mirror must carry every
/// field, or [`MirrorReflector`]'s round trip fails at canonicalisation and
/// the table is refused for writes.
pub trait Mirror<V>: Serialize + DeserializeOwned {
    /// Builds the mirror from the real value.
    fn mirror(value: &V) -> Self;

    /// Rebuilds the real value from the mirror.
    fn restore(self) -> V;
}

/// Reflects a value through its [`Mirror`], so the mirror's serde shape is what
/// the console shows and edits.
#[derive(Debug)]
pub struct MirrorReflector<M>(PhantomData<fn() -> M>);

impl<V, M: Mirror<V>> ValueReflector<V> for MirrorReflector<M> {
    fn to_value(value: &V) -> Result<FieldValue, ReflectError> {
        M::mirror(value).serialize(ValueSerializer)
    }

    fn from_value(value: &FieldValue) -> Result<V, ReflectError> {
        Ok(M::deserialize(FieldDeserializer(value))?.restore())
    }
}

/// A reflector that refuses, for a value whose shape no strategy can read.
///
/// Keeps such a table listed and countable instead of blocking the registry.
#[derive(Debug)]
pub struct Unreflectable;

impl<V> ValueReflector<V> for Unreflectable {
    fn to_value(_value: &V) -> Result<FieldValue, ReflectError> {
        Err(ReflectError(
            "no reflector configured for this table's value type".to_owned(),
        ))
    }

    fn from_value(_value: &FieldValue) -> Result<V, ReflectError> {
        Err(ReflectError(
            "no reflector configured for this table's value type".to_owned(),
        ))
    }
}

/// Reports a value that could not be converted in either direction.
#[derive(Debug, Clone, thiserror::Error)]
#[error("cannot reflect value: {0}")]
pub struct ReflectError(String);

impl SerError for ReflectError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

impl de::Error for ReflectError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

// --- value -> FieldValue ---------------------------------------------------

/// A [`Serializer`] whose output is a [`FieldValue`].
struct ValueSerializer;

/// The `Ok`/`Error` pair every `serialize_*` method shares.
type FieldResult = Result<FieldValue, ReflectError>;

impl Serializer for ValueSerializer {
    type Ok = FieldValue;
    type Error = ReflectError;
    type SerializeSeq = SeqBuilder;
    type SerializeTuple = SeqBuilder;
    type SerializeTupleStruct = SeqBuilder;
    type SerializeTupleVariant = VariantSeqBuilder;
    type SerializeMap = MapBuilder;
    type SerializeStruct = StructBuilder;
    type SerializeStructVariant = VariantStructBuilder;

    fn serialize_bool(self, v: bool) -> FieldResult {
        Ok(FieldValue::Bool(v))
    }

    fn serialize_i8(self, v: i8) -> FieldResult {
        Ok(FieldValue::I64(v.into()))
    }

    fn serialize_i16(self, v: i16) -> FieldResult {
        Ok(FieldValue::I64(v.into()))
    }

    fn serialize_i32(self, v: i32) -> FieldResult {
        Ok(FieldValue::I64(v.into()))
    }

    fn serialize_i64(self, v: i64) -> FieldResult {
        Ok(FieldValue::I64(v))
    }

    fn serialize_i128(self, v: i128) -> FieldResult {
        Ok(FieldValue::I128(v))
    }

    fn serialize_u8(self, v: u8) -> FieldResult {
        Ok(FieldValue::U64(v.into()))
    }

    fn serialize_u16(self, v: u16) -> FieldResult {
        Ok(FieldValue::U64(v.into()))
    }

    fn serialize_u32(self, v: u32) -> FieldResult {
        Ok(FieldValue::U64(v.into()))
    }

    fn serialize_u64(self, v: u64) -> FieldResult {
        Ok(FieldValue::U64(v))
    }

    fn serialize_u128(self, v: u128) -> FieldResult {
        Ok(FieldValue::U128(v))
    }

    fn serialize_f32(self, v: f32) -> FieldResult {
        Ok(FieldValue::F64(v.into()))
    }

    fn serialize_f64(self, v: f64) -> FieldResult {
        Ok(FieldValue::F64(v))
    }

    fn serialize_char(self, v: char) -> FieldResult {
        Ok(FieldValue::Str(v.to_string()))
    }

    fn serialize_str(self, v: &str) -> FieldResult {
        Ok(FieldValue::Str(v.to_owned()))
    }

    fn serialize_bytes(self, v: &[u8]) -> FieldResult {
        Ok(FieldValue::Bytes(v.to_vec()))
    }

    fn serialize_none(self) -> FieldResult {
        Ok(FieldValue::Null)
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> FieldResult {
        value.serialize(self)
    }

    fn serialize_unit(self) -> FieldResult {
        Ok(FieldValue::Unit)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> FieldResult {
        Ok(FieldValue::Unit)
    }

    /// A fieldless variant is the case predicates lean on most
    /// (`r.status == "Completed"`), so it stays a bare name.
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> FieldResult {
        Ok(FieldValue::Enum(variant.to_owned()))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> FieldResult {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> FieldResult {
        Ok(FieldValue::Variant {
            name: variant.to_owned(),
            value: Box::new(value.serialize(ValueSerializer)?),
        })
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<SeqBuilder, ReflectError> {
        Ok(SeqBuilder {
            items: Vec::with_capacity(len.unwrap_or(0)),
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<SeqBuilder, ReflectError> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<SeqBuilder, ReflectError> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<VariantSeqBuilder, ReflectError> {
        Ok(VariantSeqBuilder {
            variant,
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_map(self, len: Option<usize>) -> Result<MapBuilder, ReflectError> {
        Ok(MapBuilder {
            entries: Vec::with_capacity(len.unwrap_or(0)),
            pending_key: None,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<StructBuilder, ReflectError> {
        Ok(StructBuilder {
            entries: Vec::with_capacity(len),
        })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<VariantStructBuilder, ReflectError> {
        Ok(VariantStructBuilder {
            variant,
            entries: Vec::with_capacity(len),
        })
    }
}

/// Collects sequence and tuple elements.
struct SeqBuilder {
    items: Vec<FieldValue>,
}

impl SeqBuilder {
    fn push<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), ReflectError> {
        self.items.push(value.serialize(ValueSerializer)?);
        Ok(())
    }
}

impl SerializeSeq for SeqBuilder {
    type Ok = FieldValue;
    type Error = ReflectError;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), ReflectError> {
        self.push(value)
    }

    fn end(self) -> FieldResult {
        Ok(FieldValue::List(self.items))
    }
}

impl SerializeTuple for SeqBuilder {
    type Ok = FieldValue;
    type Error = ReflectError;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), ReflectError> {
        self.push(value)
    }

    fn end(self) -> FieldResult {
        Ok(FieldValue::List(self.items))
    }
}

impl SerializeTupleStruct for SeqBuilder {
    type Ok = FieldValue;
    type Error = ReflectError;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), ReflectError> {
        self.push(value)
    }

    fn end(self) -> FieldResult {
        Ok(FieldValue::List(self.items))
    }
}

/// Collects a tuple variant's elements under its variant name.
struct VariantSeqBuilder {
    variant: &'static str,
    items: Vec<FieldValue>,
}

impl SerializeTupleVariant for VariantSeqBuilder {
    type Ok = FieldValue;
    type Error = ReflectError;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), ReflectError> {
        self.items.push(value.serialize(ValueSerializer)?);
        Ok(())
    }

    fn end(self) -> FieldResult {
        Ok(FieldValue::Variant {
            name: self.variant.to_owned(),
            value: Box::new(FieldValue::List(self.items)),
        })
    }
}

/// Collects map entries, keeping each key as a value rather than a name.
struct MapBuilder {
    entries: Vec<(FieldValue, FieldValue)>,
    pending_key: Option<FieldValue>,
}

impl SerializeMap for MapBuilder {
    type Ok = FieldValue;
    type Error = ReflectError;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), ReflectError> {
        self.pending_key = Some(key.serialize(ValueSerializer)?);
        Ok(())
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), ReflectError> {
        let key = self
            .pending_key
            .take()
            .ok_or_else(|| ReflectError("map value without a key".to_owned()))?;
        self.entries.push((key, value.serialize(ValueSerializer)?));
        Ok(())
    }

    fn end(self) -> FieldResult {
        Ok(FieldValue::Map(self.entries))
    }
}

/// Collects a struct's named fields.
struct StructBuilder {
    entries: Vec<(FieldValue, FieldValue)>,
}

impl SerializeStruct for StructBuilder {
    type Ok = FieldValue;
    type Error = ReflectError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        name: &'static str,
        value: &T,
    ) -> Result<(), ReflectError> {
        self.entries.push((
            FieldValue::Str(name.to_owned()),
            value.serialize(ValueSerializer)?,
        ));
        Ok(())
    }

    fn end(self) -> FieldResult {
        Ok(FieldValue::Map(self.entries))
    }
}

/// Collects a struct variant's fields under its variant name.
struct VariantStructBuilder {
    variant: &'static str,
    entries: Vec<(FieldValue, FieldValue)>,
}

impl SerializeStructVariant for VariantStructBuilder {
    type Ok = FieldValue;
    type Error = ReflectError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        name: &'static str,
        value: &T,
    ) -> Result<(), ReflectError> {
        self.entries.push((
            FieldValue::Str(name.to_owned()),
            value.serialize(ValueSerializer)?,
        ));
        Ok(())
    }

    fn end(self) -> FieldResult {
        Ok(FieldValue::Variant {
            name: self.variant.to_owned(),
            value: Box::new(FieldValue::Map(self.entries)),
        })
    }
}

// --- FieldValue -> value ---------------------------------------------------

/// A [`Deserializer`] reading back out of a [`FieldValue`].
///
/// Integers were widened on the way in, so every narrowing here is checked: a
/// value that does not fit is a bad edit, not something to truncate silently.
struct FieldDeserializer<'a>(&'a FieldValue);

/// Builds the "expected X, found Y" error every narrowing path shares.
fn mismatch(expected: &str, found: &FieldValue) -> ReflectError {
    ReflectError(format!("expected {expected}, found {found}"))
}

macro_rules! deserialize_int {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
            let value = match self.0 {
                FieldValue::U64(v) => {
                    <$ty>::try_from(*v).map_err(|_| mismatch(stringify!($ty), self.0))
                }
                FieldValue::I64(v) => {
                    <$ty>::try_from(*v).map_err(|_| mismatch(stringify!($ty), self.0))
                }
                FieldValue::U128(v) => {
                    <$ty>::try_from(*v).map_err(|_| mismatch(stringify!($ty), self.0))
                }
                FieldValue::I128(v) => {
                    <$ty>::try_from(*v).map_err(|_| mismatch(stringify!($ty), self.0))
                }
                // A decimal string is how an integer wider than the script's own
                // integer type is written, and how a map keyed by one is typed.
                // Unambiguous here because serde only asks for an integer when
                // the field is one; a wrong string fails to parse.
                FieldValue::Str(text) => text
                    .parse::<$ty>()
                    .map_err(|_| mismatch(stringify!($ty), self.0)),
                other => Err(mismatch(stringify!($ty), other)),
            }?;
            visitor.$visit(value)
        }
    };
}

impl<'de> Deserializer<'de> for FieldDeserializer<'_> {
    type Error = ReflectError;

    /// Self-describing: the value says what it holds without being told, which
    /// is what makes `deserialize_any` and `#[serde(flatten)]` work.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::Null => visitor.visit_none(),
            FieldValue::Unit => visitor.visit_unit(),
            FieldValue::Bool(v) => visitor.visit_bool(*v),
            FieldValue::U64(v) => visitor.visit_u64(*v),
            FieldValue::I64(v) => visitor.visit_i64(*v),
            FieldValue::U128(v) => visitor.visit_u128(*v),
            FieldValue::I128(v) => visitor.visit_i128(*v),
            FieldValue::F64(v) => visitor.visit_f64(*v),
            FieldValue::Str(v) => visitor.visit_str(v),
            FieldValue::Bytes(v) => visitor.visit_bytes(v),
            FieldValue::Enum(_) | FieldValue::Variant { .. } => {
                self.deserialize_enum("", &[], visitor)
            }
            FieldValue::List(items) => visitor.visit_seq(SeqReader { items, index: 0 }),
            FieldValue::Map(entries) => visitor.visit_map(MapReader {
                entries,
                index: 0,
                value: None,
            }),
        }
    }

    deserialize_int!(deserialize_i8, visit_i8, i8);
    deserialize_int!(deserialize_i16, visit_i16, i16);
    deserialize_int!(deserialize_i32, visit_i32, i32);
    deserialize_int!(deserialize_i64, visit_i64, i64);
    deserialize_int!(deserialize_u8, visit_u8, u8);
    deserialize_int!(deserialize_u16, visit_u16, u16);
    deserialize_int!(deserialize_u32, visit_u32, u32);
    deserialize_int!(deserialize_u64, visit_u64, u64);
    deserialize_int!(deserialize_i128, visit_i128, i128);
    deserialize_int!(deserialize_u128, visit_u128, u128);

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::Bool(v) => visitor.visit_bool(*v),
            other => Err(mismatch("a bool", other)),
        }
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            // `f32` widened to `f64` on the way in, so narrowing back is exact.
            FieldValue::F64(v) => visitor.visit_f32(*v as f32),
            other => Err(mismatch("a float", other)),
        }
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::F64(v) => visitor.visit_f64(*v),
            other => Err(mismatch("a float", other)),
        }
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::Str(s) => {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => visitor.visit_char(c),
                    _ => Err(mismatch("a single character", self.0)),
                }
            }
            other => Err(mismatch("a single character", other)),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::Str(v) | FieldValue::Enum(v) => visitor.visit_str(v),
            other => Err(mismatch("a string", other)),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::Bytes(v) => visitor.visit_bytes(v),
            FieldValue::List(_) => self.deserialize_seq(visitor),
            other => Err(mismatch("bytes", other)),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::Unit => visitor.visit_unit(),
            other => Err(mismatch("unit", other)),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ReflectError> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ReflectError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::List(items) => visitor.visit_seq(SeqReader { items, index: 0 }),
            FieldValue::Bytes(bytes) => visitor.visit_seq(ByteReader { bytes, index: 0 }),
            other => Err(mismatch("a sequence", other)),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, ReflectError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, ReflectError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        match self.0 {
            FieldValue::Map(entries) => visitor.visit_map(MapReader {
                entries,
                index: 0,
                value: None,
            }),
            other => Err(mismatch("a map", other)),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ReflectError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ReflectError> {
        match self.0 {
            // A plain string is accepted as a fieldless variant's name so an
            // edit can be typed at a prompt that has no enum literal. Serde
            // still checks the name against the real variant set, and what is
            // written back is the canonical `Enum` form, so this widens what is
            // accepted without widening what is produced.
            FieldValue::Enum(name) | FieldValue::Str(name) => visitor.visit_enum(EnumReader {
                name,
                payload: None,
            }),
            FieldValue::Variant { name, value } => visitor.visit_enum(EnumReader {
                name,
                payload: Some(value),
            }),
            // A one-entry map is how a payload-carrying variant is written where
            // there is no variant literal. It is unambiguous here because serde
            // only reaches this method for a field that really is an enum; a
            // field that is genuinely a map is read by `deserialize_map`.
            FieldValue::Map(entries) => match entries.as_slice() {
                [(FieldValue::Str(name), payload)] => visitor.visit_enum(EnumReader {
                    name,
                    payload: Some(payload),
                }),
                _ => Err(mismatch(
                    "an enum, or a one-entry map naming a variant",
                    self.0,
                )),
            },
            other => Err(mismatch("an enum", other)),
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReflectError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, ReflectError> {
        self.deserialize_any(visitor)
    }
}

/// Walks a [`FieldValue::List`].
struct SeqReader<'a> {
    items: &'a [FieldValue],
    index: usize,
}

impl<'de> SeqAccess<'de> for SeqReader<'_> {
    type Error = ReflectError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, ReflectError> {
        let Some(item) = self.items.get(self.index) else {
            return Ok(None);
        };
        self.index += 1;
        seed.deserialize(FieldDeserializer(item)).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len() - self.index)
    }
}

/// Walks a [`FieldValue::Bytes`] element by element, for a type that stores
/// bytes but reads them as a sequence.
struct ByteReader<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl<'de> SeqAccess<'de> for ByteReader<'_> {
    type Error = ReflectError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, ReflectError> {
        let Some(byte) = self.bytes.get(self.index) else {
            return Ok(None);
        };
        self.index += 1;
        seed.deserialize(u64::from(*byte).into_deserializer())
            .map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.bytes.len() - self.index)
    }
}

/// Walks a [`FieldValue::Map`], handing back each key then its value.
struct MapReader<'a> {
    entries: &'a [(FieldValue, FieldValue)],
    index: usize,
    value: Option<&'a FieldValue>,
}

impl<'de> MapAccess<'de> for MapReader<'_> {
    type Error = ReflectError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, ReflectError> {
        let Some((key, value)) = self.entries.get(self.index) else {
            return Ok(None);
        };
        self.index += 1;
        self.value = Some(value);
        seed.deserialize(FieldDeserializer(key)).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, ReflectError> {
        let value = self
            .value
            .take()
            .ok_or_else(|| ReflectError("map value requested before its key".to_owned()))?;
        seed.deserialize(FieldDeserializer(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len() - self.index)
    }
}

/// Presents an enum variant and its payload.
struct EnumReader<'a> {
    name: &'a str,
    payload: Option<&'a FieldValue>,
}

impl<'de> EnumAccess<'de> for EnumReader<'_> {
    type Error = ReflectError;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self), ReflectError> {
        let name = seed.deserialize(self.name.into_deserializer())?;
        Ok((name, self))
    }
}

impl<'de> VariantAccess<'de> for EnumReader<'_> {
    type Error = ReflectError;

    fn unit_variant(self) -> Result<(), ReflectError> {
        match self.payload {
            None => Ok(()),
            Some(other) => Err(mismatch("a fieldless variant", other)),
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, ReflectError> {
        let payload = self
            .payload
            .ok_or_else(|| ReflectError(format!("variant `{}` carries no payload", self.name)))?;
        seed.deserialize(FieldDeserializer(payload))
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, ReflectError> {
        let payload = self
            .payload
            .ok_or_else(|| ReflectError(format!("variant `{}` carries no payload", self.name)))?;
        FieldDeserializer(payload).deserialize_seq(visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ReflectError> {
        let payload = self
            .payload
            .ok_or_else(|| ReflectError(format!("variant `{}` carries no payload", self.name)))?;
        FieldDeserializer(payload).deserialize_map(visitor)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Nested {
        depth: u32,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Status {
        Idle,
        Counted(u64),
        Pair(u8, String),
        Blocked { reason: String, attempts: u32 },
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        id: u64,
        signed: i64,
        huge: u128,
        ratio: f64,
        narrow: f32,
        flag: bool,
        letter: char,
        status: Status,
        nested: Nested,
        tags: Vec<String>,
        fixed: [u8; 4],
        missing: Option<u32>,
        present: Option<u32>,
        nothing: Option<()>,
        something: Option<()>,
        keyed: BTreeMap<u64, String>,
        unit: (),
    }

    fn round_trip<T: Serialize + DeserializeOwned>(value: &T) -> T {
        let decoded = SerdeReflector::to_value(value).expect("to_value");
        SerdeReflector::from_value(&decoded).expect("from_value")
    }

    fn sample(id: u64, status: Status) -> Record {
        Record {
            id,
            signed: -42,
            huge: u128::MAX,
            ratio: 0.5,
            narrow: 0.25,
            flag: true,
            letter: 'ß',
            status,
            nested: Nested { depth: 3 },
            tags: vec!["a".to_owned(), "b".to_owned()],
            fixed: [1, 2, 3, 4],
            missing: None,
            present: Some(9),
            nothing: None,
            something: Some(()),
            keyed: BTreeMap::from([(1u64, "one".to_owned()), (2, "two".to_owned())]),
            unit: (),
        }
    }

    /// The property the write path rests on: a value that goes out to its
    /// decoded form and back is unchanged, or editing one field would rewrite
    /// others.
    #[test]
    fn every_shape_round_trips_exactly() {
        for status in [
            Status::Idle,
            Status::Counted(7),
            Status::Pair(2, "x".to_owned()),
            Status::Blocked {
                reason: "waiting".to_owned(),
                attempts: 3,
            },
        ] {
            let original = sample(1, status);
            assert_eq!(round_trip(&original), original);
        }
    }

    /// `Some(())` and `None` are different values, and a representation that
    /// could not tell
    /// them apart would flip one into the other on write-back.
    #[test]
    fn an_optional_unit_keeps_its_presence() {
        let decoded = SerdeReflector::to_value(&sample(1, Status::Idle)).unwrap();
        assert_eq!(decoded.get("nothing"), Some(&FieldValue::Null));
        assert_eq!(decoded.get("something"), Some(&FieldValue::Unit));
    }

    /// A map keyed by something other than a string has to survive, so keys are
    /// values rather than names.
    #[test]
    fn a_map_with_integer_keys_survives() {
        let decoded = SerdeReflector::to_value(&sample(1, Status::Idle)).unwrap();
        let Some(FieldValue::Map(entries)) = decoded.get("keyed") else {
            panic!("keyed map lost its shape: {decoded}");
        };
        assert_eq!(entries[0].0, FieldValue::U64(1));
    }

    /// A fixed byte array is a sequence as far as serde is concerned; calling it
    /// bytes would be a guess, and a guess cannot round-trip.
    #[test]
    fn a_byte_array_stays_a_sequence() {
        let decoded = SerdeReflector::to_value(&sample(1, Status::Idle)).unwrap();
        assert!(
            matches!(decoded.get("fixed"), Some(FieldValue::List(items)) if items.len() == 4),
            "byte array was guessed at: {decoded}"
        );
    }

    /// A value that is not struct-shaped is that value, not a struct wrapping
    /// it: a `u64` table's value decodes to a number.
    #[test]
    fn a_scalar_table_value_stays_scalar() {
        let index: u64 = u64::MAX;
        assert_eq!(
            SerdeReflector::to_value(&index).unwrap(),
            FieldValue::U64(u64::MAX)
        );
        assert_eq!(round_trip(&index), index);

        let bytes: Vec<u8> = vec![1, 2, 3];
        assert_eq!(round_trip(&bytes), bytes);
        assert_eq!(round_trip(&()), ());
    }

    /// Editing one field must leave every other field exactly as decoded.
    #[test]
    fn replacing_one_field_leaves_the_rest_untouched() {
        let original = sample(1, Status::Idle);
        let mut decoded = SerdeReflector::to_value(&original).unwrap();
        assert!(decoded.replace_field("id", FieldValue::U64(99)));

        let edited: Record = SerdeReflector::from_value(&decoded).unwrap();
        assert_eq!(edited.id, 99);
        assert_eq!(Record { id: 1, ..edited }, original);
    }

    /// An edit that does not fit the field's real type is refused, never
    /// truncated.
    #[test]
    fn an_out_of_range_edit_is_refused() {
        let original = sample(1, Status::Idle);

        let mut decoded = SerdeReflector::to_value(&original).unwrap();
        decoded.replace_field("nested", FieldValue::U64(1));
        assert!(<SerdeReflector as ValueReflector<Record>>::from_value(&decoded).is_err());

        let mut decoded = SerdeReflector::to_value(&original).unwrap();
        decoded.replace_field(
            "nested",
            FieldValue::Map(vec![(
                FieldValue::Str("depth".to_owned()),
                FieldValue::U64(u64::from(u32::MAX) + 1),
            )]),
        );
        let err = <SerdeReflector as ValueReflector<Record>>::from_value(&decoded).unwrap_err();
        assert!(err.to_string().contains("u32"), "{err}");
    }

    /// A value with a field of its own called `key` must survive the trip. A
    /// record's key lives on the record, never inside the value, so the two can
    /// never be confused.
    #[test]
    fn a_value_with_its_own_key_field_survives() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct HasKey {
            key: String,
            payload: u64,
        }

        let original = HasKey {
            key: "mine".to_owned(),
            payload: 7,
        };

        let stored = super::super::value::Record::new(
            "0102ab",
            SerdeReflector::to_value(&original).unwrap(),
        );
        assert_eq!(stored.key, "0102ab");
        assert_eq!(
            stored.value.get("key"),
            Some(&FieldValue::Str("mine".to_owned())),
            "the value's own `key` field was displaced"
        );

        let rebuilt: HasKey = SerdeReflector::from_value(&stored.value).unwrap();
        assert_eq!(rebuilt, original);
    }

    /// The prompt has no literal for these shapes, so they are written in a
    /// looser form and resolved against the field's real type. What comes back
    /// out is always the canonical form — the loose spelling is input-only, so
    /// the round-trip property is untouched.
    #[test]
    fn loose_input_resolves_against_the_target_type() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        enum Shape {
            Plain,
            Tagged { note: String, count: u32 },
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Holder {
            shape: Shape,
            wide: u128,
            narrow: u64,
        }

        // A fieldless variant by bare name, a payload variant as a one-entry
        // map, and integers beyond the script's own range as decimal strings.
        let loose = FieldValue::Map(vec![
            (
                FieldValue::Str("shape".to_owned()),
                FieldValue::Map(vec![(
                    FieldValue::Str("Tagged".to_owned()),
                    FieldValue::Map(vec![
                        (
                            FieldValue::Str("note".to_owned()),
                            FieldValue::Str("hi".to_owned()),
                        ),
                        (FieldValue::Str("count".to_owned()), FieldValue::U64(3)),
                    ]),
                )]),
            ),
            (
                FieldValue::Str("wide".to_owned()),
                FieldValue::Str(u128::MAX.to_string()),
            ),
            (
                FieldValue::Str("narrow".to_owned()),
                FieldValue::Str(u64::MAX.to_string()),
            ),
        ]);

        let built: Holder = SerdeReflector::from_value(&loose).expect("loose input resolves");
        assert_eq!(
            built,
            Holder {
                shape: Shape::Tagged {
                    note: "hi".to_owned(),
                    count: 3
                },
                wide: u128::MAX,
                narrow: u64::MAX,
            }
        );

        // Canonicalising it yields the exact form a decode produces.
        let canonical = SerdeReflector::to_value(&built).unwrap();
        assert!(
            matches!(canonical.get("shape"), Some(FieldValue::Variant { name, .. }) if name == "Tagged"),
            "a variant was stored as a map: {canonical}"
        );
        assert_eq!(canonical.get("wide"), Some(&FieldValue::U128(u128::MAX)));
        assert_eq!(canonical.get("narrow"), Some(&FieldValue::U64(u64::MAX)));

        let loose_plain = FieldValue::Map(vec![
            (
                FieldValue::Str("shape".to_owned()),
                FieldValue::Str("Plain".to_owned()),
            ),
            (FieldValue::Str("wide".to_owned()), FieldValue::U64(1)),
            (FieldValue::Str("narrow".to_owned()), FieldValue::U64(2)),
        ]);
        let plain: Holder = SerdeReflector::from_value(&loose_plain).unwrap();
        assert_eq!(plain.shape, Shape::Plain);
        assert_eq!(
            SerdeReflector::to_value(&plain).unwrap().get("shape"),
            Some(&FieldValue::Enum("Plain".to_owned()))
        );
    }

    /// Loose input is resolved, never guessed: anything the target type cannot
    /// account for is refused.
    #[test]
    fn loose_input_that_does_not_fit_the_type_is_refused() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        enum Shape {
            Plain,
            Tagged { note: String },
        }

        let unknown_variant = FieldValue::Map(vec![(
            FieldValue::Str("Nope".to_owned()),
            FieldValue::Map(vec![]),
        )]);
        let err = <SerdeReflector as ValueReflector<Shape>>::from_value(&unknown_variant)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Nope"), "{err}");

        let two_keys = FieldValue::Map(vec![
            (FieldValue::Str("Plain".to_owned()), FieldValue::Unit),
            (FieldValue::Str("Tagged".to_owned()), FieldValue::Unit),
        ]);
        assert!(<SerdeReflector as ValueReflector<Shape>>::from_value(&two_keys).is_err());

        // A string that is not a number stays a type error for an integer field.
        let not_a_number = FieldValue::Str("twelve".to_owned());
        assert!(<SerdeReflector as ValueReflector<u64>>::from_value(&not_a_number).is_err());

        // And one that overflows the field is refused rather than truncated.
        let too_big = FieldValue::Str(u128::MAX.to_string());
        assert!(<SerdeReflector as ValueReflector<u64>>::from_value(&too_big).is_err());
    }

    /// A byte-vector table reflects as one value, and refuses anything but
    /// bytes on the way back rather than guessing at a list of integers.
    #[test]
    fn a_bytes_table_reflects_as_one_byte_string() {
        let payload: Vec<u8> = (0..=255).collect();
        let reflected = BytesReflector::to_value(&payload).unwrap();
        assert_eq!(reflected, FieldValue::Bytes(payload.clone()));
        assert_eq!(BytesReflector::from_value(&reflected).unwrap(), payload);

        let as_list = FieldValue::List(vec![FieldValue::U64(1)]);
        assert!(BytesReflector::from_value(&as_list).is_err());
    }

    proptest! {
        /// The same property over generated values, covering shapes the
        /// hand-written cases did not think of.
        #[test]
        fn generated_records_round_trip(
            id in any::<u64>(),
            signed in any::<i64>(),
            huge in any::<u128>(),
            ratio in any::<f64>().prop_filter("NaN never equals itself", |v| !v.is_nan()),
            narrow in any::<f32>().prop_filter("NaN never equals itself", |v| !v.is_nan()),
            flag in any::<bool>(),
            letter in any::<char>(),
            depth in any::<u32>(),
            tags in prop::collection::vec(".*", 0..4),
            fixed in any::<[u8; 4]>(),
            missing in any::<Option<u32>>(),
            reason in ".*",
            attempts in any::<u32>(),
            keyed in prop::collection::btree_map(any::<u64>(), ".*", 0..4),
        ) {
            let original = Record {
                id,
                signed,
                huge,
                ratio,
                narrow,
                flag,
                letter,
                status: Status::Blocked { reason, attempts },
                nested: Nested { depth },
                tags,
                fixed,
                missing,
                present: Some(1),
                nothing: None,
                something: Some(()),
                keyed,
                unit: (),
            };
            prop_assert_eq!(round_trip(&original), original);
        }
    }
}
