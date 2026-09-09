# Releasing Blue

Blue uses one semantic version for the CLI, service image, Helm chart, OpenAPI
contract, deployment bundle, and versioned documentation.

## Repository setup

Before the first public release:

- Make the repository public and enable immutable GitHub Releases.
- Allow GitHub Actions to publish packages with the repository `GITHUB_TOKEN`.
- Make `ghcr.io/blocksorg/governance-harness` and the Blue OCI chart package
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
