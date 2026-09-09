#!/bin/sh
set -eu

version="${1:-}"
output_dir="${2:-dist}"
case "$version" in v*) tag="$version"; plain="${version#v}" ;; *) tag="v$version"; plain="$version" ;; esac
if ! printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "usage: $0 <vMAJOR.MINOR.PATCH> [output-directory]" >&2
  exit 64
fi

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t blue-deployment)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
bundle="$tmp/blue-deployment-$plain"
mkdir -p "$bundle/chart" "$bundle/infra"
cp -R "$root/deploy/consumer/." "$bundle/"
cp -R "$root/deploy/helm" "$bundle/chart/blue"
cp -R "$root/deploy/helm-prerequisites" "$bundle/chart/blue-prerequisites"
cp -R "$root/deploy/tofu/aws" "$bundle/infra/aws"
rm -rf "$bundle/infra/aws/.terraform"
cp "$root/LICENSE" "$bundle/LICENSE"
mkdir -p "$output_dir"
tar -czf "$output_dir/blue-deployment-$tag.tar.gz" -C "$tmp" "blue-deployment-$plain"
echo "$output_dir/blue-deployment-$tag.tar.gz"
