<div align="center">

<pre align="center">
 ____  _&#32;&#32;&#32;&#32;&#32;&#32;&#32;&#32;&#32;&#32;&#32;&#32;
| __ )| |_   _  ___&#32;
|  _ \| | | | |/ _ \
| |_) | | |_| |  __/
|____/|_|\__,_|\___|
</pre>

### Open-source governance for coding-agent CLIs

[![CI](https://github.com/BlocksOrg/blue/actions/workflows/ci.yml/badge.svg)](https://github.com/BlocksOrg/blue/actions/workflows/ci.yml)
[![End-to-end](https://github.com/BlocksOrg/blue/actions/workflows/e2e.yml/badge.svg)](https://github.com/BlocksOrg/blue/actions/workflows/e2e.yml)
[![Documentation](https://github.com/BlocksOrg/blue/actions/workflows/docs.yml/badge.svg)](https://github.com/BlocksOrg/blue/actions/workflows/docs.yml)
[![GitHub Release](https://img.shields.io/github/v/release/BlocksOrg/blue)](https://github.com/BlocksOrg/blue/releases/latest)
[![License](https://img.shields.io/github/license/BlocksOrg/blue)](LICENSE)

</div>

Blue is a standalone, self-hosted metaharness that gives organizations one
place to govern coding-agent configuration without changing how developers use
their native CLIs. It supports **Codex**, **Claude**, **Kimi**, and
**OpenCode**.

## Why a metaharness?

A single provider can manage identity, policy, credentials, and client settings
in one place. Those controls split apart when an organization adopts multiple
agents, model providers, open-weight models, or self-hosted inference. Blue puts
one governance layer around those choices while preserving each agent's native
interface.

Read the [Blue manifesto](https://bluee.sh/manifesto) for the complete argument:
shared policy, identity-based access, server-held gateway credentials, useful
activity records, and support for multiple native agents.

> [!IMPORTANT]
> Blue governs agents through configuration; it is not a sandbox and does not
> contain what an agent can do on a developer's machine. See the
> [security and trust model](SECURITY.md).

## Install Blue

Install the latest release on macOS or Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/BlocksOrg/blue/releases/latest/download/install.sh | sh
```

On Windows PowerShell:

```powershell
irm https://github.com/BlocksOrg/blue/releases/latest/download/install.ps1 | iex
```

The installer detects your platform, downloads the matching release asset,
verifies its checksum, and installs `blue` in your user path. You can also
[inspect the release assets and installers](https://github.com/BlocksOrg/blue/releases)
before running them.

Connect to your organization's Blue deployment and launch an agent:

```bash
blue setup       # enter the Control API URL provided by your administrator
blue login       # authenticate with the browser-based device flow
blue doctor      # check installed agents and organization policy
blue codex       # launch Codex through Blue
```

Replace `codex` with `claude`, `kimi`, or `opencode`. Running bare `blue`
launches your preferred agent.

For version pinning, custom installation directories, and the complete setup
flow, read the [Quickstart](https://docs.bluee.sh/next/quickstart#install-the-workstation-cli).

## Deploy Blue

Blue consists of the workstation CLI plus a self-hosted control plane. The
reference deployment includes the dashboard, Control API, a singleton
background worker, PostgreSQL-backed state, S3-compatible object storage, and
an optional inference proxy.

### Evaluate locally with Docker Compose

```bash
git clone https://github.com/BlocksOrg/blue.git
cd blue/deploy
docker compose up --build
```

Once the services are healthy:

| Service | Local address |
| --- | --- |
| Dashboard | <http://127.0.0.1:3000> |
| Control API | <http://127.0.0.1:8080> |
| PostgreSQL | `127.0.0.1:5433` |
| MinIO API | <http://127.0.0.1:9000> |
| MinIO console | <http://127.0.0.1:9001> |

The optional inference proxy (<http://127.0.0.1:8081>) starts only with the
`gateway` Compose profile.

The local bootstrap account is `admin@example.com` with password
`change-me-in-production`. Override
`HARNESS_BOOTSTRAP_ADMIN_EMAIL` and `HARNESS_BOOTSTRAP_ADMIN_PASSWORD` in
`deploy/.env` before using the stack outside an isolated development machine.

### Deploy to production

Each GitHub release provides a deployment bundle, multi-architecture container
image, Helm chart, and AWS OpenTofu starter. Blue can run on any platform that
implements its workload, networking, PostgreSQL, S3-compatible storage, TLS,
and secret-delivery contract. Kubernetes with Helm is the maintained reference.

Start with the [deployment contract](https://docs.bluee.sh/next/deployment/runtime-contract),
or follow [Kubernetes with Helm](https://docs.bluee.sh/next/deployment/production)
for the concrete reference implementation, security baseline, and upgrade
guidance. The repository also contains the [Helm chart](deploy/helm/README.md) and
[AWS dependency starter](deploy/tofu/aws/README.md) references.

## What Blue manages

- **Allowed harnesses and versions** — decide which coding agents may run and
  reject incompatible versions before launch.
- **Launch-scoped configuration** — apply organization policy without replacing
  developers' personal agent configuration.
- **MCP servers and extensions** — distribute digest-pinned skills, plugins,
  hooks, subagents, and helper binaries.
- **Authentication and identity** — connect the CLI through OAuth device
  authorization; support invitation-based login or deployment-managed OIDC and
  SCIM provisioning.
- **Optional inference gateway** — route governed traffic through a
  LiteLLM-compatible gateway while keeping real provider keys server-side.
- **Optional session capture** — upload native agent transcripts to
  operator-controlled object storage using short-lived presigned URLs.

Raw-session capture is disabled unless the deployment explicitly configures it.
Because captured sessions may contain source code, prompts, tool results, paths,
and secrets, operators should publish a retention and access policy before
enabling it.

## How it works

For every governed launch, Blue:

1. Authenticates the developer to the organization's control plane.
2. Fetches policy personalized for that identity and the installed agents.
3. Verifies agent compatibility and managed-package digests.
4. Reconciles approved configuration into a metaharness-owned overlay.
5. Starts the native CLI and transparently forwards arguments, terminal I/O,
   signals, resize events, and its exit status.

```text
Developer                     Organization

blue codex ──▶ policy sync ──▶ Control API ──▶ PostgreSQL / object storage
     │                              │
     └──▶ native Codex CLI          └──▶ optional inference proxy ──▶ gateway
```

Blue has two service-declared operating modes:

- **Governance-only (default):** manage agent configuration while inference
  continues directly through each agent's existing provider credentials.
- **Gateway mode (opt-in):** route inference through the organization's proxy
  using a session-bound, inference-only JWT. Provider and gateway credentials
  remain on the server.

Learn more in the documentation for
[architecture](https://docs.bluee.sh/next/concepts/architecture),
[configuration](https://docs.bluee.sh/next/concepts/configuration), and
[gateway mode](https://docs.bluee.sh/next/concepts/gateway-mode).

## Common commands

| Command | Purpose |
| --- | --- |
| `blue` | Reconcile policy and launch the preferred agent. |
| `blue setup` | Connect or reconnect to a deployment. |
| `blue reset [--yes]` | Disconnect the active deployment, retaining non-secret tenant state for a later reconnect. |
| `blue login` / `blue logout` | Start or end the authenticated session. |
| `blue doctor` | Show detected harnesses, versions, and policy eligibility. |
| `blue agent [name]` | Show or change the preferred coding agent. |
| `blue status` | Show desired and applied revisions, package state, and health. |
| `blue verify` | Exit non-zero when managed files or policy are stale. |
| `blue <agent> [args]` | Launch a supported native CLI through Blue. |
| `blue apply` | Reconcile configuration without launching an agent. |
| `blue shim install` | Route supported agent commands through Blue on `PATH`. |

See the [complete CLI reference](https://docs.bluee.sh/next/cli/commands)
for all commands and options.

## Architecture

This Cargo workspace produces one `blue` client binary and two reference
backend services:

```text
crates/          Blue CLI, policy client, configuration adapters, launcher,
                 gateway integration, agent daemon, and telemetry
services/        Control API and optional inference proxy
apps/            Administration dashboard, Mintlify documentation, and website
deploy/          Compose, Helm, OpenTofu, deployment starter, and OpenAPI
tests/e2e/       Hermetic deployment and cross-service journeys
```

The included backend is a reference implementation, not a requirement. A
compatible service can implement the versioned
[governance OpenAPI contract](deploy/contract/governance.openapi.yaml).

## Build from source

Blue uses the Rust toolchain pinned by `rust-toolchain.toml`.

```bash
git clone https://github.com/BlocksOrg/blue.git
cd blue
cargo build --release
./target/release/blue version
```

Repository development, optional pre-commit hooks, local file-source
configuration, and test commands are covered in
[CONTRIBUTING.md](CONTRIBUTING.md) and the
[development documentation](https://docs.bluee.sh/next/development/local-compose).

## Project resources

- [Documentation](https://docs.bluee.sh)
- [Releases](https://github.com/BlocksOrg/blue/releases)
- [Contributing guide](CONTRIBUTING.md)
- [Security policy and trust model](SECURITY.md)
- [Release process](RELEASING.md)
- [Service API contract](deploy/contract/governance.openapi.yaml)

## License

Blue is licensed under the [MIT License](LICENSE).
