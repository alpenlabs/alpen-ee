//! The witness environment's MDBX tables and their stores: per-block state
//! diffs and the DA-published filter. The traits they implement live in
//! `alpen-common` beside the other storage traits.

pub mod mdbx;

pub use strata_db_types::{errors, DbResult};
