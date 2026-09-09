#!/bin/sh
set -eu

tag="${1:-}"
if ! printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "release tag must be vMAJOR.MINOR.PATCH" >&2
  exit 64
fi
version="${tag#v}"

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
workspace="$(sed -n '/\[workspace.package\]/,/^$/s/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml")"
cli="$(sed -n '/\[package\]/,/^$/s/^version = "\([^"]*\)"/\1/p' "$root/crates/gh-cli/Cargo.toml")"
contract="$(sed -n 's/^  version: "\([^"]*\)"/\1/p' "$root/deploy/contract/governance.openapi.yaml" | head -n 1)"
chart="$(sed -n 's/^version: \([^ ]*\)/\1/p' "$root/deploy/helm/Chart.yaml" | head -n 1)"
app="$(sed -n 's/^appVersion: "\([^"]*\)"/\1/p' "$root/deploy/helm/Chart.yaml")"

for pair in "workspace:$workspace" "cli:$cli" "contract:$contract" "chart:$chart" "chart appVersion:$app"; do
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
echo "Release versions match $tag"
