# SP1 v1 guest ELFs

Built from: tag `v0.4.0`, commit `4ef60bdc88714723147122db83d603442f975659`,
unchanged.

Prebuilt `guest-alpen-chunk` / `guest-alpen-acct` ELFs for spec version `v1`,
used as test data by the functional tests that cross a predicate rotation
under `EE_PROVER_BACKEND=sp1`.

Those tests need two prover programs resident at once. They launch on `v1` and
rotate to `v2`, which `run_tests.sh` builds from this workspace's current
source on every run. Proving only has to cross from the deployed version to
the next one, so this is the only committed pair. The Osaka fork test crosses
from `v0` to `v1`, so it always runs on the native backend.

## Why these come from v0.4.0

The `v1` program on live networks is the one release `v0.4.0` shipped, and it
never changes. The current binary has to keep proving `v1` batches with it
until a rotation activates `v2`. So the tests run the guest code of that
release, not current source with the version constant set back. The current
host builds `v1` inputs in that release's layout, so they reach the guest that
will really prove them in the form it reads.

Only the baked-in params differ from the published ELFs. Those bake each
network's params, while these bake the functional-test params, so the tests'
dev chain can run them.

## Provenance

- source: tag `v0.4.0` (`4ef60bdc88714723147122db83d603442f975659`), unchanged
- params: that tag's `functional-tests/scripts/gen_sp1_guest_params.py`, with
  `strata-datatool` built by that tag's `functional-tests/build_strata_bins.sh`
- builder: `cargo build --release -p alpen-sp1-guest-builder` at that tag
- output: `provers/sp1/generated/`, copied here

```
2f2c30b776e2b4e1e9f0baa8fa0186a5894e13be81fa9d259a0fd1d036df67ea  guest-alpen-acct.elf
d8221638255c1e78661b35d2fe1c8ccb683ee0bd188ef4cae8d1d0024f183a23  guest-alpen-chunk.elf
```

To rebuild them, check out `v0.4.0` in a separate worktree and run the steps
of its `run_tests.sh` `build_sp1_guests` there. The ELFs are not
byte-reproducible across toolchain versions, so expect the hashes above to
move when the SP1 toolchain changes.

`alpen-acct.predicate` is the file `provers/sp1/build.rs` wrote next to the
ELFs. It is the acct guest's predicate, which OL registers as the account's
`update_vk`.
