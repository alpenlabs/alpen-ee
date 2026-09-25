//! Conversion between the console core's [`Record`]/[`FieldValue`] and rhai's
//! dynamic values.
//!
//! A record crosses as `{ key, value }` — the same two halves the core names,
//! so what a predicate inspects, what a scan prints, and what the Rust types
//! hold are one shape. A value's own field called `key` sits inside `value` and
//! cannot be confused with the record's key.
//!
//! This is also the type-fidelity boundary: rhai has a single integer type, so
//! anything beyond its signed range rides across as a blob rather than being
//! silently truncated.
//!
//! The conversion is deliberately one-way. A write is expressed as a typed edit
//! applied to the value the console decoded, never as a whole value rebuilt from
//! rhai — a value that had made the round trip through a dynamically typed
//! script could not be trusted to be byte-identical in the fields nobody meant
//! to touch.

pub(crate) use alpen_ee_database::console::hex;
use alpen_ee_database::console::{FieldValue, Record, KEY_FIELD};
use rhai::{Array, Blob, Dynamic, Map};

use super::handle::ValueHandle;

/// The name a record's decoded value is presented under.
pub(crate) const VALUE_FIELD: &str = "value";

/// Converts a [`Record`] into the rhai object map scripts see.
pub(crate) fn record_to_map(record: &Record) -> Map {
    let mut map = Map::new();
    map.insert(KEY_FIELD.into(), record.key.clone().into());
    map.insert(VALUE_FIELD.into(), field_to_dynamic(&record.value));
    map
}

/// Converts one [`FieldValue`] into a rhai [`Dynamic`].
pub(crate) fn field_to_dynamic(value: &FieldValue) -> Dynamic {
    match value {
        FieldValue::Null | FieldValue::Unit => Dynamic::UNIT,
        FieldValue::Bool(b) => (*b).into(),
        // rhai integers are signed 64-bit, so anything that does not fit rides
        // as its big-endian bytes rather than wrapping into a negative number.
        FieldValue::U64(v) => match i64::try_from(*v) {
            Ok(i) => i.into(),
            Err(_) => Dynamic::from_blob(v.to_be_bytes().to_vec()),
        },
        FieldValue::I64(v) => (*v).into(),
        FieldValue::U128(v) => match i64::try_from(*v) {
            Ok(i) => i.into(),
            Err(_) => Dynamic::from_blob(v.to_be_bytes().to_vec()),
        },
        FieldValue::I128(v) => match i64::try_from(*v) {
            Ok(i) => i.into(),
            Err(_) => Dynamic::from_blob(v.to_be_bytes().to_vec()),
        },
        FieldValue::F64(v) => (*v).into(),
        FieldValue::Str(s) => s.clone().into(),
        FieldValue::Bytes(b) => Dynamic::from_blob(b.clone()),
        FieldValue::Enum(s) => s.clone().into(),
        // A variant keeps its name as the only key, so a predicate tests it as
        // `r.value.status.Blocked != ()`.
        FieldValue::Variant { name, value } => {
            let mut map = Map::new();
            map.insert(name.as_str().into(), field_to_dynamic(value));
            map.into()
        }
        FieldValue::List(items) => {
            let array: Array = items.iter().map(field_to_dynamic).collect();
            array.into()
        }
        FieldValue::Map(entries) => {
            let mut map = Map::new();
            for (key, value) in entries {
                // A structured map key has no rhai counterpart, so it crosses
                // under its rendered form; only lookup convenience is lost, and
                // the stored value is never rebuilt from this side.
                let key = match key {
                    FieldValue::Str(name) | FieldValue::Enum(name) => name.clone(),
                    other => other.to_string(),
                };
                map.insert(key.as_str().into(), field_to_dynamic(value));
            }
            map.into()
        }
    }
}

/// Converts a rhai value into the [`FieldValue`] an edit will write.
///
/// The result is deliberately *loose*: a script cannot say whether a string is
/// a field or an enum variant's name, whether a one-entry map is a map or a
/// variant with a payload, or whether `()` is `None` or unit. Nothing here
/// guesses. The value is resolved against the field's real Rust type when the
/// edit is converted back — serde asks for what the type actually is — and the
/// canonical form that comes out is what gets stored. An input the type cannot
/// account for is refused outright.
///
/// Integers arrive as rhai's signed 64-bit type; anything wider is written as a
/// decimal string, which the integer deserializers parse.
pub(crate) fn dynamic_to_field(value: &Dynamic) -> Result<FieldValue, String> {
    if value.is_unit() {
        return Ok(FieldValue::Null);
    }
    if let Ok(v) = value.as_bool() {
        return Ok(FieldValue::Bool(v));
    }
    if let Ok(v) = value.as_int() {
        return Ok(if v >= 0 {
            FieldValue::U64(v as u64)
        } else {
            FieldValue::I64(v)
        });
    }
    if let Ok(v) = value.as_float() {
        return Ok(FieldValue::F64(v));
    }
    if value.is_string() {
        return Ok(FieldValue::Str(value.clone().into_string()?));
    }
    if value.is_blob() {
        return Ok(FieldValue::Bytes(value.clone().cast::<Blob>()));
    }
    if value.is_array() {
        let items = value
            .clone()
            .cast::<Array>()
            .iter()
            .map(dynamic_to_field)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(FieldValue::List(items));
    }
    if value.is_map() {
        let entries = value
            .clone()
            .cast::<Map>()
            .iter()
            .map(|(name, value)| {
                dynamic_to_field(value).map(|v| (FieldValue::Str(name.to_string()), v))
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(FieldValue::Map(entries));
    }
    // A held value is already decoded, so it nests straight into another one.
    if let Some(handle) = value.read_lock::<ValueHandle>() {
        return Ok(handle.value.clone());
    }
    Err(format!(
        "cannot write a {} from the prompt",
        value.type_name()
    ))
}
