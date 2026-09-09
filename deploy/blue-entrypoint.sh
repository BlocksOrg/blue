#!/usr/bin/env bash
set -euo pipefail

component="${1:-all}"
shift || true

case "$component" in
  control-api)
    exec env HARNESS_LISTEN="${BLUE_CONTROL_API_LISTEN:-${HARNESS_LISTEN:-0.0.0.0:8080}}" /usr/local/bin/control-api "$@"
    ;;
  dashboard)
    exec env HOSTNAME="${BLUE_DASHBOARD_HOSTNAME:-0.0.0.0}" PORT="${PORT:-3000}" node /opt/blue/dashboard/dashboard-start.mjs "$@"
    ;;
  inference-proxy)
    exec env HARNESS_LISTEN="${BLUE_INFERENCE_PROXY_LISTEN:-${HARNESS_LISTEN:-0.0.0.0:8081}}" /usr/local/bin/inference-proxy "$@"
    ;;
  migrate)
    exec /usr/local/bin/control-api migrate "$@"
    ;;
  all) ;;
  *)
    echo "blue-entrypoint: expected all, control-api, dashboard, inference-proxy, or migrate; got $component" >&2
    exit 64
    ;;
esac

if [[ ! -r "${BLUE_CONFIG_FILE:-/etc/blue/blue.yaml}" ]]; then
  echo "blue-entrypoint: BLUE_CONFIG_FILE is not readable" >&2
  exit 78
fi

pids=()
names=()
start() {
  names+=("$1")
  shift
  "$@" &
  pids+=("$!")
}

terminate() {
  trap - TERM INT
  for pid in "${pids[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
  for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap terminate TERM INT

start control-api env HARNESS_LISTEN="${BLUE_CONTROL_API_LISTEN:-0.0.0.0:8080}" /usr/local/bin/control-api
start dashboard env HOSTNAME="${BLUE_DASHBOARD_HOSTNAME:-0.0.0.0}" PORT="${PORT:-3000}" node /opt/blue/dashboard/dashboard-start.mjs
if [[ "${BLUE_ENABLE_INFERENCE_PROXY:-false}" == "true" ]]; then
  start inference-proxy env HARNESS_LISTEN="${BLUE_INFERENCE_PROXY_LISTEN:-0.0.0.0:8081}" /usr/local/bin/inference-proxy
fi

while true; do
  for i in "${!pids[@]}"; do
    if ! kill -0 "${pids[$i]}" 2>/dev/null; then
      set +e
      wait "${pids[$i]}"
      status=$?
      set -e
      echo "blue-entrypoint: ${names[$i]} exited with status $status" >&2
      terminate
      exit "$status"
    fi
  done
  sleep 1
done
