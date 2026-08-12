//! Operator console core: the trusted native layer over the EE MDBX store.
//!
//! This module is the codec-owning half of the `alpen-ee dbconsole` command.
//! It exposes the store as decoded [`Row`]s through the same production codecs
//! the node uses, plus the restoration-[`TableClass`] each table carries, so
//! the scripting shell in `bin/alpen-ee` can orchestrate reads (and, later,
//! guarded writes) without ever touching raw bytes.
//!
//! The split is deliberate: the engine-neutral core lives here, next to the
//! schema and codecs; the scripting engine (rhai) and the REPL live in the
//! binary. See the `ee-db-console` design doc.

mod class;
mod db;
mod registry;
mod row;

pub use class::TableClass;
pub use db::ConsoleDb;
pub use registry::{TableInfo, TableReflect};
pub use row::{FieldValue, Row};
