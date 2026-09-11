# Contributing to Blue

Thanks for helping build Blue. The changes we merge most often:

- Bug fixes
- Support for a new gateway
- Support for a new agent / harness
- Missing standard behavior
- Documentation improvements

Any **UI or core product feature** must go through a design review with the core
team **before** implementation.

Not sure a change will land? Ask a maintainer, or browse issues labeled
`bug`, `good-first-issue`, or `performance`.

## Developing Blue

Blue is a Rust (Cargo) workspace pinned by `rust-toolchain.toml`.

```bash
cargo build --workspace         # client + reference services
cargo test  --workspace         # unit tests
cargo fmt --all                 # format
cargo clippy --all-targets      # lint
```

Run the one-time setup so formatting, Clippy, and the TypeScript type-checks run
on your staged files before each commit (mirrors CI):

```bash
scripts/setup.sh   # installs the lefthook git hooks; safe to re-run
```

No `lefthook` on your machine? The script prints install options (`brew` / `npm`
/ `go`). Hooks are opt-in per clone and CI enforces the same checks regardless.

The Control API uses SQLx's compile-time checked query macros. After changing a
query or a database migration, install `sqlx-cli` 0.8.6 and regenerate the
checked-in offline metadata against an isolated PostgreSQL 16 instance:

```bash
cargo install sqlx-cli --version 0.8.6 --locked \
  --no-default-features --features rustls,postgres
scripts/prepare-sqlx.sh
```

The helper uses a temporary, tmpfs-backed database on port 55432 and removes it
when preparation finishes. Normal Cargo builds use the committed `.sqlx`
metadata and do not require a running database. Static queries must use
`query!`, `query_as!`, or `query_scalar!`; `QueryBuilder` is reserved for SQL
whose number of bound values is genuinely dynamic.

Run it locally with the reference backend:

```bash
cd deploy && docker compose up --build   # dashboard :3000, docs :3001, Control API :8080
```

Before changing cross-service behavior, run the hermetic E2E journeys:

```bash
tests/e2e/run.sh smoke   # PR-gating deploy + CLI journey
tests/e2e/run.sh full    # full API / dashboard / gateway / SCIM / harness matrix
```

In CI, both end-to-end workflows (**End-to-end** and **End-to-end (slim)**) skip
while a PR is a draft — they build the production image and boot a full hermetic
stack, so drafts don't pay for that on every push. Marking the PR ready for
review triggers them. To get a run without leaving draft, dispatch the workflow
against your branch from the Actions tab (`gh workflow run e2e.yml --ref
<branch>`). The `CI` workflow — fmt, Clippy, tests, builds — runs on drafts as
usual. See [AGENTS.md](AGENTS.md) for the per-job breakdown.

Repo map, conventions, and how to add a harness or gateway live in
[AGENTS.md](AGENTS.md).

## Issues first

**Every PR must reference an existing issue.** Open one first describing the bug
or feature — this lets maintainers triage and avoids duplicate work. A small fix
only needs a short issue; just enough context to understand the problem. Link it
with `Fixes #123` or `Closes #123` in the PR description. PRs without a linked
issue may be closed without review.

All issues use a template — **Bug report**, **Feature request**, or
**Question**. Blank issues are disabled. Fill the required fields with real
detail: issues that are empty, placeholder-only, or an AI-generated wall of text
are flagged automatically and closed if not fixed within the grace period.

## Pull requests

- Keep PRs **small and focused**. If you can't explain it briefly, it's probably
  too large.
- Explain **what changed and why, in your own words** — no AI-generated walls of
  text.
- Before adding new functionality, check it doesn't already exist elsewhere in
  the codebase.
- Keep `cargo build`, `cargo test`, `cargo fmt`, and `cargo clippy` green.
- **UI changes:** include before/after screenshots or a short video.
- **Logic changes:** say what you tested and how a reviewer can
  reproduce/confirm the fix.
- Note any change to the service contract in `deploy/contract/` (run
  `npm run sync:contract` in `apps/docs`).

### PR titles

Follow [Conventional Commits](https://www.conventionalcommits.org):
`type(scope): summary`.

| Type       | Use for                              |
| ---------- | ------------------------------------ |
| `feat`     | new feature or functionality         |
| `fix`      | bug fix                              |
| `docs`     | documentation / README              |
| `refactor` | behavior-preserving code change      |
| `test`     | adding or updating tests             |
| `chore`    | deps, tooling, maintenance           |

Scope is optional but encouraged:
`cli`, `control-api`, `proxy`, `gateway`, `config`, `harness`, `dashboard`,
`docs`, `deploy`. Examples: `feat(gateway): add Foo adapter`,
`fix(control-api): reject stale tokens`, `docs: clarify install steps`.

## Contributor License Agreement

Blue is [MIT-licensed](LICENSE). Before your first contribution is merged you'll be
asked to sign our Contributor License Agreement — the CLA Assistant bot comments
on your PR with a one-time signing link and remembers you for future PRs. See
[CLA.md](CLA.md).
