//! Conversion between the console core's neutral [`Row`]/[`FieldValue`] and
//! rhai's dynamic values.
//!
//! This is the type-fidelity boundary (design doc §4.1): rhai has a single
//! integer type, so a `u64` beyond the signed range and every byte/hash field
//! ride across as a blob rather than being silently truncated. Enums cross as
//! strings so predicates read naturally (`t.status == "Pending"`).

use alpen_ee_database::console::{FieldValue, Row};
use rhai::{Array, Dynamic, Map};

/// Converts a decoded [`Row`] into a rhai object map.
pub(crate) fn row_to_map(row: &Row) -> Map {
    let mut map = Map::new();
    for (name, value) in row.fields() {
        map.insert(name.as_str().into(), field_to_dynamic(value));
    }
    map
}

/// Converts one [`FieldValue`] into a rhai [`Dynamic`].
fn field_to_dynamic(value: &FieldValue) -> Dynamic {
    match value {
        FieldValue::Null => Dynamic::UNIT,
        FieldValue::Bool(b) => (*b).into(),
        FieldValue::U32(v) => i64::from(*v).into(),
        FieldValue::U64(v) => match i64::try_from(*v) {
            Ok(i) => i.into(),
            Err(_) => Dynamic::from_blob(v.to_be_bytes().to_vec()),
        },
        FieldValue::Hash(h) => Dynamic::from_blob(h.to_vec()),
        FieldValue::Bytes(b) => Dynamic::from_blob(b.clone()),
        FieldValue::Enum(s) => s.clone().into(),
        FieldValue::Str(s) => s.clone().into(),
        FieldValue::List(items) => {
            let array: Array = items.iter().map(field_to_dynamic).collect();
            array.into()
        }
    }
}

/// Lowercase-hex encodes bytes (no `0x` prefix), for rendering blobs at the
/// prompt.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble"));
        out.push(char::from_digit((byte & 0xf) as u32, 16).expect("nibble"));
    }
    out
}
