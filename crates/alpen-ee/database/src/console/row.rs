//! Neutral decoded-row representation handed to the script shell.
//!
//! A [`Row`] is an ordered set of named [`FieldValue`]s produced by decoding a
//! table value through its production codec. It is deliberately engine-neutral:
//! the console core (this crate) never depends on the scripting engine, so the
//! script driver converts a [`Row`] into whatever dynamic value its interpreter
//! uses, and applies the type-fidelity rules (`u64`/`[u8; 32]` ride as blobs,
//! enums as validated strings) at that boundary.

use std::fmt;

/// A single decoded field value.
///
/// Widths are preserved so the script boundary can re-narrow and range-check on
/// write-back rather than guessing from a widened integer.
#[derive(Clone, Debug)]
pub enum FieldValue {
    /// An absent optional field.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 32-bit unsigned integer.
    U32(u32),
    /// A 64-bit unsigned integer (may exceed the script engine's signed range).
    U64(u64),
    /// A fixed 32-byte hash or id.
    Hash([u8; 32]),
    /// Opaque bytes.
    Bytes(Vec<u8>),
    /// An enum variant name, validated against the real variant set on write.
    Enum(String),
    /// A UTF-8 string.
    Str(String),
    /// A homogeneous or heterogeneous list of values.
    List(Vec<FieldValue>),
}

impl fmt::Display for FieldValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::U32(v) => write!(f, "{v}"),
            Self::U64(v) => write!(f, "{v}"),
            Self::Hash(h) => write!(f, "0x{}", hex(h)),
            Self::Bytes(b) => write!(f, "0x{}", hex(b)),
            Self::Enum(s) => write!(f, "{s}"),
            Self::Str(s) => write!(f, "{s:?}"),
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
        }
    }
}

/// An ordered, decoded table row.
#[derive(Clone, Debug, Default)]
pub struct Row {
    fields: Vec<(String, FieldValue)>,
}

impl Row {
    /// Creates an empty row.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a field, consuming and returning the row (builder style).
    pub fn field(mut self, name: impl Into<String>, value: FieldValue) -> Self {
        self.fields.push((name.into(), value));
        self
    }

    /// Returns the value of a field by name, if present.
    pub fn get(&self, name: &str) -> Option<&FieldValue> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
    }

    /// Returns the fields in insertion order.
    pub fn fields(&self) -> &[(String, FieldValue)] {
        &self.fields
    }
}

/// Lowercase-hex encodes bytes (no `0x` prefix).
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble"));
        out.push(char::from_digit((byte & 0xf) as u32, 16).expect("nibble"));
    }
    out
}

/// Parses a lowercase/uppercase hex string, tolerating a leading `0x`.
pub(crate) fn parse_hex(input: &str) -> Result<Vec<u8>, String> {
    let trimmed = input.strip_prefix("0x").unwrap_or(input);
    if !trimmed.len().is_multiple_of(2) {
        return Err(format!("hex string has odd length: {input:?}"));
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char)
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex digit in {input:?}"))?;
        let lo = (bytes[i + 1] as char)
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex digit in {input:?}"))?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Ok(out)
}
