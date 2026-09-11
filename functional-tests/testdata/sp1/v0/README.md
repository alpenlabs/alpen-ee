# SP1 v0 guest ELFs

Prebuilt `guest-alpen-chunk` / `guest-alpen-acct` ELFs for spec version `v0`,
used as test data by the functional tests that cross a predicate rotation
under `EE_PROVER_BACKEND=sp1`.

Those tests need two prover programs resident at once: a `v0` program matching
the account predicate OL registers at genesis, and a `v1` program the predicate
rotates to. `run_tests.sh` builds the `v1` pair from this workspace's current
source on every run. The `v0` pair is committed here because a build only ever
produces one of the two.

## Why these are v0

A guest proves under exactly one spec version, and that version is baked into
the binary rather than read from zkVM input. That is what binds it to the
verifying key, so a prover cannot pick which rules its work is checked under.
One guest package per version, and the constant naming the version is the only
thing that separates the two builds:

```rust
const SPEC_VERSION: AlpenSpecId = AlpenSpecId::V0;
```

Current source ships `AlpenSpecId::V1`, which is what `run_tests.sh` builds.
These ELFs are the same source with that one constant set to `V0`, so the pair
differs from the `v1` pair only in the rules it replays blocks under. That
makes them a real rotation target: the verifying key changes with the version,
so moving between them is a predicate rotation rather than a redeploy.

See `provers/sp1/guest-alpen-chunk/src/main.rs` and
`provers/sp1/guest-alpen-acct/src/main.rs` for the constant, and
`crates/proof-impl/alpen-chunk/src/lib.rs` for why it is an out-of-band
argument instead of zkVM input.

## Provenance

Built from this workspace, not from a sister repo:

- source: this branch, with `SPEC_VERSION` set to `AlpenSpecId::V0` in both
  `provers/sp1/guest-alpen-acct` and `provers/sp1/guest-alpen-chunk`
- builder: `cargo build --release -p strata-sp1-guest-builder`, driven by
  `run_tests.sh`'s `build_sp1_guests`
- output: `provers/sp1/generated/`, copied here

```
8a1c7aa1b1a7d7cadad89cbb73a982e27f2eb639d77af4064735dec5e93b4fee  guest-alpen-acct.elf
3844311a54dcacd50690e6c008ddac273121a5ac97693305eca1008792cdbcfa  guest-alpen-chunk.elf
```

To rebuild them, flip the constant in both guest packages to `V0`, run the
build, copy `provers/sp1/generated/` back here, and restore the constant to
`V1`. The ELFs are not byte-reproducible across toolchain versions, so expect
the hashes above to move when the SP1 toolchain changes; what matters is that
the pair is built together and that its predicate differs from the `v1` pair's.

`alpen-acct.predicate` is the file `provers/sp1/build.rs` wrote next to the
ELFs. It is the acct guest's predicate, which OL registers as the account's
`update_vk`.
