#!/bin/sh
# Regenerate SQLx's checked-query metadata against an isolated PostgreSQL 16.
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
compose_file="$root/tests/sqlx/docker-compose.yml"
database_url="postgres://harness:harness@127.0.0.1:55432/governance"

cleanup() {
  docker compose -f "$compose_file" down --volumes
}
trap cleanup EXIT INT TERM

docker compose -f "$compose_file" up --detach --wait postgres
DATABASE_URL="$database_url" cargo sqlx migrate run \
  --source "$root/services/control-api/migrations"
DATABASE_URL="$database_url" cargo sqlx prepare --workspace -- \
  --all-targets --all-features
