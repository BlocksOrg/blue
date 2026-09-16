#!/usr/bin/env bash
# Host orchestrator for the e2e-slim suite: build the control-api image, stand up
# the lean stack, then run the Rust nextest suite on the host against it. Additive
# to tests/e2e — distinct compose project and cache scopes mean the two suites
# never interfere.
#
# Two modes, chosen automatically; each runs its own nextest profile (see
# .config/nextest.toml) so the suites stay disjoint:
#   * governance-only (default, secret-free): control-api-only slim image; runs
#     the bootstrap/config/session-upload tests (profile `governance`).
#   * gateway (opt-in): when OPENROUTER_API_KEY is set, also brings up a real
#     LiteLLM (fed by OpenRouter) + inference-proxy via docker-compose.gateway.yml
#     and runs only the per-agent certification tests (profile `gateway`).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
slim_dir="$repo_root/tests/e2e-slim"
compose_file="$slim_dir/docker-compose.yml"
project="${BLUE_E2E_SLIM_PROJECT:-blue-e2e-slim-${GITHUB_RUN_ID:-local}-$$}"
artifact_dir="$slim_dir/artifacts"
mkdir -p "$artifact_dir"

# Host state the CLI reaches on the real path: `blue run` launches the MCP server
# and `blue apply` fetches the managed skill package, both on the host, so stage
# them at fixed host paths the governance config points at.
state_dir="/tmp/blue-e2e"
node "$repo_root/tests/e2e-native/prepare-fixtures.mjs" "$state_dir" --legacy

gateway=0
if [[ -n "${OPENROUTER_API_KEY:-}" ]]; then
  gateway=1
  echo "==> OPENROUTER_API_KEY present: enabling the gateway path"
  # control-api verifies the mounted executable provisioner against this pin
  # and refuses to start on a mismatch (same guard as the full e2e suite).
  provisioner_sha256="$(node -e 'const fs=require("node:fs"),crypto=require("node:crypto");console.log(crypto.createHash("sha256").update(fs.readFileSync(process.argv[1])).digest("hex"))' "$slim_dir/fixtures/litellm-provisioner.mjs")"
  if [[ ! "$provisioner_sha256" =~ ^[0-9a-f]{64}$ ]]; then
    echo "invalid e2e-slim provisioner SHA-256: $provisioner_sha256" >&2
    exit 1
  fi
  export HARNESS_PROVISIONER_EXECUTABLE_SHA256="$provisioner_sha256"
fi

# Prebuilt mode: the authoritative full deployment image was built once by the
# `images.yml` workflow and `docker load`ed by the caller under its saved tag
# (blue-e2e-base:local). Point the stack at it and skip building entirely — no
# build overlay, and no GHA buildx-cache overlay (its `cache_to: gha` needs the
# docker-container driver, which can't see `docker load`ed images).
prebuilt="${BLUE_E2E_SLIM_PREBUILT:-}"

compose_args=(-p "$project" -f "$compose_file")
if [[ "$gateway" == "1" ]]; then
  compose_args+=(-f "$slim_dir/docker-compose.gateway.yml")
fi
if [[ "$prebuilt" != "1" && "${GITHUB_ACTIONS:-}" == "true" ]]; then
  if [[ "$gateway" == "1" ]]; then
    compose_args+=(-f "$slim_dir/docker-compose.gateway.ci.yml")
  else
    compose_args+=(-f "$slim_dir/docker-compose.ci.yml")
  fi
fi

