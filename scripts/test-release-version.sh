#!/bin/sh
# Tag-shape tests for the two scripts that gate a release on its tag:
# check-release-version.sh (read side) and package-deployment.sh (bundle name).
#
# Both accept a release candidate's `-rc.g<sha7>` suffix, and the interesting
# property is what they do NOT accept: the suffix is the only one the release
# workflow can produce, so any other prerelease spelling must still be a hard
# reject rather than quietly building a bundle nobody can resolve.
#
# Runs against the working tree's own version, so it needs no fixture and stays
# correct across every release.
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t blue-release-version-test)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

version="$(sed -n '/\[workspace.package\]/,/^$/s/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml")"
candidate="v$version-rc.g1a2b3c4"

fail() {
  echo "$1" >&2
  exit 1
}

accepts() {
  "$@" >/dev/null 2>&1
}

# --- check-release-version.sh -----------------------------------------------

accepts "$root/scripts/check-release-version.sh" "v$version" \
  || fail "check-release-version.sh rejected the tree's own tag v$version"
accepts "$root/scripts/check-release-version.sh" "$candidate" \
  || fail "check-release-version.sh rejected candidate tag $candidate"

# A candidate is compared on its MAJOR.MINOR.PATCH core only, so a core that
# disagrees with the tree must still fail — the suffix cannot launder it.
! accepts "$root/scripts/check-release-version.sh" "v9999.0.0-rc.g1a2b3c4" \
  || fail "check-release-version.sh accepted a candidate whose core does not match the tree"

for bad in "v$version-rc.zzz" "v$version-beta" "v$version-rc" "v$version-rc.gzzzzzzz" \
  "v$version-rc.g1a2b3c" "v$version-rc.g1a2b3c45" "$version" "v$version.0" ""; do
  ! accepts "$root/scripts/check-release-version.sh" "$bad" \
    || fail "check-release-version.sh accepted '$bad'"
done

# --- package-deployment.sh --------------------------------------------------

# The candidate suffix survives into the bundle name, unlike in the check above.
"$root/scripts/package-deployment.sh" "$candidate" "$tmp" >/dev/null
test -f "$tmp/blue-deployment-$candidate.tar.gz" \
  || fail "package-deployment.sh did not name the bundle blue-deployment-$candidate.tar.gz"
tar -tzf "$tmp/blue-deployment-$candidate.tar.gz" >/dev/null \
  || fail "candidate deployment bundle is not readable"

"$root/scripts/package-deployment.sh" "v$version" "$tmp" >/dev/null
test -f "$tmp/blue-deployment-v$version.tar.gz" \
  || fail "package-deployment.sh did not name the plain bundle blue-deployment-v$version.tar.gz"

for bad in "v$version-rc.zzz" "v$version-beta" "v$version-rc" ""; do
  ! accepts "$root/scripts/package-deployment.sh" "$bad" "$tmp" \
    || fail "package-deployment.sh accepted '$bad'"
done

echo "Release version tests passed"
