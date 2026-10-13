// TODO(STR-3685): should some of these types be moved to a more generic "exec env types"
// crate?  so that chunk-runtime doesn't have to depend on this crate?  maybe
// do this after repo restructure

use std::error::Error;

use alpen_chain_types::{ExecHeaderSummary, ExecInputs};
use strata_codec::Codec;
use strata_identifiers::Hash;

use crate::outputs::ExecBlockOutput;

/// Represents a partially-loaded state, including any information we would need
/// to manipulate it and compute state roots.
pub trait ExecPartialState: Codec + Clone + Sized {
    /// Computes the state root of a partial state.
    fn compute_state_root(&self) -> Hash;
}

/// Represents an execution block header.
pub trait ExecHeader: Codec + Clone + Sized {
    /// Gets the parent exec blkid field.
    fn get_parent_id(&self) -> Hash;

    /// Gets the state root field.
    fn get_state_root(&self) -> Hash;

    /// Gets the execution-environment-specific summary committed for this header.
    fn get_exec_header_summary(&self) -> ExecHeaderSummary;

    /// Computes the exec block ID.
    fn compute_block_id(&self) -> Hash;
}

/// Represents the body of an execution block, without the header.
///
/// This is the executable content of a block (transactions, operations, etc.)
/// that can be processed to produce state changes.
pub trait ExecBlockBody: Codec + Sized {}

/// Represents a full execution block, with whatever information needed to
/// execute it on top of a pre-state.
pub trait ExecBlock: Codec + Sized {
    /// The block's header type.
    type Header: ExecHeader;

    /// The block's body type.
    type Body: ExecBlockBody;

    /// Constructs a block from a header and body.
    fn from_parts(header: Self::Header, body: Self::Body) -> Self;

    /// Gets a reference to the block's header.
    fn get_header(&self) -> &Self::Header;

    /// Gets a reference to the block's body.
    fn get_body(&self) -> &Self::Body;
}

/// Execution environment.
pub trait ExecutionEnvironment: Sized + 'static {
    /// Partial execution chain state.
    type PartialState: ExecPartialState;

    /// Execution block.
    type Block: ExecBlock;

    /// Write batch that can be applied to the partial state.
    ///
    /// This is NOT any kind of writes we check for in DA.
    type WriteBatch: Sized;

    /// Execution-derived block output that is neither a state write nor OL-facing output.
    ///
    /// This carries execution-environment-specific commitments needed to verify
    /// a block header.
    type BlockOutput: Sized;

    /// Why one of this environment's checks or its execution failed.
    ///
    /// Each environment names its own failures, so a rejected block says
    /// which rule it broke.
    type Error: Error + Send + Sync + 'static;

    /// Checks a block header on its own and against its parent's header.
    ///
    /// This must check that `header` builds on `parent`, meaning its parent
    /// ID is `parent`'s block ID. Callers rely on that to treat a run of
    /// blocks as one chain. It also checks every other rule the environment
    /// puts on a header, such as its number, timestamp and fee fields.
    fn verify_header(
        &self,
        parent: &<Self::Block as ExecBlock>::Header,
        header: &<Self::Block as ExecBlock>::Header,
    ) -> Result<(), Self::Error>;

    /// Checks a block's body against its header, before executing it.
    ///
    /// This must check that the header commits to the body. It also checks
    /// every other rule the environment puts on a block that needs the body
    /// but not the execution result, such as a limit on the block's size.
    fn verify_block_pre_execution(&self, block: &Self::Block) -> Result<(), Self::Error>;

    /// Executes a block on top of a pre-state, returning the execution output.
    ///
    /// Execution alone does not verify the block: the caller must also check
    /// the header with [`Self::verify_header`], the body with
    /// [`Self::verify_block_pre_execution`], the inputs with
    /// [`Self::verify_inputs_against_block`] and the output with
    /// [`Self::verify_block_post_execution`].
    fn execute_block(
        &self,
        pre_state: &Self::PartialState,
        block: &Self::Block,
    ) -> Result<ExecBlockOutput<Self>, Self::Error>;

    /// Checks that the inputs match what the block applies.
    ///
    /// Execution reads the block, not the inputs, so the inputs are a claim
    /// about the block. Each environment checks that claim against the block
    /// itself, for example against a deposit list in the body.
    fn verify_inputs_against_block(
        &self,
        block: &Self::Block,
        inputs: &ExecInputs,
    ) -> Result<(), Self::Error>;

    /// Checks a block's execution output against its header.
    ///
    /// This must check every commitment the header makes to execution, such
    /// as the receipts and the gas used. The state root is left to the caller,
    /// since it only exists once the writes are merged.
    fn verify_block_post_execution(
        &self,
        header: &<Self::Block as ExecBlock>::Header,
        outputs: &ExecBlockOutput<Self>,
    ) -> Result<(), Self::Error>;

    /// Applies a block's write batch to the partial state.
    ///
    /// Besides the state changes, this must record whatever later blocks in
    /// the chunk need from this one, such as its hash.
    fn merge_write_into_state(
        &self,
        state: &mut Self::PartialState,
        wb: &Self::WriteBatch,
    ) -> Result<(), Self::Error>;
}
