# `e2e-slim` — slim, gateway-disabled end-to-end suite

A lean end-to-end suite that exercises the **governance-only** path of Blue
against a **control-api-only** stack. No dashboard, inference-proxy, LiteLLM,
Redis, or gateway. Tests are Rust (`cargo-nextest`) and run **on the host**
against services in Docker Compose.

It complements `tests/e2e` (which stands up the full deployment). This suite is
leaner by **topology** — a control-api-only stack with no dashboard, gateway, or
browser/Playwright layer — and it reuses the single authoritative deployment
image built once by `images.yml` (the same one the full suite loads) rather than
compiling an image of its own. control-api ships in that image, so the
governance-only stack just runs it with `command: ["control-api"]`.

Stateful browser/PTY behaviors, concurrent governance revision updates,
executable-provisioner invocation reuse and failure controls, and deterministic
proxy/upstream failures remain in the full `tests/e2e` suite. Keeping those
cases there preserves this suite's control-api-only topology and secret-free PR
tier.

## What's tested

Every test runs against a **live** control-api + Postgres + MinIO (real service
binary, real database, real object storage). The MinIO server and client use
pinned release images from the project's Quay registry so CI does not depend on
mutable or retired Docker Hub tags.

| Test | What it exercises | Needs a real agent CLI? | Needs the gateway path? |
| --- | --- | --- | --- |
| `bootstrap_login` | Mint token → control-api verifies it via JWKS (RS256 signature, issuer, audience, expiry) → `blue login` short-circuits → `GET /auth/me` returns the admin identity. | no | no |
| `claude_config` | `blue apply` writes Claude's governed model, the `e2e-remote` MCP server, and the managed `example` skill into its managed config. | yes (`claude`) | no |
| `all_agents_config` | The **same** model + MCP + skill assertions for `codex` / `kimi` / `opencode`, so all four agents are covered identically. | yes (per agent) | no |
| `session_upload` | `blue session-upload` per harness → detached worker presigns → PUTs to MinIO → completes; the list shows all 4, and one artifact is downloaded and byte-compared to the upload. | no | no |
| `agent_matrix_config` | **Tier A (version matrix).** The same model + MCP + skill assertions as `all_agents_config`, but per `(agent, version)` cell across multiple CLI versions (not just the pin). No inference. Self-skips unless the matrix is enabled. | yes (per cell) | no |
| `agent_certification` | **Tier B (version matrix).** Per `(agent, version)` cell, one uniform body: `blue run <agent>` launches the real agent through the real gateway, forces the managed `blue_certify` MCP tool (asserts `mcp-started` + `mcp-called` markers and `BLUE_MCP_OK`), and asserts a LiteLLM spend log recorded the governed model alias with a swapped virtual key rather than the client inference JWT. With the matrix off it certifies just the pin per agent (today's coverage). | yes (per cell) | **yes** (opt-in) |

**Real vs. faked.** Real: control-api, Postgres, MinIO (real presigned PUT/GET),
the `blue` CLI, the agent CLIs (really installed, version-probed for eligibility),
and — on the gateway path — a real inference-proxy and a real LiteLLM fed by a
real OpenRouter key, i.e. **real model inference, no provider faking**. Faked:
only the token **issuer** — a sidecar mints RS256 JWTs (user tokens and the
inference-proxy's M2M service token) with a committed **test-only** key, though
control-api's *verification* of them is the real production path — and the JWKS
endpoint that serves that public test key. There is no dashboard.

On the gateway path, the user JWT carries a `sid` and the harness creates the
matching Better Auth user/session rows. The control API uses the same test-only
RSA fixture as its dedicated gateway signing ring, publishes `/gateway/jwks`,
and mints the session-bound inference JWT consumed by the proxy.

## Two paths

- **Governance-only (default, secret-free).** control-api-only stack (the full
  deployment image run as `control-api`); runs `bootstrap_login`, `claude_config`,
  `all_agents_config`, `session_upload`, and — with `E2E_SLIM_MATRIX=1` (CI sets
  it) — the Tier A `agent_matrix_config` grid. The `agent_certification` tests
  self-skip. This is what runs on every PR.
- **Gateway (opt-in, secret-gated on `OPENROUTER_API_KEY`).** `run.sh` additionally
  brings up a real LiteLLM (fed by OpenRouter) + inference-proxy
  (`docker-compose.gateway.yml`, using the **full** deployment image, which ships
  both `control-api` and `inference-proxy`), points control-api at
  `gateway-slim.yaml`, and runs `agent_certification` — the full Tier B version
  grid with `E2E_SLIM_MATRIX=1`, else just the pin per agent. Internal
  control-api↔proxy transport is plain HTTP (`insecure-http`) — no mTLS in this
  suite; the inference-proxy's M2M service token is minted by the JWKS sidecar's
  `/oauth2/token`. LiteLLM gets its own `litellm` database on the shared Postgres.

## Version matrix

By default the managed-config and certification tests exercise the **single**
pinned version in `agents.lock.json`. Setting `E2E_SLIM_MATRIX=1` (both CI jobs
do) additionally exercises each supported harness across **multiple blessed CLI
versions** — one generated `cargo-nextest` case per `(agent, version)` cell.

- **Cells** = the current lock pin (the newest/blessed version) **plus** the
  historical samples in [`agents.matrix.json`](agents.matrix.json). That file is
  slim-only (deliberately **not** the symlinked lock) so the full `tests/e2e`
  suite is untouched, and it holds only *extra* samples — the pin is added
  automatically and must not be re-listed (a guard test enforces this). Every
  sample is below its adapter's `verified_before` ceiling, so **no cell is
  expected to skip**; above-ceiling / calendar-current builds are excluded.
- **Cases are generated** by [`build.rs`](build.rs) from the two JSON files, so
  the list stays data-driven (no hand-maintained tuples, no drift).
- **Binary selection is by `PATH`.** With the matrix on, `run.sh` installs each
  cell into its own npm prefix and records the bin dirs in a manifest
  (`E2E_SLIM_AGENT_MATRIX`); each case prepends its cell's bin dir so `blue`'s
  version probe selects that exact build.

Two tiers, run by the two CI jobs:

- **Tier A — config matrix (free, governance path, every PR).**
  `agent_matrix_config`: `blue apply` + config assertions only, no inference.
  Runs on every PR — including fork PRs where the gateway secret (and thus
  Tier B) is absent — so there is per-version coverage even without the paid path.
- **Tier B — certification matrix (paid, gateway path).** `agent_certification`:
  the **full** grid, a real OpenRouter round-trip per cell (cost accepted). The
  lock-pin cell per agent preserves today's certification coverage; with the
  matrix off, only those pin cells certify.

> **Old versions fail hard, not skip.** Eligibility gates on version *range*, not
> feature support, so an in-range build that predates a certified feature (managed
> MCP tool / skill / session-upload) fails red rather than skipping. Sample floors
> are chosen at versions that support the certified features.

## Run it

```bash
tests/e2e-slim/run.sh
```

The orchestrator builds (or, in prebuilt mode, reuses) the deployment image,
brings up the stack (`--wait`), builds the `blue` binary, and runs
`cargo nextest run --manifest-path tests/e2e-slim/Cargo.toml`. The host-side
fixture setup supports both GNU/Linux and macOS command-line tools. Useful env:

| Variable | Effect |
| --- | --- |
| `OPENROUTER_API_KEY` | Enables the gateway path: brings up LiteLLM + inference-proxy and runs `agent_certification`. Unset → governance-only. |
| `BLUE_E2E_SLIM_PREBUILT=1` | Skip building; use the deployment image already `docker load`ed under `blue-e2e-base:local` (how CI runs it, after `images.yml`). Locally it builds the image instead. |
| `E2E_SLIM_INSTALL_AGENTS=1` | npm-install the pinned agent CLIs so the managed-config + certification tests run (otherwise they self-skip). |
| `E2E_SLIM_MATRIX=1` | Enable the [version matrix](#version-matrix): install every `(agent, version)` cell into its own prefix and run the generated per-version cases (Tier A config, and Tier B certs on the gateway path). Requires `E2E_SLIM_INSTALL_AGENTS=1`. Unset → single-pin behavior, unchanged. |
| `E2E_SLIM_PRUNE=1` | `docker image prune` + `docker builder prune` on teardown. |
| `E2E_SLIM_BLUE_BIN` | Path to a prebuilt `blue` binary (defaults to `target/debug/blue`). |

Without the stack env (`E2E_SLIM_CONTROL_API_URL`), every test early-returns, so
the suite stays green with no services up.

### Not a root workspace member

This crate is its own Cargo workspace, deliberately kept out of the root
workspace, so its test-only dependencies never enter the deployment image's
`cargo chef` recipe (a dep change here would otherwise force the control-api
image to recompile from scratch). Build/lint/test it via its own manifest, e.g.
`cargo nextest run --manifest-path tests/e2e-slim/Cargo.toml`; the root
`cargo <build|test|clippy> --workspace` does not cover it.

## How auth works without a dashboard

control-api verifies **user** tokens exactly as in production: it fetches the
JWKS from `HARNESS_AUTH_JWKS_URL`, checks the RS256 signature by `kid`, and
enforces issuer / audience / expiry. The only thing missing here is the thing
that *issues* those tokens (the dashboard's Better Auth).

So the suite self-serves auth:

- A committed **test-only** RSA keypair lives in `fixtures/jwks/`
  (`jwt-signing-key.pem` private, `jwks.json` public). See that folder's README
  — the keys grant no access to anything real and must never be reused.
- A tiny `jwks-server.mjs` sidecar serves the public JWK set in-network at
  `HARNESS_AUTH_JWKS_URL`.
- The Rust harness (`src/lib.rs`) mints an RS256 admin JWT with the private key,
  setting `iss`/`aud`/`iat`/`exp` and the scopes the user routes need. The org id
  in the token is read from Postgres (bootstrap assigns a random UUID — never
  hardcode it).

The constants `KID` / `ISSUER` / `AUDIENCE` in `src/lib.rs` must stay
**byte-identical** to the `HARNESS_AUTH_*` values in `docker-compose.yml`.

## Pitfall: do **not** call `blue setup`

`blue setup` needs a TTY and drives the interactive OIDC/device login against the
dashboard — neither exists here. Instead the harness writes `blue.toml` +
`session.json` **directly** (byte-for-byte what `setup`/`login` would persist:
`[service].url`, `[identity]` token, `[mode].force_governance_only`, and a valid
non-expired session carrying the JWT). Then it only runs `blue login`, which
short-circuits to *"Already logged in"*. **Do not "fix" this by calling
`setup`** — it will hang/fail.

## Endpoint crossing (why two MinIO URLs)

control-api's own S3 client talks to `minio:9000` (in-network), but the
**presigned** URLs it hands back must be reachable from the host test runner, so
`HARNESS_BLOB_PUBLIC_ENDPOINT` and `governance.session_upload.presign_url` point
at `127.0.0.1`. Path-style addressing keeps the signatures valid across the
hostname change.

## Keeping agent pins in sync

`agents.lock.json` is a symlink to `../e2e/agents.lock.json` so the two suites
never drift on agent versions. The extra historical samples for the
[version matrix](#version-matrix) live in `agents.matrix.json` (slim-only, not
symlinked); it must list only versions **other** than the lock pin — the
`matrix_excludes_lock_pin` guard test fails if it re-lists the pin.

## Native Windows and macOS clients

[`../e2e-native`](../e2e-native/README.md) runs these **same scenario bodies and
version matrix** against a separate disposable Linux backend per OS/suite.
Unix still uses temporary HOME/XDG roots; Windows uses real Known Folders in a
fresh disposable account, serial tests, an exclusive reservation, and descendant
cleanup before removing only test-owned application state.

| Additional native evidence | Assertion |
| --- | --- |
| Shared config matrix | Exact pinned/historical agent, model, MCP and managed skill |
| Gateway invocation | Fresh per-profile MCP markers and invocation nonce in uploaded bundle |
| Native isolation | Windows account preflight, session ACL, sequential cleanup, one active Home and child completion |
| Client/backend connection | Package object digest and DB query through loopback SSM tunnels |
| Required coverage | Missing endpoints, agents and matrix cells fail CI; expected/executed report |

New knobs: `E2E_SLIM_REQUIRED=1` makes missing prerequisites failures;
`E2E_SLIM_REPORT_DIR` receives completed cell evidence;
`E2E_SLIM_DISPOSABLE_ACCOUNT=1` acknowledges the required fresh Windows account.
`E2E_SLIM_MARKER_DIR` is set per Home for the managed MCP child. Native Blue config,
data and cache accessors replace Unix path assumptions. Matrix generation reads
`../e2e/agents.lock.json` directly, including on Windows without Git symlinks.
The Linux `run.sh` delegates byte-identical legacy fixture decoding to Node.

**Real vs. faked:** native governance still seeds a signed session and synthetic
transcripts; it does not automate browser login. Gateway runs actual installed
agents through real inference. The full Linux suite retains browser/device
approval, SCIM, mTLS and container-topology coverage. Windows ARM64 and full
upstream TUI certification are not implied by these tests. Native parity requires
successful native gateway results, not just a passing fixture suite or workflow.

Native package fixtures use a seeded managed artifact and the authenticated download
API because public package URLs reject HTTP/loopback. Production access rules
are unchanged; the advertised MinIO URL is signed before the client receives it.
