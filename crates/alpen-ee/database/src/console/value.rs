//! The decoded representation of a stored record.
//!
//! A [`Record`] is a key and a value — the same two halves MDBX stores, named
//! the same way at every layer: in these types, in the reflector, and at the
//! prompt. A [`FieldValue`] is one decoded value, whatever its shape: a struct
//! is a [`FieldValue::Map`] of named fields, a `u64` table's value is a
//! [`FieldValue::U64`], and neither is dressed up as the other.
//!
//! # Fidelity
//!
//! [`FieldValue`] is a **lossless mirror of serde's data model**, because
//! editing a field is a read-modify-write: the value is decoded, one field is
//! changed, and the whole thing is encoded back. Anything the representation
//! could not hold would be silently rewritten in fields nobody asked to touch —
//! unacceptable for a database.
//!
//! That is why several distinctions here look redundant but are not:
//!
//! * [`FieldValue::Null`] (`None`) is separate from [`FieldValue::Unit`] (`()`), so `Option<()>`
//!   survives.
//! * [`FieldValue::Bytes`] comes *only* from `serialize_bytes`. A `[u8; 32]` reaches serde as a
//!   sequence of integers and stays [`FieldValue::List`], because nothing in the data says
//!   otherwise. Showing it as hex is the renderer's business, not this type's.
//! * [`FieldValue::Map`] holds key/value *pairs* keyed by [`FieldValue`], not by `String`, so a map
//!   with integer or structured keys round-trips. These are map keys inside a value, unrelated to
//!   the record key.
//! * [`FieldValue::Variant`] is distinct from a one-entry [`FieldValue::Map`], so an enum payload
//!   is never confused with a map that has one key.
//! * Integers widen on the way in and are range-checked on the way out, which is lossless because
//!   the widened value came from the narrower type.
//!
//! [`crate::console::reflect`] proves the round-trip rather than asserting it.

use std::fmt;

use hex::FromHexError;

/// A single decoded value, in serde's shape.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    /// An absent optional value (`None`).
    Null,
    /// The unit value (`()`), or a unit struct.
    Unit,
    /// A boolean.
    Bool(bool),
    /// Any unsigned integer up to 64 bits, widened.
    U64(u64),
    /// Any signed integer up to 64 bits, widened.
    I64(i64),
    /// A 128-bit unsigned integer.
    U128(u128),
    /// A 128-bit signed integer.
    I128(i128),
    /// A floating-point number; `f32` widens losslessly.
    F64(f64),
    /// A UTF-8 string, or a `char`.
    Str(String),
    /// A byte string, from a type that declares itself as one.
    Bytes(Vec<u8>),
    /// A fieldless enum variant, by name.
    Enum(String),
    /// A data-carrying enum variant and its payload.
    Variant {
        /// The variant's name.
        name: String,
        /// Its payload: a value, a list, or a map of its named fields.
        value: Box<FieldValue>,
    },
    /// A sequence, tuple, or tuple struct.
    List(Vec<FieldValue>),
    /// A struct or a map, as ordered key/value pairs.
    ///
    /// A struct's keys are [`FieldValue::Str`] field names; a map's keys are
    /// whatever the key type serializes to.
    Map(Vec<(FieldValue, FieldValue)>),
}

impl FieldValue {
    /// Returns the value under `name` if this is a struct-shaped map.
    pub fn get(&self, name: &str) -> Option<&FieldValue> {
        let Self::Map(entries) = self else {
            return None;
        };
        entries
            .iter()
            .find(|(key, _)| matches!(key, Self::Str(k) if k == name))
            .map(|(_, value)| value)
    }

    /// Returns this value's named fields, if it is struct-shaped.
    pub fn fields(&self) -> Option<Vec<(&str, &FieldValue)>> {
        let Self::Map(entries) = self else {
            return None;
        };
        entries
            .iter()
            .map(|(key, value)| match key {
                Self::Str(name) => Some((name.as_str(), value)),
                _ => None,
            })
            .collect()
    }

    /// Replaces one named field, reporting whether it existed.
    ///
    /// Only the named field is touched; every other field keeps the exact value
    /// it decoded to, which is what keeps an edit from disturbing the rest of
    /// the record.
    pub fn replace_field(&mut self, name: &str, value: FieldValue) -> bool {
        let Self::Map(entries) = self else {
            return false;
        };
        for (key, slot) in entries.iter_mut() {
            if matches!(key, Self::Str(k) if k == name) {
                *slot = value;
                return true;
            }
        }
        false
    }
}

impl fmt::Display for FieldValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "null"),
            Self::Unit => write!(f, "()"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::U64(v) => write!(f, "{v}"),
            Self::I64(v) => write!(f, "{v}"),
            Self::U128(v) => write!(f, "{v}"),
            Self::I128(v) => write!(f, "{v}"),
            Self::F64(v) => write!(f, "{v}"),
            Self::Str(s) => write!(f, "{s:?}"),
            Self::Bytes(b) => write!(f, "0x{}", hex(b)),
            Self::Enum(s) => write!(f, "{s}"),
            Self::Variant { name, value } => write!(f, "{name}({value})"),
            Self::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Self::Map(entries) => {
                write!(f, "{{ ")?;
                for (i, (key, value)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match key {
                        Self::Str(name) => write!(f, "{name}: {value}")?,
                        other => write!(f, "{other}: {value}")?,
                    }
                }
                write!(f, " }}")
            }
        }
    }
}

/// One stored record: the key it lives at, and the value it holds.
///
/// The key is kept in the form a user types and `get` accepts, so a key read out
/// of a scan pastes straight back into a lookup. Writing re-parses it through
/// the table's [`ConsoleKey`](super::key::ConsoleKey), a checked conversion
/// whose round trip is tested per key type.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// Where the value is stored, rendered as `get` accepts it.
    pub key: String,
    /// What is stored there.
    pub value: FieldValue,
}

impl Record {
    /// Pairs a key with the value stored at it.
    pub fn new(key: impl Into<String>, value: FieldValue) -> Self {
        Self {
            key: key.into(),
            value,
        }
    }
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{ key: {}, value: {} }}", self.key, self.value)
    }
}

/// Lowercase-hex encodes bytes (no `0x` prefix): the form every key and blob
/// is shown in.
pub fn hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Parses a hex string of either case, tolerating a leading `0x`.
pub fn parse_hex(input: &str) -> Result<Vec<u8>, String> {
    let trimmed = input.strip_prefix("0x").unwrap_or(input);
    hex::decode(trimmed).map_err(|err| match err {
        FromHexError::OddLength => format!("hex string has odd length: {input:?}"),
        FromHexError::InvalidHexCharacter { .. } | FromHexError::InvalidStringLength => {
            format!("invalid hex digit in {input:?}")
        }
    })
}
