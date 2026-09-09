#!/usr/bin/env bash
set -euo pipefail

mode="${1:-smoke}"
case "$mode" in
  smoke) exec npm run test:smoke ;;
  full) exec npm run test:full ;;
  mtls) exec npx playwright test specs/gateway-m2m.spec.ts ;;
  *) echo "usage: $0 [smoke|full|mtls]" >&2; exit 64 ;;
esac
