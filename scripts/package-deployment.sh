#!/bin/sh
set -eu

version="${1:-}"
output_dir="${2:-dist}"
case "$version" in v*) tag="$version"; plain="${version#v}" ;; *) tag="v$version"; plain="$version" ;; esac
# Mirrors scripts/check-release-version.sh's accepted tag shapes. Unlike that
# script, the candidate suffix is kept in full here: the bundle a candidate
# ships is named for the candidate, blue-deployment-v0.2.0-rc.gSHA.tar.gz.
if ! printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-rc\.g[0-9a-f]{7})?$'; then
  echo "usage: $0 <vMAJOR.MINOR.PATCH[-rc.g<sha7>]> [output-directory]" >&2
  exit 64
fi

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t blue-deployment)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
bundle="$tmp/blue-deployment-$plain"
mkdir -p "$bundle/chart" "$bundle/infra"
cp -R "$root/deploy/consumer/." "$bundle/"
cp -R "$root/deploy/helm" "$bundle/chart/blue"
cp -R "$root/deploy/tofu/aws" "$bundle/infra/aws"
rm -rf "$bundle/infra/aws/.terraform"
cp "$root/LICENSE" "$bundle/LICENSE"
mkdir -p "$output_dir"
tar -czf "$output_dir/blue-deployment-$tag.tar.gz" -C "$tmp" "blue-deployment-$plain"
echo "$output_dir/blue-deployment-$tag.tar.gz"
