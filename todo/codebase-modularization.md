# Codebase modularization

## Purpose

Reduce review and change risk in the largest Rust modules through incremental,
behavior-preserving extraction. This is not a rewrite and does not block isolated
pilot security fixes.

## Rules

- Add characterization tests before moving a domain.
- Keep routes, JSON/YAML shapes, status codes, database queries, config ownership,
  CLI output, and exit codes unchanged in extraction PRs.
- Do not combine structural movement with feature behavior unless the feature is
  new and can begin inside the target module.
- Keep module internals private by default. Cross-domain calls use small service
  interfaces rather than sharing the complete application state.
- Avoid generic `utils`, `helpers`, or `common` dumping grounds. Shared code must
  have a named invariant or owning domain.

## Control API target

Retain a small application root responsible for configuration validation,
dependency construction, router composition, startup, and shutdown. Extract
these domains in order:

1. identity, OAuth/service authorization, users, invitations, and SCIM;
2. governance revisions, managed configuration, catalog, and client reporting;
3. packages, repository connections, inspection, mirroring, and source security;
4. gateway policy, credentials, health, request logs, and coding sessions;
5. captured sessions, object storage, retention, and sharing;
6. telemetry ingestion/query/retention.

Each domain contains its request/response adapters, validation, service logic,
repository queries, and tests. Database transaction helpers stay with the domain
that owns the transaction; truly shared tenant/user lookup primitives live in the
database layer. New attribution and telemetry code starts in its target domain
rather than entering the current root module.

Router snapshots and the OpenAPI surface test must prove that extraction neither
adds nor removes a route, method, scope, or response contract.

## CLI target

Retain argument parsing and dispatch in `main`; split command implementation into:

- setup, identities, login/logout, and auth polling;
- discovery, status, doctor, and verification;
- policy fetch, apply, reconciliation, and reset;
- governed launch and agent selection;
- gateway setup/status;
- session capture/upload/resume;
- shim lifecycle;
- daemon and background workers.

Introduce an explicit command context carrying filesystem roots, clock, terminal
capabilities, HTTP clients, and process launcher. Production construction uses
real dependencies; tests inject fakes without global environment mutation.
Domain command modules call `gh-config` plans and public service clients and do
not write native-agent configuration directly.

The supervisor remains separate from command orchestration. Split it only along
stable responsibilities—terminal model, control overlay, input routing, and
rendering—after its existing terminal regression suite characterizes behavior.

## Dashboard boundary

The dashboard-specific component split is owned by
[Dashboard contract and build hardening](dashboard-contract-and-build-hardening.md).
This plan only enforces the cross-language boundary: generated OpenAPI DTOs at
the edge and local view models inside the UI.

## Delivery and tests

Use one domain per PR. A typical PR first moves tests, then internal types and
queries, then routes/commands, and finally deletes the old section. Temporary
re-exports are allowed for one follow-up PR and must be recorded for removal.

For every extraction:

- run workspace format, Clippy, and tests;
- run SQLx offline checks for moved queries;
- compare route/auth surface snapshots or CLI golden output;
- run the affected slim and full end-to-end journeys;
- report line counts and dependency direction before and after.

## Acceptance criteria

- The Control API root contains composition/startup rather than domain behavior,
  and no domain depends on another domain's route module.
- CLI dispatch and command modules have explicit dependencies and no new native
  config writer outside `gh-config`.
- No extraction changes a public contract or user-visible behavior.
- New feature work can be reviewed and tested within one named domain without
  navigating a multi-thousand-line root file.

