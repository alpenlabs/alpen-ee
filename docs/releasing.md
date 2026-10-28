# Releasing

A release is a `v*` tag. Pushing one runs `.github/workflows/release.yml`. It
builds the SP1 guests for every network in `params/` except `dev`, publishes
them in a GitHub Release and copies them to S3.
[What a release publishes](#what-a-release-publishes) lists the files.

## Branches and tags

Development happens on `main`. Each release line gets its own branch, named
`releases/vX.Y.x`, for example `releases/v0.4.x`. Every tag of the line comes
from that branch: its rcs, `vX.Y.0` and the patch releases `vX.Y.1` and up.
`main` is never tagged. To use code from `main` in another repo, pin a commit.

Changes reach a release branch only by cherry-picking them from `main` through
a PR.

## What a line keeps fixed

Once a line reaches `vX.Y.0`, its full node behavior must not change. Every
later release of the line must accept the same blocks and reach the same state
from them. A change to that goes in the next line. The rcs before `vX.Y.0` can
still change it. Review enforces this. CI does not check it.

The guests' verifying keys can change within a line. A refactor, a moved
line, a dependency bump in a guest's lockfile or an SP1 bump is enough to
change them, and that is fine. The guests prove the full node's rules, so
while the rules stay the same, the guests a network was rolled out with still
prove them. So a network keeps those guests when it updates its node within
the line. It only needs new guests for a security fix in the guests. Moving to
a new key takes a rotation to a new spec version.

The guests bake in each network's params, so each network has its own keys.
To see whether a release changed a network's key, compare that network's
program ID in the two releases' notes.

## Cutting a release

1. Cut `releases/vX.Y.x` from `main` when the line is ready for its first rc.
2. Tag `vX.Y.0-rc.1` on the branch and push the tag.
3. Land fixes on `main` first. Then cherry-pick them onto the branch through
   a PR, and tag the next rc from the branch.
4. Once testnet or mainnet runs an rc, tag `vX.Y.0` at that same commit, so
   the final release names the build that network runs.
5. Tag patch releases from the branch. They must not change full node
   behavior.

Staging takes the rcs first. If a later rc only changes the guests' keys,
staging keeps proving with the guests it has. If it changes full node
behavior, staging needs a chain reset to take it, because its guests no longer
prove the new rules and a new key always means a new spec version.

The tag does not build the `alpen-client` image. To build it, run the
"CI — Build and Push Images" workflow by hand with the tag as `ref`.

## What a release publishes

Each network's guests are built with its `params/<network>.json`. For each
network, the GitHub Release holds these files:

- `<network>-guest-alpen-chunk.elf` and `<network>-guest-alpen-acct.elf`
- `<network>-alpen-acct.predicate`, the account guest's `Sp1Groth16` predicate
  that the OL uses to check account proofs
- `<network>-alpen-params.json`, the params the guests were built with

The account program ID is not a release file. The release notes give it next
to each network's predicate, along with the command that rebuilds the files.
[`provers/sp1/README.md`](../provers/sp1/README.md#outputs) explains what the
program ID and the predicate are, and
[Checking a release](../provers/sp1/README.md#checking-a-release) shows how to
rebuild a release and compare.

After the release is published, the files are copied to
`s3://$ALPEN_GUESTS_S3_BUCKET/elfs/alpen-ee/<version>/`, with a `.sha256` file
next to each. `<version>` is `<commit sha8>-<params sha8>`, so networks with
the same params share a folder. The files there drop their network prefix,
and `manifest.json` lists the networks.

## What `release.yml` does

1. It checks that the tag is `vMAJOR.MINOR.PATCH`, optionally with a
   `-suffix`, and that it points at a commit on `releases/vMAJOR.MINOR.x`. If
   either check fails, nothing is published.
2. It creates a draft GitHub Release. A tag with a `-suffix` makes a
   prerelease.
3. It runs `.github/workflows/publish-guests.yml` once per network. Each run
   builds the guests with `docker-build` and attaches its files to the draft.
4. It puts each network's account program ID and predicate in the release
   notes.
5. It publishes the release once the draft holds exactly the files that were
   built, and the tag still points at the commit they were built from. A
   published release is immutable.
6. It runs `.github/workflows/publish-guests-s3.yml`. That checks each file
   against the release attestation and copies it to S3. The copy waits for a
   reviewer to approve it in the `sp1-artifacts` environment. It fails if the
   `ALPEN_GUESTS_S3_BUCKET` or `ALPEN_GUESTS_S3_ROLE_ARN` repo variable is not
   set.

The workflow that runs for a tag is the copy at the tagged commit. So a
release branch only gets the branch check once it holds this version of
`release.yml`.

## Building guests outside a release

`publish-guests.yml` can also run by hand. Every run uploads its files as a
workflow artifact. It also puts the account program ID in the run summary and
in the `alpen-acct-program-id-<network>` artifact.

- To build `dev`, which releases skip, run it with `network` set to `dev`.
- To build with params that are not in the repo yet, pass `params_url`.
- To attach the files to a release, pass `release_tag`. This only works while
  that release is still a draft.

To copy an older release to S3, run `publish-guests-s3.yml` by hand with a
`v*` tag selected as the ref and that release's tag as `tag`.
