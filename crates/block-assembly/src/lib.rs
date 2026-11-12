//! Handles assembly of EE blocks.

mod block;
mod package;
mod payload;
mod program_registry;

pub use block::{build_next_exec_block, BlockAssemblyInputs, BlockAssemblyOutputs};
pub use program_registry::{ProgramRegistry, ProgramRegistryError};
