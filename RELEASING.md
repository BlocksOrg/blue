# Releasing Blue

Blue uses one semantic version for the CLI, service image, Helm chart, OpenAPI
contract, deployment bundle, and versioned documentation.

## Repository setup

Before the first public release:

- Make the repository public and enable immutable GitHub Releases.
- Allow GitHub Actions to publish packages with the repository `GITHUB_TOKEN`.
- Make `ghcr.io/blocksorg/blue` and the Blue OCI chart package
  publicly readable. Container packages can remain private even when their
  source repository is public.
- Protect `main`, require CI and Documentation checks, and restrict creation of
  `v*` tags to release maintainers.
- Configure GitHub artifact attestations for the organization.

## Version preparation

Update the workspace, CLI, OpenAPI, Helm chart, consumer image, and docs to the
same `MAJOR.MINOR.PATCH`. Commit the immutable documentation snapshot before
tagging, then verify locally:

```bash
scripts/check-release-version.sh v0.1.0
scripts/test-install.sh
cargo test --workspace
```

Run the Release workflow manually with the intended tag to assemble a dry-run
artifact. Inspect the CLI archives, `SHA256SUMS`, SBOM, Helm package, and
deployment bundle.

## Candidate images

To deploy unreleased code, cut a candidate image instead of tagging. The Dev
image workflow builds any ref on demand and publishes it to a separate package,
`ghcr.io/blocksorg/blue-rc`:

```bash
gh workflow run dev-image.yml --ref <branch>
```

It names the candidate `<workspace version>-rc.g<short sha>` from the version
already in `Cargo.toml`, so a cut of `main` today is `0.1.0-rc.g1a2b3c4`.
Repeated cuts of the same version stay distinct, the prerelease suffix keeps a
candidate from ever squatting the tag the real release will take, and the
`{{major}}` / `{{major}}.{{minor}}` rollup tags are suppressed. No git tag, no
GitHub Release, and `latest` never moves.

Only the image is published — no CLI archives, chart or deployment bundle. The
run summary prints the manifest digest; deploy with that rather than the tag,
since production mode requires `image.digest`:

```bash
helm upgrade --install blue oci://ghcr.io/blocksorg/charts/blue \
  --set image.repository=ghcr.io/blocksorg/blue-rc \
  --set image.digest=sha256:...
```

Dispatching requires write access to the repository, which is the only access
control on the workflow. `ghcr.io/blocksorg/blue-rc` is created on the first
push and is private until someone makes it public.

## Publish

Create and push an annotated tag only after the dry run succeeds:

```bash
git tag -s v0.1.0 -m "Blue v0.1.0"
git push origin v0.1.0
```

The tag workflow validates versions, builds every artifact, publishes and
attests the multi-architecture GHCR image, uploads a draft GitHub Release, and
publishes the release after all required jobs succeed. Confirm anonymous CLI,
image, chart, and deployment-bundle downloads after publication.
