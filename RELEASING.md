# Releasing Blue

Blue uses one semantic version for the CLI, service image, Helm chart, OpenAPI
contract, deployment bundle, and versioned documentation. A bot decides that
version; nobody edits it by hand.

## The loop

1. Merges land on `main`. The **Release Please** workflow recomputes the next
   version from the conventional commits since the last release and keeps one
   `chore: release <version>` PR open on the
   `release-please--branches--main` branch, carrying the CHANGELOG entry.
2. The same run's `finalize` job writes that version into every version-bearing
   file (`scripts/set-version.sh`) and freezes the documentation snapshot
   (`apps/docs/<version>/`), as one extra commit on the branch.
3. Cut release candidates from the PR head as often as you like — see below.
4. Review and merge the release PR. Release Please creates the `v<version>` tag
   and a **draft** GitHub Release holding the changelog.
5. The tag fires the **Release** workflow, which builds every artifact, attests
   it, publishes the image and chart, and flips the draft live.

Nothing publishes on `main`. A candidate can carry every artifact a release
carries, but it is always a **prerelease**, which is what keeps it out of every
default resolution — see below.

## Repository setup

`RELEASE_PLEASE_TOKEN` — a **fine-grained PAT** scoped to this repository, with
**Contents: read/write**, **Pull requests: read/write**, and **Issues:
read/write** (the `autorelease: *` labels go through the Issues API). Nothing
else. Set the longest expiry you are willing to rotate and put the rotation date
in a calendar: expiry is silent, Release Please simply stops opening PRs.

`GITHUB_TOKEN` cannot do this job, and no repository setting changes that:

- `can_approve_pull_request_reviews` is `false` at the **organization** level,
  which gates PR *creation*, not just approval.
- Events authored by `GITHUB_TOKEN` start no workflow runs, so the release PR's
  head would carry none of the required checks and could never be merged.

Release Please's own README documents the PAT as the normal path for exactly
these reasons.

Two consequences of the `Protect main` ruleset worth knowing before the first
release:

- `require_last_push_approval: true` — every release PR's last push is the
  finalize commit, authored by the PAT owner, so **the PAT owner cannot be the
  approving reviewer.** Either keep a second maintainer in the loop or hold the
  PAT on a dedicated machine account.
- `dismiss_stale_reviews_on_push: true` — every rebuild and every finalize push
  dismisses approval. Approve last, merge promptly.

Still outstanding from the pre-public checklist: enable immutable GitHub
Releases — decide candidate retention *first*, because immutability forecloses
pruning them later — make `ghcr.io/blocksorg/blue`,
`ghcr.io/blocksorg/blue-rc` and the Blue OCI chart package publicly readable,
and configure organization-level artifact attestations. Creation of `v*` tags is **not** restricted by a ruleset.

## How the version gets written

Release Please computes the version and the CHANGELOG. It writes no other file,
because all three of its declarative mechanisms provably break here:

- `release-type: rust` throws — the root `Cargo.toml` is a virtual `[workspace]`
  manifest with no `[package]`, and the updater has no concept of
  `[workspace.package].version`.
- `extra-files` with `type: yaml` round-trips the 1600-line contract through
  js-yaml: it reflows the file, strips every comment, and emits `version: 0.2.0`
  unquoted, breaking all four readers that match on `version: "X.Y.Z"`.
- `type: generic` annotations have to live in the files forever, and every
  `sed`/JS parser here either captures the trailing comment or stops matching.
  It also cannot produce `apps/docs/<version>/`.

So `scripts/set-version.sh <version>` owns the write side, and
`scripts/check-release-version.sh v<version>` owns the read side. They are exact
mirrors — every file one writes is a file the other reads — and CI runs the
check on every PR, so a file that drifts out of the pair fails `packaging`. The
writer also updates the small allowlist of release-bearing examples in the live
`apps/docs/next` tree; other documentation versions, policy examples, and
dependency versions are deliberately outside its ownership.

`scripts/set-version.sh` needs `node` and `cargo` on PATH. Run it by hand if you
ever need to prepare a release without the bot:

```bash
scripts/set-version.sh 0.2.0
node apps/docs/scripts/release-version.mjs 0.2.0
scripts/check-release-version.sh v0.2.0
```

Two Release Please config keys look redundant and are not. `include-component-in-tag:
false` defaults to `true` and only happens to yield `v0.2.0` for a root package
— if that ever changes, the tag becomes `blue-v0.2.0`, misses `release.yml`'s
`v*.*.*` trigger, and nothing publishes, silently.
`pull-request-title-pattern` replaces the default `chore(main): release X`,
whose `main` scope is not in AGENTS.md's list and would land in `main`'s log via
squash-merge.

## The release-branch gate

`packaging` (a required check) runs one extra step on
`release-please--branches--main`: it reads the version from
`.release-please-manifest.json` — not `Cargo.toml`, which is still the previous
version and self-consistently wrong until finalize runs — then re-runs
`check-release-version.sh` and `check-release-snapshot.mjs`.

