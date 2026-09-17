#!/bin/sh
set -eu
request=$(cat)
state_dir=${E2E_PROVISIONER_STATE_DIR:-/e2e-provisioner-state}
mode=${E2E_PROVISIONER_MODE:-success}
if [ -r "$state_dir/mode" ]; then
  mode=$(sed -n '1p' "$state_dir/mode")
fi
if [ -d "$state_dir" ]; then
  printf '%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$request" >> "$state_dir/invocations.jsonl"
fi

case "$mode" in
  success) ;;
  timeout) sleep 30 ;;
  malformed) printf '%s\n' '{not-json'; exit 0 ;;
  incomplete)
    printf '%s\n' '{"protocol_version":1,"status":"success","result":{"external_id":"e2e-incomplete","alias":"e2e","metadata":{},"expires_at":null}}'
    exit 0
    ;;
  unavailable)
    printf '%s\n' '{"protocol_version":1,"status":"error","error":{"code":"temporary_unavailable","message":"e2e provisioner unavailable"}}'
    exit 75
    ;;
  account-missing)
    printf '%s\n' '{"protocol_version":1,"status":"error","error":{"code":"account_missing","message":"member has no upstream account"}}'
    exit 4
    ;;
  protocol-mismatch)
    printf '%s\n' '{"protocol_version":2,"status":"success","result":{"credential":"must-not-persist","external_id":"e2e-mismatch","alias":"e2e","metadata":{},"expires_at":null}}'
    exit 0
    ;;
  nonzero) exit 23 ;;
  revocation-failure)
    case "$request" in
      *'"operation":"revoke"'*)
        printf '%s\n' '{"protocol_version":1,"status":"error","error":{"code":"temporary_unavailable","message":"e2e revocation failure"}}'
        exit 75
        ;;
    esac
    ;;
  *)
    printf '%s\n' '{"protocol_version":1,"status":"error","error":{"code":"invalid_config","message":"unknown e2e fixture mode"}}'
    exit 64
    ;;
esac

case "$request" in
  *'"operation":"ensure"'*)
    printf '%s\n' "{\"protocol_version\":1,\"status\":\"success\",\"result\":{\"credential\":\"${E2E_PLUGIN_KEY:?}\",\"external_id\":\"e2e-executable\",\"alias\":\"e2e\",\"metadata\":{\"custom_provisioner\":true,\"runtime\":\"executable\"},\"expires_at\":null}}"
    ;;
  *'"operation":"revoke"'*) printf '%s\n' '{"protocol_version":1,"status":"success","result":{"revoked":true}}' ;;
  *) printf '%s\n' '{"protocol_version":1,"status":"error","error":{"code":"invalid_config","message":"unknown operation"}}'; exit 1 ;;
esac
