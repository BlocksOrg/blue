# Dashboard contract and build hardening

## Purpose

Make dashboard builds hermetic, replace manually duplicated API shapes with the
versioned contract, and split the highest-risk interface components into testable
boundaries without changing workflows or visual behavior.

## Runtime-only authentication initialization

Move environment parsing, database pool creation, Better Auth construction, and
bootstrap mutation behind memoized runtime functions. Importing a route, action,
or shared type must perform no network call, database connection, migration, or
bootstrap write.

Use a side-effect-free validation module shared by startup and health reporting.
Production runtime invokes it before accepting traffic; `next build`, typecheck,
lint, and unit tests may import the complete module graph without PostgreSQL.
Concurrent first requests share one initialization promise. A failed
initialization is reported as readiness failure and may be retried with bounded
logging that contains no secret values.

## Generated API contract

Generate TypeScript declarations from `deploy/contract/governance.openapi.yaml`
into `apps/dashboard/lib/generated/governance.ts` using a pinned
`openapi-typescript` development dependency. The file is committed so editor and
offline builds work, and a CI command regenerates to a temporary location and
fails on diff.

Keep the dashboard's fetch/action helpers handwritten. Type their request bodies,
responses, enum filters, pagination, and error payloads from generated component
and operation types. Add small view models only where presentation state differs
from the wire contract; do not copy server DTOs into components.

Contract changes proceed in this order: canonical OpenAPI, server behavior and
contract tests, regenerated TypeScript, dashboard consumers, docs copy. A removed
or narrowed field requires an explicit compatibility migration.

## Package-manager decomposition

Retain the existing route and server actions while splitting the package manager
into:

- a controller hook for loading, draft state, mutation, refresh, and errors;
- pure validation/normalization functions;
- package identity/source and digest editor;
- audience assignment editor;
- per-harness adapter/mapping editor;
- read-only compatibility and publication summary;
- focused confirmation/error presentation.

The controller owns asynchronous state and prevents stale responses from
overwriting newer edits. Child components receive typed values and callbacks and
perform no fetches. Validation is shared between submit enablement and the error
summary, with server errors remaining authoritative.

## Tests

- Run a production build and full module-import smoke test with database host
  access blocked; both must emit no connection attempt.
- Test one-time concurrent auth initialization, retry after dependency recovery,
  validation redaction, and readiness behavior.
- Fail CI when generated types differ from the canonical contract or dashboard
  code reintroduces a duplicate wire DTO in the migrated domains.
- Unit-test package normalization, version intervals, audience changes, stale
  request ordering, server validation mapping, and preservation of unsaved edits.
- Add component tests for create, edit, publish, deactivate, incompatibility,
  loading, empty, permission, and error states; retain the Playwright operator
  journey as the cross-service check.

## Acceptance criteria

- Dashboard build/typecheck/test requires no live service and makes no network
  attempt.
- Migrated API consumers compile exclusively against generated wire types.
- Package management behavior and accessibility remain unchanged while the main
  component becomes orchestration-only and each editor is independently tested.
- OpenAPI, server, dashboard, and docs drift is detected in CI.

