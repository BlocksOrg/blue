# Gateway coding session attribution

## Purpose

Create a Blue coding session for every gateway-mode harness launch. Blue captures
a fixed set of local metadata, persists it to Postgres, caches it in Redis, and
exchanges the user's existing OAuth JWT for a 30-day inference-only JWT carrying
`blue_coding_session_id`.

No tenant-defined scripts or metadata collectors are supported. Governance-only
launches perform none of this work and install no attribution hooks.

The deployed `services/inference-proxy` is the only inference data-plane proxy.
Blue does not run a second HTTP proxy on the workstation: authenticated session
state gives the central proxy the trusted context it needs without another local
listener or TLS boundary.

## Interfaces and data model

### Control API endpoints

Authenticated with the existing `session:write` scope:

- `POST /coding-sessions` — idempotent create from a client-generated UUID.
- `POST /coding-sessions/{id}/token` — verifies ownership, returns the inference JWT.
- `PATCH /coding-sessions/{id}/agent-session` — attaches a native agent session ID
  and refreshes the built-in environment snapshot.

### Inference JWT

Standard `iss`, `aud`, `sub`, `iat`, `exp`, `jti` claims plus the custom claim
`blue_coding_session_id`. It carries no session metadata and no gateway
credential.

### Metadata shape

Fixed; no tenant extension points.

```json
{
  "blue_coding_session_id": "uuid",
  "blue_policy_revision_id": "revision",
  "session_data": {
    "agent": "codex",
    "agent_session_id": null,
    "cwd": "/workspace/repo",
    "has_git": true,
    "git_root": "/workspace/repo",
    "git_origin": "git@github.com:example/repo.git",
    "git_branch": "feature/ACME-123",
    "git_detached": false,
    "git_commit_hash": "abc123",
    "git_dirty": true
  }
}
```

- Sanitize Git origins by removing URL credentials, query strings, and fragments.
- Represent probe failures with nullable Git fields.
- Never collect commit messages, author identities, email addresses, ticket
  inference, arbitrary environment variables, file contents, or prompts.

### Storage

- Add Postgres tables for coding sessions and native-session associations.
- Keep `agent_session_id` as the current value while retaining previous
  associations for `/clear`, resume, or session switching within one Blue launch.
- Index native associations by organization, user, agent, and native session ID.
  Do **not** make them unique — one resumed native session can appear in multiple
  Blue launches.
- Retain Postgres and Redis records for 30 days.

## Launch, hooks, and token flow

1. Only when the delivered governance configuration contains gateway mode,
   generate a UUIDv4 and capture cwd/Git state with built-in Rust code using
   bounded Git subprocess timeouts.
2. Create the Postgres session through `POST /coding-sessions`; write the same
   non-secret snapshot to Redis.
3. Call `POST /coding-sessions/{id}/token` with the existing user OAuth JWT and
   place the returned inference JWT into the launch-scoped gateway configuration.
4. Let `gh-config`, as the only harness-config writer, install the inference token
   and the Blue-owned attribution hook before launching the native CLI.
5. Export `BLUE_CODING_SESSION_ID` for the hook process.
6. At the earliest native session event, the hook invokes a dedicated hidden Blue
   CLI command with the vendor payload. Blue validates the payload, recaptures
   cwd/Git state, and PATCHes the session.
7. The Control API commits the update to Postgres, updates Redis, and publishes an
   invalidation event so inference-proxy instances evict that UUID from memory.

### Harness adapters

- **Codex** — managed `SessionStart`, whose current payload includes `session_id`.
  Retain version-aware compatibility; hook behavior varies by Codex generation.
- **Claude** — managed `SessionStart` for startup, resume, and clear events.
- **Kimi** — managed `SessionStart` for startup and resume.
- **OpenCode** — Blue-owned plugin listening for the first top-level
  `session.created` (or equivalent session event containing an ID). Ignore
  child/subagent sessions; update again if the top-level session changes.

These hooks are independent of raw-session upload policy and are removed when the
client downgrades to governance-only mode.

### Failure behavior

- Initial persistence or token minting failure **blocks** the gateway launch.
- Native-session hook updates are best-effort: retry three times with short
  bounded backoff, emit diagnostics, and leave the Blue session usable with a null
  native ID.

## Inference proxy and caching

- The Control API signs inference JWTs with a dedicated asymmetric key and
  publishes a Blue inference JWKS endpoint. Support an active signing key plus
  additional verification keys for rotation.
- The inference proxy validates signature, issuer, audience, expiry, subject, and
  UUID claim before using the token.