So an un-finalized release branch holds the merge button shut, and says why. If
finalize ever fails, use **Re-run failed jobs** on the Release Please run, or
dispatch the workflow again: finalize takes no inputs, discovers the PR and the
version off the branch, refreshes that current not-yet-released documentation
snapshot from the synchronized `next` tree, and commits only if the tree
changed. Replacement is automation-only and refuses to touch any historical
release that is not the first/default stable navigation entry.

## Release candidates

To ship unreleased code, cut a candidate from any ref on demand. Two entry
points, differing only in how much they build:

| | Command | Publishes | Cost |
|---|---|---|---|
| Image only | `gh workflow run dev-image.yml --ref <ref>` | the multi-arch deployment image | ~10 min |
| Full candidate | `gh workflow run release.yml --ref <ref> -f candidate=true` | everything a release publishes: six CLI archives, installers, chart, deployment bundle, SBOM, attestation, image | ~30 min |

Both resolve the same name, `<workspace version>-rc.g<short sha>`, from the
version already in `Cargo.toml`, so a candidate image and a candidate Release
cut from the same commit carry the identical version. A cut of the release
branch **after finalize has run** is `0.2.0-rc.g1a2b3c4`; a cut of `main` today
is `0.1.0-rc.g1a2b3c4`. Repeated cuts of the same version stay distinct.

Dispatching is the only access control on either workflow — it requires write
access to the repository. `release.yml -f candidate=true` ignores the `tag`
input; a candidate names itself off the tree and the commit.

### Why a prerelease is enough

A candidate is not a parallel distribution channel. Every channel already has a
stable/unstable split, and in each one the **prerelease marker is what selects
it**:

| Channel | Why the default resolves past a candidate |
|---|---|
| GitHub Release | `GET /releases/latest` is *the most recent non-prerelease, non-draft release*, and that redirect is what both installers follow when `BLUE_VERSION` is unset |
| Helm / OCI chart | With no `--version`, Helm installs the latest stable version until you pass `--devel`, and `-rc.g…` is a semver prerelease |
| GHCR image | Candidates go to the separate `ghcr.io/blocksorg/blue-rc` package; a release digest only ever lands in `ghcr.io/blocksorg/blue`, `latest` never moves, and the `{{major}}` / `{{major}}.{{minor}}` rollup tags collapse onto the candidate tag |

So a candidate does create a git tag and a GitHub Release — and neither can be
resolved by accident. What it must never do is carry `--latest`.

### Installing a candidate

Pin `BLUE_VERSION`; the installers compose the asset name from it and need no
change:

```bash
curl -fsSL https://raw.githubusercontent.com/BlocksOrg/blue/main/scripts/install.sh \
  | BLUE_VERSION=0.2.0-rc.g1a2b3c4 sh
```

```powershell
$env:BLUE_VERSION = "0.2.0-rc.g1a2b3c4"; iwr -useb https://raw.githubusercontent.com/BlocksOrg/blue/main/scripts/install.ps1 | iex
```

The binary reports the candidate string it was built from — `blue version`
prints `Blue metaharness 0.2.0-rc.g1a2b3c4`, and the same string goes out in the
user-agent and in the `client_version` the control API stores. That is
`BLUE_BUILD_VERSION`, baked in by the release workflow for candidate runs only;
nothing writes it into the tree.

### Deploying a candidate

One asymmetry to know: the candidate chart still defaults `image.repository` to
the **release** package, because the chart's values are part of the tree and the
tree does not know it is being cut as a candidate. Point it at `-rc` yourself:

```bash
helm upgrade --install blue oci://ghcr.io/blocksorg/charts/blue \
  --version 0.2.0-rc.g1a2b3c4 \
  --set image.repository=ghcr.io/blocksorg/blue-rc \
  --set image.digest=sha256:...
```

`--version` is required — Helm skips prereleases otherwise. Production mode
requires `image.digest` rather than a tag; both workflows print the manifest
digest in their run summary.

`ghcr.io/blocksorg/blue-rc` is created on the first push and is private until
someone makes it public.

### Building without publishing

`gh workflow run release.yml --ref <ref> -f tag=v<version>` — the original
dry-run path, unchanged. It assembles all six archives, the chart and the
deployment bundle as a workflow artifact, pushes nothing, and creates no tag.
The tag you pass must match the version in the tree.

## Publishing

Merging the release PR is the publish. Release Please tags `v<version>` and
opens the draft Release; the tag fires the Release workflow, which uploads every
artifact and flips the draft live. Confirm anonymous CLI, image, chart, and
deployment-bundle downloads afterwards.

The manual path still works if you need it — the Release workflow creates the
draft itself when one does not already exist:

```bash
git tag -s v0.2.0 -m "Blue v0.2.0"
git push origin v0.2.0
```

It refuses to rebuild over a tag whose Release is already published, before
spending ~30 minutes on builds.
