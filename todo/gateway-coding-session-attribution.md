# Gateway coding-session attribution

## Purpose

Add trustworthy per-launch attribution after
[gateway inference authentication](gateway-inference-authentication.md) is
complete. This workstream starts from an authenticated, session-bound inference
JWT and adds only coding-session identity and server-owned metadata. It does not
redesign gateway authentication or credential resolution.

## Coding-session lifecycle

- Create one Blue coding-session UUID for every gateway-mode harness launch.
- Persist the session and native-session associations in Postgres, with a
  non-secret Redis write-through cache and 30-day retention.
- Capture cwd and bounded, sanitized Git state using built-in Rust logic. Never
  run tenant-defined collectors or capture prompts, file contents, commit
  messages, author identity, arbitrary environment variables, or inferred
  tickets.
- Install Blue-owned native lifecycle hooks through `gh-config` for Codex,
  Claude, Kimi, and OpenCode. Hooks attach or update the native agent-session ID;
  initial creation failures block launch while hook updates retry and fail soft.
- Mint a launch-scoped inference JWT that retains the authenticated gateway
  claims and adds only `blue_coding_session_id`.

## Trusted proxy attribution

- Validate the launch-scoped inference JWT, load session metadata by Blue
  coding-session UUID, and fall back from Redis to the M2M-authenticated Control
  API/Postgres path when required.
- Remove all caller-supplied `X-Blue-*` and legacy `X-Harness-*` attribution
  headers before constructing the upstream request.
- Inject trusted Blue session, policy revision, agent, native session, cwd, and
  sanitized Git fields only from server-owned records. Missing metadata yields
  absent headers, never caller-controlled fallback values.
- Add Blue and native session IDs to gateway request logs for correlation and
  publish invalidations when session metadata changes.

## Workstation cleanup

Move reusable Git-origin sanitization into the coding-session metadata module,
then delete the unused workstation `gh-proxy` crate and its CLI dependency.
This does not affect the central `services/inference-proxy` data plane.

## Deployment and tests

- Add authenticated/TLS Redis settings to Control API and inference proxy,
  Compose, Helm, and AWS examples. Postgres remains authoritative.
- Unit-test Git capture, origin sanitization, hook payload parsing, ownership,
  JWT coding-session claims, and spoofed-header removal.
- Integration-test Redis write-through/invalidation, Postgres fallback, expiry,
  multiple native IDs, and native-session reuse across launches.
- Run end-to-end journeys for all supported harnesses, gateway setup and hook
  failures, spoof removal/trusted replacement, request-log correlation, and the
  absence of all coding-session work in governance-only mode.

## Acceptance criteria

- Every gateway request is already authenticated by the prerequisite workstream
  and carries server-owned coding-session attribution or is rejected.
- Caller-controlled attribution never reaches upstreams or request logs.
- Governance-only launches create no coding session, Redis record, inference
  claim, or attribution hook.
- The obsolete workstation proxy is absent and central inference journeys remain
  green.
