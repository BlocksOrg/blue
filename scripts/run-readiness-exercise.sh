#!/usr/bin/env bash
set -euo pipefail

exercise="${1:?usage: $0 <exercise> <budget-seconds> <evidence-directory> -- <command> [args...]}"
budget_seconds="${2:?budget seconds are required}"
evidence_dir="${3:?evidence directory is required}"
shift 3
[[ "${1:-}" == -- ]] || { echo "expected -- before the exercise command" >&2; exit 64; }
shift
[[ $# -gt 0 ]] || { echo "exercise command is required" >&2; exit 64; }
[[ "$exercise" =~ ^[a-z0-9-]+$ ]] || { echo "invalid exercise name" >&2; exit 64; }
[[ "$budget_seconds" =~ ^[1-9][0-9]*$ ]] || { echo "invalid budget" >&2; exit 64; }

mkdir -p "$evidence_dir"
started_epoch="$(date -u +%s)"
started_at="$(date -u +%FT%TZ)"
set +e
"$@"
command_status=$?
set -e
finished_epoch="$(date -u +%s)"
finished_at="$(date -u +%FT%TZ)"
duration_seconds="$((finished_epoch - started_epoch))"
result=passed
if (( command_status != 0 || duration_seconds > budget_seconds )); then result=failed; fi

jq -n \
  --arg exercise "$exercise" \
  --arg operator "${BLUE_EXERCISE_OPERATOR:-unknown}" \
  --arg release_digest "${BLUE_RELEASE_DIGEST:-unknown}" \
  --arg started_at "$started_at" \
  --arg finished_at "$finished_at" \
  --arg result "$result" \
  --argjson command_status "$command_status" \
  --argjson duration_seconds "$duration_seconds" \
  --argjson budget_seconds "$budget_seconds" \
  --argjson evidence_links "${BLUE_EVIDENCE_LINKS_JSON:-[]}" \
  '{exercise:$exercise,operator:$operator,release_digest:$release_digest,started_at:$started_at,finished_at:$finished_at,duration_seconds:$duration_seconds,budget_seconds:$budget_seconds,command_status:$command_status,result:$result,evidence_links:$evidence_links}' \
  > "$evidence_dir/$exercise.json"

[[ "$result" == passed ]]
