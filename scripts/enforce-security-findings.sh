#!/usr/bin/env bash
set -euo pipefail

exceptions_file="${SECURITY_EXCEPTIONS_FILE:-.github/security-exceptions.yml}"
today="$(date -u +%F)"
supported='["cargo-deny","npm-audit","gitleaks","trivy","checkov"]'

validate() {
  jq -e --arg today "$today" --argjson supported "$supported" '
    .exceptions | type == "array" and
    (map([.scanner, .identifier] | join(":")) | length == (unique | length)) and
    all(.[];
      (.scanner as $scanner | $supported | index($scanner) != null) and
      (.identifier | type == "string" and length > 0) and
      (.owner | type == "string" and length > 0) and
      (.rationale | type == "string" and length > 0) and
      (.compensating_control | type == "string" and length > 0) and
      (.expires | type == "string" and test("^[0-9]{4}-[0-9]{2}-[0-9]{2}$") and
        ((. + "T00:00:00Z" | fromdateiso8601) >= ($today + "T00:00:00Z" | fromdateiso8601)))
    )
  ' "$exceptions_file" >/dev/null
}

validate
[[ "${1:-}" == validate ]] && exit 0

scanner="${1:?usage: $0 <scanner> <finding-ids.json> [scanner-exit-code]}"
findings_file="${2:?usage: $0 <scanner> <finding-ids.json> [scanner-exit-code]}"
scanner_status="${3:-0}"
jq -e --arg scanner "$scanner" --argjson supported "$supported" '$supported | index($scanner) != null' <<<"null" >/dev/null
jq -e 'type == "array" and all(.[]; type == "string" and length > 0)' "$findings_file" >/dev/null

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
jq -S 'unique' "$findings_file" > "$tmp_dir/findings.json"
jq -S --arg scanner "$scanner" '[.exceptions[] | select(.scanner == $scanner) | .identifier] | unique' "$exceptions_file" > "$tmp_dir/exceptions.json"
jq -n --slurpfile findings "$tmp_dir/findings.json" --slurpfile exceptions "$tmp_dir/exceptions.json" '$findings[0] - $exceptions[0]' > "$tmp_dir/unexcepted.json"
jq -n --slurpfile findings "$tmp_dir/findings.json" --slurpfile exceptions "$tmp_dir/exceptions.json" '$exceptions[0] - $findings[0]' > "$tmp_dir/unused.json"

if [[ "$scanner_status" != 0 ]] && [[ "$(jq length "$tmp_dir/findings.json")" == 0 ]]; then
  echo "$scanner failed with status $scanner_status without a parseable finding" >&2
  exit "$scanner_status"
fi
if [[ "$(jq length "$tmp_dir/unused.json")" != 0 ]]; then
  echo "$scanner has unused exceptions; remove them:" >&2
  jq -r '.[]' "$tmp_dir/unused.json" >&2
  exit 1
fi
if [[ "$(jq length "$tmp_dir/unexcepted.json")" != 0 ]]; then
  echo "$scanner reported blocking findings:" >&2
  jq -r '.[]' "$tmp_dir/unexcepted.json" >&2
  exit 1
fi

count="$(jq length "$tmp_dir/findings.json")"
echo "$scanner: $count blocking finding(s), all covered by active exceptions"
