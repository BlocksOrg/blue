#!/usr/bin/env bash
set -euo pipefail

evidence_dir="${1:?usage: $0 <evidence-directory>}"
required=(clean-install rolling-upgrade failed-migration application-rollback database-restore object-restore oauth-key-rotation gateway-credential-rotation user-revocation redis-loss object-store-loss upstream-gateway-outage)
for exercise in "${required[@]}"; do
  file="$evidence_dir/$exercise.json"
  jq -e --arg exercise "$exercise" '
    .exercise == $exercise and .result == "passed" and
    (.operator | length > 0 and . != "unknown") and
    (.release_digest | test("^sha256:[a-f0-9]{64}$")) and
    (.duration_seconds <= .budget_seconds) and
    (.command_status == 0) and (.evidence_links | type == "array")
  ' "$file" >/dev/null || { echo "missing or failed readiness evidence: $exercise" >&2; exit 1; }
done
echo "Production-shaped staging evidence is complete"
