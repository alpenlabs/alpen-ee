# Releasing

A release is a `v*` tag. Pushing one runs `.github/workflows/release.yml`. It
builds the SP1 guests for every network in `params/` except `dev`, publishes
them in a GitHub Release and copies them to S3.
[What a release publishes](#what-a-release-publishes) lists the files.

## Branches and tags

Development happens on `main`. Each release line gets its own branch, named
`releases/vX.Y.x`, for example `releases/v0.4.x`.

A line's first rcs are tagged on `main`. The branch is cut later, at an rc's
commit. From then on, every tag of the line comes from the branch: later rcs,
`vX.Y.0` and the patch releases `vX.Y.1` and up. `vX.Y.0` and the patch
releases always need the branch, because the line is maintained there.

Once the branch exists, changes reach it only by cherry-picking them from
`main` through a PR.

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

1. Tag `vX.Y.0-rc.1` on `main` and push the tag. Fixes land on `main` and go
   out in the next rc.
2. Cut `releases/vX.Y.x` at the commit of the rc you want to ship. Cut it
   earlier if `main` starts taking work for the next line while the line
   still needs rcs. After that, land fixes on `main` first, cherry-pick them
   onto the branch through a PR, and tag the next rc from the branch.
3. Once testnet or mainnet runs an rc, tag `vX.Y.0` at that same commit, so
   the final release names the build that network runs.
4. Tag patch releases from the branch. They must not change full node
   behavior.

Each rc tagged on `main` takes everything merged since the last one. So while
a line takes rcs from `main`, keep work for the next line out of `main`, or
cut the branch.

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
   `-suffix`, and that it points at a commit on `releases/vMAJOR.MINOR.x`.
   Until that branch exists, a `vMAJOR.MINOR.0` tag with a `-suffix` must
   point at a commit on `main` instead. If either check fails, nothing is
   published.
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

The workflow that runs for a tag is the copy at the tagged commit. So the
"Check tag's branch" step only runs for a tag on a commit that has it.
`releases/v0.4.x` doesn't have it yet. It sits at `v0.4.0-rc.3`, and `v0.4.0`
goes on that same commit (step 3 of "Cutting a release"). So no v0.4 tag is
checked until the step is cherry-picked onto the branch, and neither is a tag
on any older commit.

## Building guests outside a release

`publish-guests.yml` can also run by hand. Every run uploads its files as a
workflow artifact. It also puts the account program ID in the run summary and
in the `alpen-acct-program-id-<network>` artifact.

- To build `dev`, which releases skip, run it with `network` set to `dev`.
- To build with params that are not in the repo yet, pass `params_url`.
- To attach the files to a release, pass `release_tag`, and set `ref` to that
  tag. The release must still be a draft. The run fails before it builds if
  the checked-out commit is not the tag's commit, or if `params_url` is set,
  because a release's guests are built with the params in the repo.

To copy an older release to S3, run `publish-guests-s3.yml` by hand with a
`v*` tag selected as the ref and that release's tag as `tag`.
