# AGENTS.md

Working notes for anyone — human or AI agent — writing code here. Full process
lives in [CONTRIBUTING.md](CONTRIBUTING.md); this is the quick reference.

## What this is

Blue — self-hosted governance for coding-agent CLIs (Codex, Claude, Kimi,
OpenCode). A Rust Cargo workspace: one `blue` client binary plus reference
backend services.

### Supported harnesses & upstream CLIs

The governed CLIs are declared in `gh_common::harness_catalog!`
(`crates/gh-common/src/harness.rs`). Each maps to the upstream repository it
tracks:

| Harness | Binary | Install package | Upstream repo |
|---|---|---|---|
| Codex | `codex` | `@openai/codex` | [openai/codex](https://github.com/openai/codex) |
| Claude | `claude` | `@anthropic-ai/claude-code` | [anthropics/claude-code](https://github.com/anthropics/claude-code) |
| Kimi | `kimi` | `@moonshot-ai/kimi-code` | [MoonshotAI/kimi-code](https://github.com/MoonshotAI/kimi-code) |
| OpenCode | `opencode` | `opencode-ai` | [sst/opencode](https://github.com/sst/opencode) |

## Repo map

```
crates/      Blue CLI, policy client, config adapters, launcher, gateway, agent daemon, telemetry
services/    control-api (serves governance config) + inference-proxy (gateway mode)
apps/        dashboard (Next.js) + docs (Mintlify)
deploy/      compose, Helm, OpenTofu, example configs, versioned OpenAPI contract
tests/e2e/   hermetic deploy + cross-service journeys
scratch/     git-ignored; throwaway local files go here, never at the repo root
```

## Build & check

```bash
cargo build --workspace
cargo test  --workspace
cargo fmt --all
cargo clippy --all-targets
```

Keep all four green before you push. Run `scripts/setup.sh` once per clone to
install the git hooks that run the cheap subset (fmt, Clippy, TS type-checks) on
staged files automatically.

## Branch naming

`<type>/<short-slug>`, where `<type>` matches the change:
`feat`, `fix`, `docs`, `refactor`, `test`, `chore`.

Examples: `feat/opencode-gateway`, `fix/control-api-stale-token`,
`docs/contributing`.

## Commit & PR titles

[Conventional Commits](https://www.conventionalcommits.org): `type(scope): summary`.

- **Types:** `feat` `fix` `docs` `refactor` `test` `chore`
- **Scopes (optional):** `cli` `control-api` `proxy` `gateway` `config`
  `harness` `dashboard` `docs` `deploy`

Every PR references an issue (`Fixes #123`). Keep descriptions short and in your
own words — no AI-generated walls of text.

All PRs must be created in draft mode.

### E2E while in draft

Both end-to-end workflows skip while the PR is a draft — they build the
production image and boot a full hermetic stack, so drafts don't pay for that on
every push:

| Workflow | Draft-gated job | Runs on |
|---|---|---|
| `e2e.yml` | `build`, `smoke` | non-draft PRs |
| `e2e.yml` | `full` | main, nightly cron, dispatch (never on PRs) |
| `e2e-slim.yml` | `build`, `e2e-slim-governance` | non-draft PRs |
| `e2e-slim.yml` | `e2e-slim-gateway` | main, dispatch (never on PRs — real inference costs money) |

Marking the PR ready for review triggers them (`ready_for_review` is in both
workflows' `pull_request` type list). To get a run without leaving draft,
dispatch the workflow against the branch from the Actions tab — `workflow_dispatch`
bypasses the draft gate:

```bash
gh workflow run e2e.yml      --ref <branch>
gh workflow run e2e-slim.yml --ref <branch>
```

`ci.yml` (fmt, Clippy, tests, builds, packaging) is unaffected and still runs on
drafts.

## Conventions that bite

- **`gh-config` is the only writer of agent config files.** Never write agent
  files from anywhere else.
- Write cross-cutting state to predictable files, atomically (tempfile + rename;
  `0600` for secrets).
- Never degrade the native CLI passthrough (argv, TUI, signals, resize, exit
  code).
- The governance-only path takes **no** new hard dependency on Redis / LiteLLM /
  a database.
- The **server** decides the operating mode; the client may only *downgrade* to
  governance-only, never enable gateway mode itself.

## Adding support

- **New harness:** add one entry to `gh_common::harness_catalog!` and an initial
  `gh-config/src/adapters/<harness>/` family implementation plus its initial
  `<version>/VersionSpec`. The family defines complete atomic operation sets;
  each version spec selects one operation set and explicitly declares optional
  capabilities. Add golden plans and boundary fixtures with it; see
  [Harness adapter architecture](apps/docs/next/development/harness-adapter-architecture.mdx).
- **Package compatibility:** use adapter-level `introduced`/`before` for the
  harness releases where a package is available. Use nested `variants` only for
  layout changes inside that range.
- **New harness version boundary:** close the previous interval, add a new
  version spec, and reuse the family operation set for unchanged behavior. If
  an operation changes, add a named operation set in the family module and
  select it from the new spec. Retain previous specs and operation sets. Add
  golden plans immediately below and at the boundary. Every generation also
  declares an exclusive `verified_before` release ceiling; advance the current
  ceiling when the E2E harness lock is updated.
- **New gateway:** extend the gateway service contract and implement its common
  wiring in `gh-gateway`; keep harness-specific auth placement inside each
  `HarnessImplementation`. Do not add shared harness dispatch to `gh-config`.
  See
  [Gateway adapter architecture](apps/docs/next/development/gateway-adapter-architecture.mdx).
