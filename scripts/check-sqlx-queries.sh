#!/bin/sh
# Static SQL must use SQLx's compile-time checked macros.
# Genuinely-dynamic / DDL / bootstrap statements may opt out with a
# `// sqlx-guard: allow-raw <reason>` comment, placed either on the
# sqlx::query line itself or on the line directly above it (rustfmt sometimes
# reflows a trailing comment off a wrapped call, so both spots are accepted).
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
sources="$root/services/control-api/src"

violations="$(
  find "$sources" -name '*.rs' -print0 |
    xargs -0 awk '
      FNR == 1 { prev = "" }
      {
        if ($0 ~ /sqlx::query(_as|_scalar)?(::[^!(]*)?\(/ &&
            $0 !~ /sqlx-guard: allow-raw/ &&
            prev !~ /sqlx-guard: allow-raw/) {
          printf("%s:%d:%s\n", FILENAME, FNR, $0)
        }
        prev = $0
      }
    '
)"

if [ -n "$violations" ]; then
  printf '%s\n' "$violations" >&2
  echo "unchecked SQLx query call found; use query!, query_as!, or query_scalar! (or annotate a genuinely-dynamic/DDL call with '// sqlx-guard: allow-raw <reason>' on the query line or the line directly above it)" >&2
  exit 1
fi
