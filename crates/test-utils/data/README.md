# Test data

Fixtures shared by tests and tools across the workspace. No crate owns this directory. Readers find files by path from their own `CARGO_MANIFEST_DIR`.

## `evm-ee/rsp_block_witness.json`

A single EVM block in RSP's `EthClientExecutorInput` shape, under the `witness` key. The `params` key holds the results the block should produce (block hashes, state root, deposits and withdrawal intents).

The block is block 5 of a dev chain. It has no transactions, and its header carries the V0 stamp. It was dumped from a dev node's witness RPC and has been converted to newer RSP formats since. It cannot be regenerated from this repository.

Read by:

- `alpen-evm-ee` unit tests (`src/execution.rs`, `src/types/tests.rs`)
- `alpen-proof-chunk` native execution test
