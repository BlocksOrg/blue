#!/usr/bin/env python3
import json
import sys

envelope = json.load(sys.stdin)
if envelope.get("protocol_version") != 1:
    json.dump({"protocol_version": 1, "status": "error", "error": {"code": "invalid_config", "message": "unsupported protocol"}}, sys.stdout)
    sys.exit(1)
request = envelope["request"]
if envelope["operation"] == "ensure":
    identity = request["identity"]
    result = {"credential": "replace-with-created-key", "external_id": identity["id"], "alias": identity["email"], "metadata": {"runtime": "python"}, "expires_at": None}
    json.dump({"protocol_version": 1, "status": "success", "result": result}, sys.stdout)
elif envelope["operation"] == "revoke":
    json.dump({"protocol_version": 1, "status": "success", "result": {"revoked": True}}, sys.stdout)
else:
    json.dump({"protocol_version": 1, "status": "error", "error": {"code": "invalid_config", "message": "unsupported operation"}}, sys.stdout)
    sys.exit(1)
