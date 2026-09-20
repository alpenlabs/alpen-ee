//! The decoded value a script holds between reading it and writing it back.
//!
//! A [`ValueHandle`] is a decoded value plus the table it came from. Carrying
//! the table is what lets an edit be checked as it is made: the value alone
//! knows its shape — which fields exist — but only the table knows their types,
//! so without it a wrong value would sit unnoticed until the write was staged
//! and be reported against the write rather than the edit that caused it.
//!
//! Each accepted edit is stored in the canonical form the table's decoder
//! produces, so the handle always holds exactly what a read would return.

use std::rc::Rc;

use alpen_ee_database::console::{ConsoleDb, FieldValue};
use rhai::{Dynamic, EvalAltResult};

use super::value::dynamic_to_field;

/// A decoded value, and the table whose type governs it.
#[derive(Clone)]
pub(crate) struct ValueHandle {
    /// The table this value was read from, in its `env/Table` form.
    pub(crate) table: String,
    /// The value, always in canonical form.
    pub(crate) value: FieldValue,
    /// The attach the value came from, for checking edits against its type.
    db: Rc<ConsoleDb>,
}

impl ValueHandle {
    /// Builds a handle over a value just read from `table`.
    pub(crate) fn new(table: String, value: FieldValue, db: Rc<ConsoleDb>) -> Self {
        Self { table, value, db }
    }

    /// Replaces one field, checking the result against the table's real type.
    ///
    /// The check runs on the whole value rather than the field alone, because a
    /// field's type is only knowable in the context of the type that holds it.
    /// The canonical form is kept, so a variant named by a bare string becomes
    /// the variant, and reading the field back shows what would be stored.
    pub(crate) fn set_field(
        &mut self,
        field: &str,
        new: &Dynamic,
    ) -> Result<(), Box<EvalAltResult>> {
        let new = dynamic_to_field(new).map_err(|e| -> Box<EvalAltResult> { e.into() })?;

        let mut candidate = self.value.clone();
        if !candidate.replace_field(field, new) {
            return Err(format!("this value has no field `{field}`").into());
        }

        // Only commit the edit to the handle once the table's type has accepted
        // it, so a rejected edit leaves the value exactly as it was.
        self.value = self
            .db
            .canonicalize(&self.table, &candidate)
            .map_err(|e| -> Box<EvalAltResult> { e.to_string().into() })?;
        Ok(())
    }
}
