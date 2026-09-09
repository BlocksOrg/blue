# Production-pilot hardening roadmap

## Purpose

Turn the holistic application review into independent, issue-sized workstreams.
The target is a production pilot: secure defaults, trustworthy gateway data,
repeatable deployment, and supported client behavior take priority over broad
cleanup or new product surface.

Blue remains configuration governance for cooperative endpoints. This roadmap
does not redefine it as an operating-system sandbox or claim enforcement against
a hostile local administrator.

## Workstreams

| Plan | Priority | Status | Depends on |
| --- | --- | --- | --- |
| [Gateway coding-session attribution](gateway-coding-session-attribution.md) | Pilot blocker | Planned | — |
| [Windows platform parity](windows-platform-parity.md) | Pilot blocker | Planned | — |
| [Dashboard contract and build hardening](dashboard-contract-and-build-hardening.md) | Pilot blocker | Planned | — |
| [Control-plane telemetry](control-plane-telemetry.md) | Post-baseline | Planned | Gateway attribution; dashboard contracts |
| [Codebase modularization](codebase-modularization.md) | Continuous | Planned | Characterization tests for each extracted domain |

Allowed status values are **Planned**, **In progress**, **Blocked**, and
**Complete**. When implementation starts, add the owning issue and active PR to
this table; do not use a plan document in place of the issue required by
`CONTRIBUTING.md`.

## Delivery waves

1. **Trusted gateway:** authenticated coding-session attribution.
2. **Client and operator parity, in parallel:** Windows support and dashboard
   hardening.
3. **Post-baseline capability:** authenticated control-plane telemetry.
4. **Structural work throughout:** extract a domain when it is already being
   changed, then complete the remaining modularization after pilot blockers.

No later workstream may weaken the production-default failure behavior to
preserve an older development convenience.

## Shared delivery rules

- Preserve explicit existing governance choices during upgrades. Safer defaults
  apply only when a value is absent or a deployment is newly created.
- Keep `gh-config` as the only writer of native-agent configuration files.
- Update the versioned OpenAPI contract before consumers when a wire shape
  changes, and keep the docs copy generated from that contract in sync.
- Use small Conventional Commit PRs. Every PR references an issue and includes
  its plan's acceptance tests.
- Do not mark a workstream complete until its docs, local examples, Helm / Compose
  paths, upgrade notes, and relevant end-to-end journey agree.

## Program exit criteria

- Production services fail closed when required secrets or mode dependencies are
  absent, while the explicit local-development path remains one-command usable.
- Package and managed-config writes resist concurrent writers, unsafe modes,
  archive resource exhaustion, traversal, and network-source SSRF.
- Gateway attribution is derived from authenticated session state; caller-spoofed
  attribution never reaches logs or upstreams.
- A clean governance-only deployment is the default, and the documented gateway
  deployment passes its complete health and rotation journey.
- Linux, macOS, and Windows pass the defined CLI lifecycle matrix.
- Builds are hermetic, schema drift is detected, production migrations are
  separate from replica startup, and the required security scans run in CI.
