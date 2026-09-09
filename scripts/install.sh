#!/bin/sh
set -eu

REPOSITORY="${BLUE_REPOSITORY:-BlocksOrg/blue}"
INSTALL_DIR="${BLUE_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${BLUE_VERSION:-}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "blue installer: required command not found: $1" >&2
    exit 1
  }
}

need curl
need tar

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-musl" ;;
  *)
    echo "blue installer: unsupported operating system: $(uname -s)" >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64) arch="x86_64" ;;
  *)
    echo "blue installer: unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

if [ -z "$VERSION" ]; then
  latest_url="$(curl --proto '=https' --tlsv1.2 -LsS -o /dev/null -w '%{url_effective}' \
    "https://github.com/$REPOSITORY/releases/latest")"
  VERSION="${latest_url##*/}"
fi
case "$VERSION" in
  v*) ;;
  *) VERSION="v$VERSION" ;;
esac

asset="blue-$VERSION-$arch-$os.tar.gz"
base_url="https://github.com/$REPOSITORY/releases/download/$VERSION"
tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t blue-cli)"
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 -LsSf "$base_url/$asset" -o "$tmp_dir/$asset"
curl --proto '=https' --tlsv1.2 -LsSf "$base_url/SHA256SUMS" -o "$tmp_dir/SHA256SUMS"

expected="$(awk -v name="$asset" '$2 == name { print $1 }' "$tmp_dir/SHA256SUMS")"
if [ -z "$expected" ]; then
  echo "blue installer: $asset is missing from SHA256SUMS" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp_dir/$asset" | awk '{print $1}')"
else
  need shasum
  actual="$(shasum -a 256 "$tmp_dir/$asset" | awk '{print $1}')"
fi
if [ "$actual" != "$expected" ]; then
  echo "blue installer: checksum verification failed for $asset" >&2
  exit 1
fi

tar -xzf "$tmp_dir/$asset" -C "$tmp_dir"
test -x "$tmp_dir/blue" || {
  echo "blue installer: archive does not contain an executable blue binary" >&2
  exit 1
}
mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp_dir/blue" "$INSTALL_DIR/blue"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    if [ "${BLUE_UPDATE_PATH:-1}" = "1" ] && [ "$INSTALL_DIR" = "$HOME/.local/bin" ]; then
      if [ "$(uname -s)" = "Darwin" ]; then profile="$HOME/.zprofile"; else profile="$HOME/.profile"; fi
      marker='# Added by the Blue CLI installer'
      if ! grep -F "$marker" "$profile" >/dev/null 2>&1; then
        {
          printf '\n%s\n' "$marker"
          printf 'export PATH="$HOME/.local/bin:$PATH"\n'
        } >>"$profile"
      fi
    fi
    echo "Add $INSTALL_DIR to PATH or open a new shell before running blue." >&2
    ;;
esac

echo "Installed Blue $VERSION to $INSTALL_DIR/blue"
