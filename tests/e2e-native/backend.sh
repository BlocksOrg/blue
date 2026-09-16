#!/usr/bin/env bash
# Runs on the disposable Linux backend; never clones a branch or fetches credentials.
set -euo pipefail
cd "$(dirname "$0")/../.."
operation="${1:?up|down|logs}"
suite="${2:?governance|gateway}"
case "$suite" in governance|gateway) ;; *) exit 1 ;; esac
export BLUE_E2E_SLIM_IMAGE=blue-e2e-base:local
export E2E_NATIVE_FIXTURES="${E2E_NATIVE_FIXTURES:-/opt/blue-e2e/fixtures}"
export HARNESS_PROVISIONER_EXECUTABLE_SHA256
HARNESS_PROVISIONER_EXECUTABLE_SHA256="$(sha256sum tests/e2e-slim/fixtures/litellm-provisioner.mjs | cut -d' ' -f1)"
args=(-p blue-e2e-native -f tests/e2e-slim/docker-compose.yml)
if [[ "$suite" == gateway ]]; then args+=(-f tests/e2e-slim/docker-compose.gateway.yml); fi
args+=(-f tests/e2e-slim/docker-compose.native.yml)
if [[ "$suite" == gateway ]]; then args+=(-f tests/e2e-slim/docker-compose.native-gateway.yml); fi
# Secrets exist only on this dedicated backend. Never enable shell tracing.
if [[ -f /opt/blue-e2e/backend.env ]]; then
  set -a
  source /opt/blue-e2e/backend.env
  set +a
fi
case "$operation" in
  up)
    chmod +x tests/e2e-slim/fixtures/litellm-provisioner.mjs
    docker compose "${args[@]}" up -d --wait --wait-timeout 240 --no-build
    ;;
  down) docker compose "${args[@]}" down -v --remove-orphans ;;
  logs) docker compose "${args[@]}" logs --no-color | python3 tests/e2e-native/sanitize-log.py ;;
  *) exit 1 ;;
esac
