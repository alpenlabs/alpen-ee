# SP1 v1 guest ELFs

Prebuilt `guest-alpen-chunk` / `guest-alpen-acct` ELFs for spec version `v1`,
used as test data by the functional tests that cross a predicate rotation
under `EE_PROVER_BACKEND=sp1`.

Those tests need two prover programs resident at once. Most of them launch on
`v1` and rotate to `v2`, which `run_tests.sh` builds from this workspace's
current source on every run. The Osaka fork test launches on `v0` and rotates
to `v1`, so it uses the committed `v0` pair and this one. A build only ever
produces one version, so the older pairs are committed.

## Why these are v1

A guest proves under exactly one spec version, and that version is baked into
the binary rather than read from zkVM input. That is what binds it to the
verifying key, so a prover cannot pick which rules its work is checked under.
The constant naming the version is the only thing that separates the builds:

```rust
const SPEC_VERSION: AlpenSpecId = AlpenSpecId::V1;
```

Current source ships `AlpenSpecId::V2`. These ELFs are the same source with
that one constant set to `V1`.

## Provenance

Built from this workspace:

- source: this branch, with `SPEC_VERSION` set to `AlpenSpecId::V1` in both
  `provers/sp1/guest-alpen-acct` and `provers/sp1/guest-alpen-chunk`
- params: `scripts/gen_sp1_guest_params.py`, as `run_tests.sh`'s
  `build_sp1_guests` runs it
- builder: `cargo build --release -p alpen-sp1-guest-builder`
- output: `provers/sp1/generated/`, copied here

```
2f5d182c1359d8e4623a5e5d87d06bc6598a88d7c67a2e7af0612225a3f8d355  guest-alpen-acct.elf
d12e14555353a25fc6a2056b8f32a9e4183a7fc6ca15db76f731cec66986471d  guest-alpen-chunk.elf
```

To rebuild them, flip the constant in both guest packages to `V1`, run the
build, copy `provers/sp1/generated/` back here, and restore the constant to
`V2`. The ELFs are not byte-reproducible across toolchain versions, so expect
the hashes above to move when the SP1 toolchain changes. What matters is that
the pair is built together and that its predicate differs from the other
pairs'.

`alpen-acct.predicate` is the file `provers/sp1/build.rs` wrote next to the
ELFs. It is the acct guest's predicate, which OL registers as the account's
`update_vk`.
