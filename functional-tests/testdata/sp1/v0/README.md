# SP1 v0 guest ELFs

Prebuilt `guest-alpen-chunk` / `guest-alpen-acct` ELFs for spec version `v0`,
used as test data by the EE live fork upgrade functional test under
`EE_PROVER_BACKEND=sp1`.

The test needs two prover programs resident at once: a `v0` program matching
the account predicate OL registers at genesis, and a `v1` program that the
predicate rotates to. `run_tests.sh` builds the `v1` pair from this
workspace's current source. The `v0` pair cannot be built from this
workspace at all -- it predates the Osaka work -- so it is committed here
instead.

## Provenance

Built by CI in the sister `alpen` repo:

- repo: `alpenlabs/alpen`
- commit: `42d45ee7220ab2b06ea6737a35ebbbdb7ca52e77` (branch `main`)
- workflow: `.github/workflows/ci-build.yml`
- run: https://github.com/alpenlabs/alpen/actions/runs/31703405261 (2026-08-13)

```
d3eeec286e4d019faf57833614f42be62a6be636cb84635f228393264afbaca1  guest-alpen-acct.elf
3ed4b9356cbf74ff5cdcec03cd8ac4dd93b15c302de21dbf230ce0c53877bdfd  guest-alpen-chunk.elf
```

`alpen-acct.predicate` is the file that build's `provers/sp1/build.rs` wrote
next to the ELFs. It is the acct guest's predicate, which OL registers as the
account's `update_vk`.