cleanup() {
  status=$?
  docker compose "${compose_args[@]}" logs --no-color > "$artifact_dir/compose.log" 2>&1 || true
  docker compose "${compose_args[@]}" down -v --remove-orphans || true
  if [[ "${E2E_SLIM_PRUNE:-}" == "1" ]]; then
    docker image prune -f || true
    docker builder prune -f || true
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

# The whole suite now runs control-api — and, on the gateway path, also
# inference-proxy — from the single full deployment image (it ships both
# binaries). Prebuilt: reuse the loaded image. Otherwise build it once.
if [[ "$gateway" == "1" ]]; then
  export LITELLM_MASTER_KEY="${LITELLM_MASTER_KEY:-sk-e2e-slim-master}"
fi

if [[ "$prebuilt" == "1" ]]; then
  export BLUE_E2E_SLIM_IMAGE="${BLUE_E2E_SLIM_IMAGE:-blue-e2e-base:local}"
  echo "==> Using prebuilt deployment image ($BLUE_E2E_SLIM_IMAGE)"
else
  export BLUE_E2E_SLIM_IMAGE="${BLUE_E2E_SLIM_IMAGE:-$project-full:local}"
  build_compose_args=("${compose_args[@]}")
  if [[ "$gateway" != "1" ]]; then
    # The gateway overlay already carries control-api's build stanza; the
    # governance-only stack needs the build overlay to supply it.
    build_compose_args+=(-f "$slim_dir/docker-compose.build.yml")
  fi
  echo "==> Building full deployment image ($BLUE_E2E_SLIM_IMAGE)"
  docker compose "${build_compose_args[@]}" build control-api
fi

echo "==> Starting slim stack (project $project)"
docker compose "${compose_args[@]}" up -d --wait --no-build

# Optionally install the pinned agent CLIs so the managed-config tests run.
# Without them those tests self-skip; the session-upload and login tests still
# run. Gated because a fresh npm-global install is slow.
if [[ "${E2E_SLIM_INSTALL_AGENTS:-}" == "1" ]]; then
  lock="$repo_root/tests/e2e/agents.lock.json"
  npm_prefix="${E2E_SLIM_NPM_PREFIX:-$slim_dir/artifacts/npm-global}"
  mkdir -p "$npm_prefix"
  export NPM_CONFIG_PREFIX="$npm_prefix"
  export PATH="$npm_prefix/bin:$PATH"
  echo "==> Installing pinned agent CLIs into $npm_prefix"
  for agent in codex claude kimi opencode; do
    pkg="$(jq -r ".agents.${agent}.package" "$lock")"
    version="$(jq -r ".agents.${agent}.version" "$lock")"
    echo "    $agent: $pkg@$version"
    npm install -g "${pkg}@${version}"
  done

  # Version matrix (opt-in): install each historical `(agent, version)` sample
  # from agents.matrix.json into its OWN npm prefix so multiple builds coexist,
  # then record every cell's bin dir in a manifest the Rust matrix tests read
  # (E2E_SLIM_AGENT_MATRIX). The lock pin is included as a cell too, pointing at
  # the global install above, so the current pin is covered by the same tests.
  if [[ "${E2E_SLIM_MATRIX:-}" == "1" ]]; then
    matrix_file="$slim_dir/agents.matrix.json"
    matrix_root="$artifact_dir/agent-matrix"
    manifest="$artifact_dir/agent-matrix.json"
    mkdir -p "$matrix_root"
    echo "==> Installing version-matrix agent CLIs into $matrix_root"
    manifest_json='{}'
    for agent in codex claude kimi opencode; do
      pkg="$(jq -r ".agents.${agent}.package" "$lock")"
      # Lock pin cell -> the global install bin dir.
      pin_version="$(jq -r ".agents.${agent}.version" "$lock")"
      manifest_json="$(jq -n --argjson m "$manifest_json" \
        --arg a "$agent" --arg v "$pin_version" --arg d "$npm_prefix/bin" \
        '$m | .[$a][$v] = $d')"
      # Historical sample cells -> per-version prefixes.
      count="$(jq -r ".agents.${agent}.versions | length" "$matrix_file")"
      for ((i = 0; i < count; i++)); do
        version="$(jq -r ".agents.${agent}.versions[$i].version" "$matrix_file")"
        prefix="$matrix_root/$agent/$version"
        mkdir -p "$prefix"
        echo "    matrix $agent: $pkg@$version"
        NPM_CONFIG_PREFIX="$prefix" npm install -g "${pkg}@${version}"
        manifest_json="$(jq -n --argjson m "$manifest_json" \
          --arg a "$agent" --arg v "$version" --arg d "$prefix/bin" \
          '$m | .[$a][$v] = $d')"
      done
    done
    echo "$manifest_json" > "$manifest"
    export E2E_SLIM_AGENT_MATRIX="$manifest"
    echo "==> Wrote agent matrix manifest to $manifest"
  fi
elif [[ "${E2E_SLIM_MATRIX:-}" == "1" ]]; then
  echo "E2E_SLIM_MATRIX=1 requires E2E_SLIM_INSTALL_AGENTS=1 (matrix cells are real CLI installs)" >&2
  exit 1
fi

# Ensure the test runner (cargo-nextest) is available.
if ! cargo nextest --version >/dev/null 2>&1; then
  echo "==> Installing cargo-nextest"
  cargo install cargo-nextest --locked
fi

echo "==> Building the blue binary"
cargo build -p gh-cli --bin blue
export E2E_SLIM_BLUE_BIN="$repo_root/target/debug/blue"

export E2E_SLIM_CONTROL_API_URL="http://127.0.0.1:8080"
export E2E_SLIM_DATABASE_URL="postgres://harness:harness@127.0.0.1:5432/governance"
export E2E_SLIM_MINIO_URL="http://127.0.0.1:9000"

if [[ "$gateway" == "1" ]]; then
  export E2E_SLIM_INFERENCE_PROXY_URL="http://127.0.0.1:8081"
  export E2E_SLIM_LITELLM_URL="http://127.0.0.1:4000"
  export E2E_SLIM_LITELLM_MASTER_KEY="$LITELLM_MASTER_KEY"
  export E2E_SLIM_OPENROUTER=1
fi

# Explicit suite selection per mode (see .config/nextest.toml): governance runs
# everything except the certification tests; gateway runs only them.
profile="governance"
if [[ "$gateway" == "1" ]]; then
  profile="gateway"
fi

echo "==> Running e2e-slim nextest suite (profile $profile)"
# e2e-slim is its own workspace (kept out of the root workspace so its test-only
# deps stay out of the deployment image's cargo-chef recipe), so target it by
# manifest path rather than `-p`.
cargo nextest run --manifest-path "$slim_dir/Cargo.toml" --profile "$profile" \
  --no-fail-fast 2>&1 | tee "$artifact_dir/nextest.log"
