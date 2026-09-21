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

# --- allowlisted live documentation ----------------------------------------

# Exercise the writer in an isolated minimal repository. Stub the two external
# commands: their behavior is tested elsewhere, while this fixture is concerned
# only with the exact fields set-version.sh owns.
fixture="$tmp/repository"
mkdir -p "$fixture/scripts" "$fixture/crates/gh-cli" \
  "$fixture/deploy/contract" "$fixture/deploy/helm" \
  "$fixture/deploy/consumer/.github/workflows" "$fixture/deploy/consumer/blue" \
  "$fixture/apps/docs/openapi" "$fixture/apps/docs/scripts" \
  "$fixture/apps/docs/next/deployment" "$fixture/apps/docs/next/development" \
  "$fixture/apps/docs/next/cli" "$tmp/bin"
for file in Cargo.toml crates/gh-cli/Cargo.toml deploy/contract/governance.openapi.yaml \
  deploy/helm/Chart.yaml deploy/docker-compose.yml \
  deploy/consumer/.github/workflows/deploy.yml deploy/consumer/blue/blue.yaml \
  deploy/consumer/provisioner.sh apps/docs/openapi/next.yaml \
  apps/docs/next/deployment/production.mdx \
  apps/docs/next/development/local-compose.mdx apps/docs/next/cli/commands.mdx \
  apps/docs/next/development/contributing.mdx; do
  cp "$root/$file" "$fixture/$file"
done
cp "$root/scripts/set-version.sh" "$root/scripts/check-release-version.sh" "$fixture/scripts/"
printf '#!/bin/sh\nexit 0\n' > "$tmp/bin/node"
printf '#!/bin/sh\nexit 0\n' > "$tmp/bin/cargo"
chmod +x "$tmp/bin/node" "$tmp/bin/cargo" "$fixture/deploy/consumer/provisioner.sh"

unrelated='minimum_client_version: 7.6.5'
printf '\n%s\n' "$unrelated" >> "$fixture/apps/docs/next/development/contributing.mdx"
PATH="$tmp/bin:$PATH" "$fixture/scripts/set-version.sh" 9.8.7 >/dev/null
PATH="$tmp/bin:$PATH" "$fixture/scripts/check-release-version.sh" v9.8.7 >/dev/null \
  || fail "release scripts rejected synchronized live documentation fields"
grep -Fx "$unrelated" "$fixture/apps/docs/next/development/contributing.mdx" >/dev/null \
  || fail "set-version.sh changed an unrelated semantic version example"
grep -F 'ghcr.io/blocksorg/blue:${VERSION}' "$fixture/apps/docs/next/deployment/production.mdx" >/dev/null \
  || fail "set-version.sh did not make the production image follow VERSION"

assert_named_mismatch() {
  file="$1"
  expression="$2"
  expected="$3"
  cp "$file" "$file.before-mismatch"
  sed "$expression" "$file.before-mismatch" > "$file"
  output="$("$fixture/scripts/check-release-version.sh" v9.8.7 2>&1 || true)"
  mv "$file.before-mismatch" "$file"
  printf '%s\n' "$output" | grep -F "$expected" >/dev/null \
    || fail "release checker did not identify mismatched field: $expected"
}

assert_named_mismatch "$fixture/apps/docs/next/deployment/production.mdx" \
  's/^VERSION=9\.8\.7$/VERSION=9.8.6/' 'docs production VERSION version is 9.8.6'
assert_named_mismatch "$fixture/apps/docs/next/development/local-compose.mdx" \
  's/^BLUE_DEPLOYMENT_VERSION=9\.8\.7$/BLUE_DEPLOYMENT_VERSION=9.8.6/' 'docs compose version version is 9.8.6'
assert_named_mismatch "$fixture/apps/docs/next/cli/commands.mdx" \
  's/BLUE_VERSION=9\.8\.7/BLUE_VERSION=9.8.6/' 'docs CLI installer version version is 9.8.6'
assert_named_mismatch "$fixture/apps/docs/next/development/contributing.mdx" \
  's/release:docs -- 9\.8\.7/release:docs -- 9.8.6/' 'docs release command version version is 9.8.6'
assert_named_mismatch "$fixture/apps/docs/next/deployment/production.mdx" \
  's/blue:${VERSION}/blue:9.8.7/' 'docs production image reference must use ${VERSION} exactly once'

echo "Release version tests passed"
