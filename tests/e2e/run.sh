#!/usr/bin/env bash
set -euo pipefail

mode="${1:-smoke}"
case "$mode" in
  smoke|full|mtls) ;;
  *) echo "usage: tests/e2e/run.sh [smoke|full|mtls]" >&2; exit 64 ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
compose_file="$repo_root/tests/e2e/docker-compose.yml"
project="${BLUE_E2E_PROJECT:-blue-e2e-${GITHUB_RUN_ID:-local}-$$}"
compose_args=(-p "$project" -f "$compose_file" --profile build)
artifact_dir="$repo_root/tests/e2e/artifacts"
mkdir -p "$artifact_dir/provisioner"
chmod -R a+rwX "$artifact_dir"

# Prebuilt mode: the authoritative deployment + rust-artifacts images were built
# once by the `images.yml` workflow and `docker load`ed by the caller under their
# default tags. The `blue` (custom-provisioner) and `runner` overlays are built
# with the default docker driver so they resolve `FROM` the preloaded images in
# the daemon store — a docker-container buildx builder cannot see `docker load`ed
# images, and would try to pull them from a registry. That also means skipping
# both the buildx/GHA cache overlay (docker-compose.ci.yml, whose `cache_to: gha`
# needs the container driver) and the build overlay (docker-compose.build.yml,
# which rebuilds the base/rust services). The thin overlays are cheap to rebuild.
prebuilt="${BLUE_E2E_PREBUILT:-}"
if [[ "$prebuilt" == "1" ]]; then
  build_compose_args=("${compose_args[@]}")
else
  if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    compose_args+=(-f "$repo_root/tests/e2e/docker-compose.ci.yml")
  fi
  build_compose_args=("${compose_args[@]}" -f "$repo_root/tests/e2e/docker-compose.build.yml")
fi

cleanup() {
  status=$?
  docker compose "${compose_args[@]}" logs --no-color > "$artifact_dir/compose.log" 2>&1 || true
  docker compose "${compose_args[@]}" down -v --remove-orphans || true
  exit "$status"
}
trap cleanup EXIT INT TERM

# In prebuilt mode the base/rust images carry the fixed tags they were saved
# under (see images.yml); point at those. Otherwise use project-scoped tags so
# concurrent local/CI builds never collide.
if [[ "$prebuilt" == "1" ]]; then
  export BLUE_E2E_BASE_IMAGE="${BLUE_E2E_BASE_IMAGE:-blue-e2e-base:local}"
  export BLUE_E2E_RUST_IMAGE="${BLUE_E2E_RUST_IMAGE:-blue-e2e-rust:local}"
else
  export BLUE_E2E_BASE_IMAGE="$project-base:local"
  export BLUE_E2E_RUST_IMAGE="$project-rust:local"
fi
export BLUE_E2E_IMAGE="$project-custom:local"
export HARNESS_PROVISIONER_EXECUTABLE_SHA256="$(printf '0%.0s' {1..64})"

docker compose "${build_compose_args[@]}" --profile test build \
  blue runner
provisioner_sha256="$(docker run --rm --entrypoint sha256sum "$BLUE_E2E_IMAGE" /etc/blue/e2e-provisioner | awk '{print $1}')"
if [[ ! "$provisioner_sha256" =~ ^[0-9a-f]{64}$ ]]; then
  echo "invalid E2E provisioner SHA-256: $provisioner_sha256" >&2
  exit 1
fi
export HARNESS_PROVISIONER_EXECUTABLE_SHA256="$provisioner_sha256"
docker compose "${compose_args[@]}" up -d --wait --no-build blue blue-governance-only docs
docker compose "${compose_args[@]}" --profile test run --no-deps --rm runner ./run-in-container.sh "$mode"
