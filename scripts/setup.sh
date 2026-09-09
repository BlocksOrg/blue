#!/bin/sh
# One-time developer setup: wire up the git pre-commit hooks (lefthook).
# Safe to re-run. Bypass hooks for a single commit with `git commit --no-verify`.
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

if ! command -v lefthook >/dev/null 2>&1; then
  echo "lefthook not found on PATH." >&2
  echo "Install it, then re-run this script. Options:" >&2
  echo "  brew install lefthook" >&2
  echo "  npm  install -g lefthook" >&2
  echo "  go   install github.com/evilmartians/lefthook@latest" >&2
  echo "  docs: https://github.com/evilmartians/lefthook#install" >&2
  exit 1
fi

lefthook install
echo "Git hooks installed. Formatting, Clippy, TS type-checks, and docs contract sync now run on staged files at commit time."
