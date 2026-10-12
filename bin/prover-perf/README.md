# Prover performance

This binary measures the SP1 guests (`alpen-chunk` and `alpen-acct`) on a realistic workload and reports their cycle counts. It can also prove the guests and save the proofs.

Report formatting and PR posting come from [`zkaleido-perf-report`](https://github.com/alpenlabs/zkaleido). This crate supplies the guests and their inputs.

## What the guests run on

Both guests run on the checked-in `mixed` workload in `crates/test-utils/data/workloads/mixed/`. It is 100 V1 blocks, the default size of a batch and a chunk, with about 2,100 transactions: native and legacy transfers, ERC-20 transfers and approvals, AMM swaps, contract deploys, bridge-outs, Schnorr precompile calls, and deposits. `summary.json` next to it lists the exact counts. The data README explains how it is built.

- `alpen-chunk` proves the 100 blocks as one chunk. Its input is assembled the way the sequencer's chunk prover assembles it.
- `alpen-acct` proves one account update over that chunk. The update processes the chunk's deposits as inbox messages, verifies the saved Groth16 chunk proof, and checks the batch's DA. The DA is the real batch state diff, posted in SPS-51 commit and reveal transactions inside a full-size synthetic L1 block.

## Generating reports

```shell
just prover-eval
```

This builds the guests with `params/dev.json` baked in and executes both. The account guest reads the chunk proof from `proofs/`.

## Posting reports to a PR

A run only prints the report unless you pass a GitHub flag such as `--github-token`. Then it also posts the report as a comment on the PR, and later runs update that comment. The report shows the change in cycles and gas since the last merged PR's report. In GitHub Actions, the PR number, repository and commit come from the run's environment, so CI only passes the token.

## The saved chunk proof

The account guest verifies chunk proofs with the chunk guest's program ID, which the build bakes into its ELF. A saved proof only verifies if its program ID is the one baked in. Most changes to the execution code change the chunk ELF, and ELFs built on different machines differ too. So prover-perf builds the account guest with `SP1_ALPEN_CHUNK_PROOF_PATH` pointing at the saved proof, and the guest then accepts that proof's program ID. Only the baked ID differs, so the cycle count is the same as for the real guest. `just prover-eval` and CI both set it. Never set it for guests that will run on a chain. Release builds (`docker-build`) refuse it.

The proof has to be regenerated when it no longer proves the workload's chunk: when the workload is regenerated, or when a change alters what the chunk guest commits to. prover-perf checks this before executing the account guest, and fails with a message saying to regenerate.

## Generating proofs

```shell
just prover-proof
```

This proves `alpen-chunk` and saves the proof as `proofs/alpen-chunk_SP1_<circuit version>.proof`. Pass `just prover-proof alpen-chunk,alpen-acct` to prove the account update as well. The programs always run chunk first, and each proof is saved before the next program starts, so the account proof is always built on the fresh chunk proof.

A Groth16 proof of a 100-block chunk is too heavy for a laptop. Use the SP1 network prover:

```shell
SP1_PROVER=network NETWORK_PRIVATE_KEY=<your-key> SP1_PROOF_STRATEGY=reserved just prover-proof
```

- `SP1_PROVER`: set to `network` to prove on the SP1 network instead of locally.
- `NETWORK_PRIVATE_KEY`: the key that authenticates with, and pays, the SP1 network.
- `SP1_PROOF_STRATEGY`: how the network fulfils the request, such as `auction` (the default), `hosted` or `reserved`.

Commit the new proof.

## Regenerating the workload

```shell
just prover-workload
```

The generator is deterministic, so a run without changes reproduces the same bytes. After regenerating, regenerate the chunk proof too.

## Profiling

```shell
ZKVM_PROFILING_DUMP=1 just prover-eval
```

This writes a `<program>_<program id>.trace_profile` file per guest. View one with [Samply](https://github.com/mstange/samply):

```bash
samply load <FILENAME>.trace_profile
```

Install Samply with `cargo install --locked samply` if you do not have it.

Profiling is only available for SP1.
