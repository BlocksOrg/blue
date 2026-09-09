#!/bin/sh
set -eu
request=$(cat)
case "$request" in
  *'"operation":"ensure"'*) printf '%s\n' '{"protocol_version":1,"status":"success","result":{"credential":"replace-with-created-key","external_id":"example","alias":"example","metadata":{"runtime":"shell"},"expires_at":null}}' ;;
  *'"operation":"revoke"'*) printf '%s\n' '{"protocol_version":1,"status":"success","result":{"revoked":true}}' ;;
  *) printf '%s\n' '{"protocol_version":1,"status":"error","error":{"code":"invalid_config","message":"unsupported request"}}'; exit 1 ;;
esac
