#!/bin/sh
set -eu

tag="${1:-}"
# The optional `-rc.g<sha7>` suffix is a release candidate, cut from an
# arbitrary ref by `.github/workflows/release.yml` with `candidate=true`. Its
# tree still carries the plain version the seven literals below agree on — the
# candidate string is baked into the artifacts, never written into the tree — so
# only the MAJOR.MINOR.PATCH core is compared. Anything else (`-beta`,
# `-rc.zzz`, a bare `-rc`) is still rejected: a tag shape nothing produces.
if ! printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-rc\.g[0-9a-f]{7})?$'; then
  echo "release tag must be vMAJOR.MINOR.PATCH, optionally -rc.g<sha7>" >&2
  exit 64
fi
version="${tag#v}"
version="${version%%-*}"

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
workspace="$(sed -n '/\[workspace.package\]/,/^$/s/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml")"
cli="$(sed -n '/\[package\]/,/^$/s/^version = "\([^"]*\)"/\1/p' "$root/crates/gh-cli/Cargo.toml")"
contract="$(sed -n 's/^  version: "\([^"]*\)"/\1/p' "$root/deploy/contract/governance.openapi.yaml" | head -n 1)"
chart="$(sed -n 's/^version: \([^ ]*\)/\1/p' "$root/deploy/helm/Chart.yaml" | head -n 1)"
app="$(sed -n 's/^appVersion: "\([^"]*\)"/\1/p' "$root/deploy/helm/Chart.yaml")"
compose="$(sed -n 's/^.*BLUE_DEPLOYMENT_VERSION:-\([^}]*\)}.*$/\1/p' "$root/deploy/docker-compose.yml" | head -n 1)"
# Copied verbatim into blue-deployment-v<tag>.tar.gz by package-deployment.sh,
# so a stale literal here ships the wrong version inside the release bundle.
consumer="$(sed -n 's/^.*--build-arg BLUE_VERSION=\([^ ]*\).*$/\1/p' "$root/deploy/consumer/.github/workflows/deploy.yml" | head -n 1)"

for pair in "workspace:$workspace" "cli:$cli" "contract:$contract" "chart:$chart" \
  "chart appVersion:$app" "compose default:$compose" "consumer build arg:$consumer"; do
  name="${pair%%:*}"
  actual="${pair#*:}"
  if [ "$actual" != "$version" ]; then
    echo "$name version is $actual, expected $version" >&2
    exit 1
  fi
done
test -d "$root/apps/docs/next" || { echo "missing apps/docs/next" >&2; exit 1; }
test -f "$root/apps/docs/openapi/next.yaml" || { echo "missing apps/docs/openapi/next.yaml" >&2; exit 1; }
test -f "$root/deploy/consumer/blue/blue.yaml" || {
  echo "consumer configuration must be blue/blue.yaml" >&2
  exit 1
}
test -x "$root/deploy/consumer/provisioner.sh" || {
  echo "consumer provisioner executable is missing or not executable" >&2
  exit 1
}
echo "Release versions match $version (tag $tag)"
