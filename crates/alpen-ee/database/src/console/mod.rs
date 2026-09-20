//! Operator console core: the trusted native layer over the EE MDBX store.
//!
//! This module is the codec-owning half of the `dbconsole` binary.
//! It exposes the store as decoded [`Record`]s through the same production
//! codecs the node uses, so the scripting shell in `bin/dbconsole` can
//! orchestrate reads and guarded writes without ever touching raw bytes.
//!
//! The split is deliberate: the engine-neutral core lives here, next to the
//! schema and codecs; the scripting engine (rhai) and the REPL live in the
//! binary. See the `ee-db-console` design doc.

mod key;
mod mirrors;
mod reflect;
mod value;

pub use key::ConsoleKey;
pub use mirrors::ProofReceiptMirror;
pub use reflect::{
    BytesReflector, Mirror, MirrorReflector, ReflectError, SerdeReflector, Unreflectable,
    ValueReflector,
};
pub use value::{hex, parse_hex, FieldValue, Record};
