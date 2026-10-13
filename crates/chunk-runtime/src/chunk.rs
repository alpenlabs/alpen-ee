//! Chunk data structures.

use alpen_acct_types::{ExecBlock, ExecutionEnvironment};
use alpen_chain_types::{ExecInputs, ExecOutputs};

/// Chunk of decoded exec env blocks and the header they extend from.
#[expect(missing_debug_implementations, reason = "impossible")]
pub struct Chunk<E: ExecutionEnvironment> {
    prev_header: <E::Block as ExecBlock>::Header,
    blocks: Vec<ChunkBlock<E>>,
}

impl<E: ExecutionEnvironment> Chunk<E> {
    pub fn new(prev_header: <E::Block as ExecBlock>::Header, blocks: Vec<ChunkBlock<E>>) -> Self {
        Self {
            prev_header,
            blocks,
        }
    }

    /// Header of the block the chunk's first block builds on.
    pub fn prev_header(&self) -> &<E::Block as ExecBlock>::Header {
        &self.prev_header
    }

    /// Returns whether the chunk has no blocks.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Consumes the chunk and returns its prev header and blocks.
    pub fn into_parts(self) -> (<E::Block as ExecBlock>::Header, Vec<ChunkBlock<E>>) {
        (self.prev_header, self.blocks)
    }
}

/// Decoded execution env block within a chunk.
#[expect(missing_debug_implementations, reason = "impossible")]
pub struct ChunkBlock<E: ExecutionEnvironment> {
    inputs: ExecInputs,
    outputs: ExecOutputs,
    exec_block: E::Block,
}

impl<E: ExecutionEnvironment> ChunkBlock<E> {
    pub fn new(inputs: ExecInputs, outputs: ExecOutputs, exec_block: E::Block) -> Self {
        Self {
            inputs,
            outputs,
            exec_block,
        }
    }

    /// Consumes the chunk block and returns its inputs, outputs and block.
    pub fn into_parts(self) -> (ExecInputs, ExecOutputs, E::Block) {
        (self.inputs, self.outputs, self.exec_block)
    }
}
