# Control-plane telemetry

## Purpose

Replace the currently inert client-to-external-sink model with an opt-in,
authenticated, tenant-scoped Control API event channel. Until this plan ships,
public docs and examples must not describe telemetry as operational.

Telemetry is metadata-only operational reporting. It is separate from gateway
request attribution and native transcript capture.

## Compatibility and configuration

In the next governance contract version, define:

```yaml
telemetry:
  enabled: true
```

Absence or `enabled: false` disables collection and installs no background work.
Continue accepting the legacy `telemetry.sink_url` field for one compatibility
window, but do not send data to it. The Control API rejects new publications that
set `sink_url`, returns a targeted migration error, and omits it from newly
exported configuration. Remove the field in the following contract version.

Before implementation, remove external-sink examples and mark the feature as
planned in user-facing docs. Restore operational documentation only when the
end-to-end acceptance journey passes.

## Event interface

Add authenticated `POST /telemetry/run-events` with a dedicated
`telemetry:write` user scope. The client generates a UUID `event_id` and UUID
`run_id`. Requests are idempotent on organization plus `event_id`.

Use two event kinds with a versioned body:

```json
{
  "version": 1,
  "event_id": "uuid",
  "run_id": "uuid",
  "kind": "started | finished",
  "harness": "codex | claude | kimi | opencode",
  "policy_revision": "revision",
  "gateway": false,
  "occurred_at": "RFC3339 UTC",
  "outcome": "success | agent_error | governance_error | cancelled | unknown",
  "exit_code": 0
}
```

`outcome` and `exit_code` are absent on `started`; `exit_code` may be absent on a
cancelled or unknown finish. The server derives organization and user from the
token. It rejects unknown fields, timestamps more than 24 hours from server time,
unsupported harnesses, and a finish whose immutable fields disagree with its
start. Duplicate identical events succeed without duplicating data.

Never add cwd, repository, Git metadata, native session IDs, model prompts,
responses, tool data, environment variables, arguments, filenames, or arbitrary
labels to this interface. New fields require a privacy review and contract
version.

## Client delivery and storage

Emit `started` after policy reconciliation succeeds and immediately before native
launch. Emit `finished` after terminal restoration with the classified outcome.
Telemetry failure never blocks or changes a governed run.

Send with the current user OAuth token and a five-second timeout. On transient
failure, store events in an owner-only local JSONL spool, capped at 1 MiB or 1,000
events and 24 hours, whichever is reached first. Retry oldest-first during the
next authenticated command with bounded backoff; discard permanent 4xx failures
with a diagnostic. Corrupt records are quarantined without exposing their body.

Store a normalized run row plus event timestamps in Postgres, scoped by
organization and user. Default retention is 30 days and uses the existing
singleton worker for bounded batch deletion. Add an admin-only paginated query
endpoint with filters for date, user, harness, revision, gateway use, and outcome.
Do not add a dashboard page in the first delivery; verify the query through the
API and defer visualization until operators identify useful views.

## Tests

- Contract tests cover scopes, tenant isolation, validation, idempotency,
  start/finish consistency, pagination, filters, and retention.
- Client tests cover opt-in/out, event timing, outcome classification, timeout,
  spool permissions and limits, retry ordering, expiry, corrupt records, and
  permanent rejection.
- Privacy tests assert forbidden metadata cannot be serialized or accepted.
- End-to-end tests prove a successful run and each failure class appear once,
  telemetry outage never blocks a launch, and disabled telemetry creates no
  request or spool.

## Acceptance criteria

- No arbitrary sink URL or unauthenticated telemetry delivery remains.
- Events contain only the fixed metadata contract and cannot cross tenant/user
  authorization boundaries.
- Runs remain behaviorally identical when ingestion is unavailable.
- Public docs describe telemetry as available only after all contract, retention,
  privacy, and end-to-end tests pass.