- Cache metadata in process by Blue session UUID. On a miss, read Redis directly.
- If Redis is missing or unavailable, call an authenticated internal Control API
  fallback that reads Postgres and repopulates Redis.
- Resolve the verified JWT subject to the managed gateway credential through the
  existing M2M Control API channel. Never place upstream credentials in
  coding-session Redis records.
- Continue accepting legacy opaque pseudotokens during rollout; new clients always
  require session JWT setup in gateway mode.
- Subscribe inference-proxy instances to explicit Redis invalidation messages. A
  short local-cache TTL remains the safety net for missed messages.

### Legacy pseudotoken retirement

- Add `issued_at`, `expires_at`, `last_used_at`, and revocation reason to legacy
  pseudotoken records. Newly issued legacy tokens expire after 30 days and rotate
  on credential, role, or access changes.
- During one advertised compatibility window, accept an unexpired legacy token
  but record a structured migration metric. Do not renew it when a client supports
  coding-session JWTs.
- After the minimum supported client version requires session JWTs, stop issuing
  legacy tokens, revoke remaining mappings at the end of their lifetime, and then
  remove the resolver path in a separate contract-breaking release.
- Never log a token or use token text as an observability identifier.

### Header handling

Strip caller-supplied `X-Blue-*` and legacy `X-Harness-*` attribution headers,
then inject trusted headers:

- `X-Blue-Coding-Session-Id`
- `X-Blue-Policy-Revision-Id`
- `X-Blue-Agent`
- `X-Blue-Agent-Session-Id` (when known)
- `X-Blue-Cwd`
- `X-Blue-Has-Git`
- `X-Blue-Git-Root`
- `X-Blue-Git-Origin`
- `X-Blue-Git-Branch`
- `X-Blue-Git-Detached`
- `X-Blue-Git-Commit`
- `X-Blue-Git-Dirty`

Add Blue and native session IDs to gateway request logs for correlation.

Header stripping happens before the forwarded request is constructed and covers
case-insensitive names, repeated values, alternate underscore spellings rejected
by the HTTP stack, and all legacy names. Only values loaded after JWT validation
from the server-owned session record may be injected. The proxy never falls back
to an incoming attribution value when metadata is absent.

## Remove the obsolete workstation proxy

Delete the unused `gh-proxy` workspace crate, its `gh-cli` dependency, and the
`serve_ephemeral`/local header-gathering implementation after moving any reusable
Git-origin sanitization tests into the coding-session metadata module. This
removal does not affect `services/inference-proxy`, gateway URLs, or native-agent
routing: current production code has no caller of the local proxy.

Remove comments and architecture text that promise a future workstation
forwarder. Add a dependency check that fails if the CLI regains a direct
HTTP-forwarding responsibility without a new reviewed design.

## Deployment

- Add authenticated/TLS Redis settings to the Control API and inference proxy,
  wire local Compose to Redis, and add Helm and AWS managed-Redis configuration.
- Require persistent signing-key configuration whenever gateway mode is enabled;
  replicas must share the same signing keys.
- Treat Postgres as authoritative. A Redis write or publication failure does not
  invalidate a committed session, because the proxy can fall back through the
  Control API.

## Tests

### Unit

Git capture, origin sanitization, detached/non-repository states, subprocess
timeouts, endpoint ownership, idempotency, JWT validation, hook payload parsing,
and header-spoof removal.

### Integration

Redis write-through, memory invalidation, Postgres fallback, key rotation, record
expiry, multiple native IDs per Blue launch, and native-session reuse across
launches.

### End-to-end

- Codex, Claude, Kimi, and OpenCode against the fake upstream, asserting the
  initial headers and the later native-session-ID update.
- Gateway setup failures block launch; hook failures do not.
- Legacy pseudotokens remain accepted during rollout.
- Caller-supplied attribution headers are discarded and never appear in request
  logs or at the fake upstream; trusted session values replace them.
- Governance-only launches create no session, mint no inference JWT, connect to no
  Redis/proxy path, and install no attribution hook.

## Acceptance criteria

- Every new-client gateway request has a validated session JWT and server-owned
  attribution, or is rejected before upstream forwarding.
- Missing optional metadata produces absent headers, never caller-controlled
  fallback values.
- Legacy pseudotokens have bounded lifetime and a measured removal path.
- The workstation `gh-proxy` crate and dependencies are absent, while all central
  inference-proxy and governed-launch journeys remain green.

### Verification matrix

```text
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
dashboard typecheck and production build
OpenAPI contract tests
deployment journeys (tests/e2e)
```
