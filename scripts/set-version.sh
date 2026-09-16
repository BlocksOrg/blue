#!/bin/sh
# Write one MAJOR.MINOR.PATCH across every version-bearing file Blue ships.
#
# The write-side mirror of scripts/check-release-version.sh: every file written
# here is a file that script reads, and every file that script reads is written
# here. Keep the two in lockstep — CI runs the check on every PR, so a file that
# drifts out of this script fails `packaging` rather than shipping wrong.
#
# Normally invoked by the `finalize` job of .github/workflows/release-please.yml
# against the open release PR, but it is an ordinary script: run it by hand when
# preparing a release without the bot.
#
# Deliberately NOT written:
#   scripts/test-install.sh          self-contained fixture; its literals must
#                                    match each other, not the release
#   minimum_client_version (x3)      governance policy floor, not the release
#                                    version — bumping it would force every
#                                    client to upgrade on every release
#   tests/e2e-slim/Cargo.toml        separate workspace, never shipped
#   .github/workflows/release.yml    cosmetic `workflow_dispatch` default
#   apps/docs/next/**/*.mdx          prose; an automated rewrite here gets
#                                    frozen immutably into the docs snapshot
set -eu

version="${1:-}"
if ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "usage: set-version.sh MAJOR.MINOR.PATCH" >&2
  exit 64
fi

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

# Rewrite through a tempfile in the same directory, then rename. `sed -i` is a
# GNU extension that takes a mandatory argument on BSD/macOS, and contributors
# run these locally.
rewrite() {
  file="$1"
  shift
  "$@" < "$file" > "$file.set-version.tmp"
  mv "$file.set-version.tmp" "$file"
}

# Cargo manifests: scope to the section. `[workspace.dependencies]` below
# `[workspace.package]` carries its own `version = "…"` lines for third-party
# crates, and a global substitution would rewrite all of them.
toml_version() {
  awk -v section="$1" -v version="$2" '
    /^\[/ { current = $0 }
    current == section && /^version = "/ {
      print "version = \"" version "\""
      next
    }
    { print }
  '
}

rewrite Cargo.toml toml_version '[workspace.package]' "$version"
rewrite crates/gh-cli/Cargo.toml toml_version '[package]' "$version"

# The contract's `info.version`, and only that: L1626 is
# `client_version: { … example: 0.1.0 }`, which a global substitution corrupts.
# The emitted byte pattern is exact — four `$`-anchored consumers match on
# `^  version: "X.Y.Z"$`.
rewrite deploy/contract/governance.openapi.yaml awk -v version="$version" '
  /^[^[:space:]#]/ { top = $0 }
  top == "info:" && /^  version: "/ {
    print "  version: \"" version "\""
    next
  }
  { print }
'

# `version:` is unquoted and `appVersion:` is quoted; check-release-version.sh
# depends on the difference to tell them apart.
rewrite deploy/helm/Chart.yaml awk -v version="$version" '
  /^version:/ { print "version: " version; next }
  /^appVersion:/ { print "appVersion: \"" version "\""; next }
  { print }
'

rewrite deploy/docker-compose.yml awk -v version="$version" '
  { sub(/BLUE_DEPLOYMENT_VERSION:-[^}]*}/, "BLUE_DEPLOYMENT_VERSION:-" version "}"); print }
'

# Shipped inside blue-deployment-v<tag>.tar.gz by scripts/package-deployment.sh,
# so a stale literal here ships a wrong version to every consumer.
rewrite deploy/consumer/.github/workflows/deploy.yml awk -v version="$version" '
  { sub(/--build-arg BLUE_VERSION=[^ ]*/, "--build-arg BLUE_VERSION=" version); print }
'

# Not a rewrite — apps/docs/openapi/next.yaml is a byte-for-byte copy of the
# canonical contract, asserted by apps/docs/scripts/check-contract.mjs. Copying
# it makes that identity structural instead of two rewrites happening to agree.
node apps/docs/scripts/sync-contract.mjs

# `--workspace` restricts the resolve to workspace members, so third-party pins
# do not move — including `pin-utils` and `wasite`, which also sit at 0.1.0.
# Not `--offline`: on a cold runner ~/.cargo/registry is empty and the resolve
# re-validates against the index, so `--offline` fails outright.
cargo update --workspace --quiet

scripts/check-release-version.sh "v$version"
