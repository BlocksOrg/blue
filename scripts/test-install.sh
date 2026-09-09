#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t blue-installer-test)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
fixture="$tmp/fixture"
fake_bin="$tmp/bin"
mkdir -p "$fixture/archive" "$fake_bin" "$tmp/home"

printf '#!/bin/sh\necho blue-test\n' >"$fixture/archive/blue"
chmod 0755 "$fixture/archive/blue"
asset="blue-v0.1.0-x86_64-unknown-linux-musl.tar.gz"
tar -czf "$fixture/$asset" -C "$fixture/archive" blue
if command -v sha256sum >/dev/null 2>&1; then
  digest="$(sha256sum "$fixture/$asset" | awk '{print $1}')"
else
  digest="$(shasum -a 256 "$fixture/$asset" | awk '{print $1}')"
fi
printf '%s  %s\n' "$digest" "$asset" >"$fixture/SHA256SUMS"

printf '%s\n' '#!/bin/sh' 'if [ "${1:-}" = "-s" ]; then echo Linux; else echo x86_64; fi' >"$fake_bin/uname"
printf '%s\n' '#!/bin/sh' 'output=""' 'url=""' \
  'while [ "$#" -gt 0 ]; do case "$1" in -o) output="$2"; shift 2 ;; http*) url="$1"; shift ;; *) shift ;; esac; done' \
  'cp "$FIXTURE_DIR/${url##*/}" "$output"' >"$fake_bin/curl"
chmod 0755 "$fake_bin/uname" "$fake_bin/curl"

env HOME="$tmp/home" PATH="$fake_bin:$PATH" FIXTURE_DIR="$fixture" \
  BLUE_VERSION=0.1.0 BLUE_INSTALL_DIR="$tmp/install" BLUE_UPDATE_PATH=0 \
  "$root/scripts/install.sh"
test "$("$tmp/install/blue")" = "blue-test"

printf '%064d  %s\n' 0 "$asset" >"$fixture/SHA256SUMS"
if env HOME="$tmp/home" PATH="$fake_bin:$PATH" FIXTURE_DIR="$fixture" \
  BLUE_VERSION=0.1.0 BLUE_INSTALL_DIR="$tmp/bad" BLUE_UPDATE_PATH=0 \
  "$root/scripts/install.sh" >/dev/null 2>&1; then
  echo "installer accepted an invalid checksum" >&2
  exit 1
fi

echo "Installer tests passed"
