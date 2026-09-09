#!/usr/bin/env bash
set -euo pipefail

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
findings="$tmp_dir/findings.json"
exceptions="$tmp_dir/exceptions.json"

printf '["CVE-2099-0001"]\n' > "$findings"
printf '{"exceptions":[]}\n' > "$exceptions"
if SECURITY_EXCEPTIONS_FILE="$exceptions" scripts/enforce-security-findings.sh trivy "$findings" >/dev/null 2>&1; then
  echo "unexcepted finding unexpectedly passed" >&2
  exit 1
fi

jq -n '{exceptions:[{scanner:"trivy",identifier:"CVE-2099-0001",owner:"security@example.com",rationale:"test fixture",compensating_control:"isolated test image",expires:"2999-01-01"}]}' > "$exceptions"
SECURITY_EXCEPTIONS_FILE="$exceptions" scripts/enforce-security-findings.sh trivy "$findings" >/dev/null

printf '[]\n' > "$findings"
if SECURITY_EXCEPTIONS_FILE="$exceptions" scripts/enforce-security-findings.sh trivy "$findings" >/dev/null 2>&1; then
  echo "unused exception unexpectedly passed" >&2
  exit 1
fi

jq '.exceptions[0].expires = "2000-01-01"' "$exceptions" > "$tmp_dir/expired.json"
if SECURITY_EXCEPTIONS_FILE="$tmp_dir/expired.json" scripts/enforce-security-findings.sh validate >/dev/null 2>&1; then
  echo "expired exception unexpectedly passed" >&2
  exit 1
fi

echo "Security exception enforcement tests passed"
