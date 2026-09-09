# AGENTS.md — `tests/e2e-slim`

Working notes for anyone (human or AI) touching this suite.

**Keep [`README.md`](README.md) current — in the same change.** It is the single
source of truth for what this suite covers, and CI publishes it verbatim into the
GitHub Actions run summary (`.github/workflows/e2e-slim.yml`). If you add, remove,
or rename a test, change a fixture, alter the stack topology
(`docker-compose*.yml` / `run.sh`), or touch the env knobs, update `README.md` in
the **same** commit — especially the **What's tested** table and the **Real vs.
faked** note. A behavior change here without a matching README update is
incomplete.

Also keep the auth constants in `src/lib.rs` (`KID` / `ISSUER` / `AUDIENCE`)
byte-identical to the `HARNESS_AUTH_*` values in `docker-compose.yml`. On the
gateway path the same values also flow to the token issuer as `E2E_TOKEN_ISSUER` /
`E2E_TOKEN_AUDIENCE` / `E2E_TOKEN_CLIENT_ID` in `docker-compose.gateway.yml`
(the inference-proxy's M2M service token must match control-api's `iss`/`aud` and
`internal_allowed_client_id`) — keep those in sync too.

See the repo-root `AGENTS.md`/`CLAUDE.md` for workspace-wide conventions.
