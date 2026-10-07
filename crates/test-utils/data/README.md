# Test data

Fixtures shared by tests and tools across the workspace. No crate owns this directory. Readers find files by path from their own `CARGO_MANIFEST_DIR`.

## `evm-ee/rsp_block_witness.json`

A single EVM block in RSP's `EthClientExecutorInput` shape, under the `witness` key. The `params` key holds the results the block should produce (block hashes, state root, deposits and withdrawal intents).

The block is block 5 of a dev chain. It has no transactions, and its header carries the V0 stamp. It was dumped from a dev node's witness RPC and has been converted to newer RSP formats since. It cannot be regenerated from this repository.

Read by:

- `alpen-evm-ee` unit tests (`src/execution.rs`, `src/types/tests.rs`)
- `alpen-proof-chunk` native execution test

## `workloads/<name>/`

EVM workloads made by `alpen-test-utils-evm-workload`: a run of consecutive V1 blocks with real signed transactions, built on the `params/dev.json` chain. Each directory holds:

- `workload.bin`: the blocks and their deposits, plus the host-side data the provers build their inputs from. That is the chunk prover's pre-state, the account prover's range witness, and the batch state diff. The crate's `Workload` type defines the encoding.
- `summary.json`: the generator settings, the block range, and how many transactions of each kind the range holds. Nothing reads it; it is there for reviewers.

The generator builds the blocks on a throwaway reth database with the sequencer's EVM config. The witness, state changes and range witness come from the same functions the node uses. It is deterministic, so regenerating without changes reproduces the same bytes.

### `workloads/mixed/`

The workload the prover-perf guests run on. Setup blocks first fund 10,000 EOAs, deploy two ERC-20 tokens and a constant-product pool, and seed 5,000 token holders and 300 pool traders. The stored range is the next 100 blocks, one default batch, with 10 to 30 random transactions per block:

| Kind | Share |
|---|---|
| Native transfer between funded EOAs | 35% |
| Native transfer to a fresh address | 10% |
| Legacy (EIP-155) transfer | 5% |
| ERC-20 transfer between holders | 25% |
| ERC-20 transfer to a new holder | 10% |
| ERC-20 approve, then `transferFrom` of the whole allowance in the next block | 5% |
| Pool swap | 10% |

On top of those, on a fixed schedule: a bridge-out every 10 blocks, a deposit every 10 blocks, a Schnorr precompile call every 5 blocks, and two token deploys. One deploy has new code, which the DA blob publishes. The other deploys code an earlier batch already published, so the blob leaves it out and the DA witness supplies it through dedup.

Regenerate it with:

```shell
just prover-workload
```

Then regenerate the chunk proof prover-perf reads (see `bin/prover-perf/README.md`).

Read by:

- `alpen-prover-perf`, for both guests' inputs and native tests
