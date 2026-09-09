#!/bin/sh
set -eu
request=$(cat)
case "$request" in
  *'"operation":"ensure"'*)
    printf '%s\n' "{\"protocol_version\":1,\"status\":\"success\",\"result\":{\"credential\":\"${E2E_PLUGIN_KEY:?}\",\"external_id\":\"e2e-executable\",\"alias\":\"e2e\",\"metadata\":{\"custom_provisioner\":true,\"runtime\":\"executable\"},\"expires_at\":null}}"
    ;;
  *'"operation":"revoke"'*) printf '%s\n' '{"protocol_version":1,"status":"success","result":{"revoked":true}}' ;;
  *) printf '%s\n' '{"protocol_version":1,"status":"error","error":{"code":"invalid_config","message":"unknown operation"}}'; exit 1 ;;
esac
