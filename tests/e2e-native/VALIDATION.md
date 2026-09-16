> Current scope: native Windows E2E is manual only. The AWS-backed GitHub
> workflow was removed at the user's request. Fixtures, the runner, isolation
> regression, backend helpers, and infrastructure module remain available for
> on-demand runs. Workflow references below are historical evidence, not active
> automation. See [manual setup](README.md#run-on-demand).

# Validation recorded 2026-09-15

| Target | Result |
| --- | --- |
| Linux native entry point, governance | Passed: all 16 pinned/historical agent cells; 28 shared tests, zero skips |
| Existing Linux full container suite | Passed: 64 tests |
| Existing Linux smoke suite | Passed: 11 tests |
| Root Cargo workspace | Build, tests, formatting and Clippy passed |
| Shared slim Rust support | Unit tests, formatting and Clippy passed |
| Windows compilation | All shared targets checked for `x86_64-pc-windows-gnu`; compilation only, not MSVC/native execution |
| Node support | 9 helper tests passed, including runtime/platform rejection, cleanup after backend failure and subprocess cancellation |
| Workflow / infrastructure | actionlint, OpenTofu formatting and validation passed |
| Existing Linux `run.sh` | Startup blocked by an existing SSH listener on 5432; preserved that listener |
| Windows x64 native governance/gateway | Not run; requires dedicated AWS backend resources and protected GitHub environment |
| macOS native governance/gateway | Not run; same prerequisite |
| Real gateway inference | Not run locally: provider secret unavailable |
| Windows ARM64 | Unverified; existing protected workflow remains unchanged |

The successful native Linux invocation used Node 22.23.2, the existing PR's
`images.yml` artifact, and `E2E_SLIM_DATABASE_URL` with local port 15432 to avoid
the occupied 5432. Initial runtime preflight correctly rejected Node 22.14 for
Kimi 0.39.1 (requires >=22.19). No agent pins or compatibility ceilings changed.

Public package URLs intentionally reject HTTP/loopback. Native fixtures therefore
seed a managed artifact row and use the existing authenticated package download
API. The actual MinIO object/digest and presigned download path passed the full
config grid. No production package access rules changed.

In the accessible AWS account, `blue-e2e-native-github` and
`blue-e2e-native-instance` returned `NoSuchEntity`. GitHub variable/secret discovery
was denied to the available integration. No infrastructure was created. Apply the
isolated module with reviewed network/AMI inputs and configure the protected
GitHub environment before dispatching native jobs. Successful native gateway
runs are still required before claiming Windows/macOS parity.

## Windows-only follow-up, 2026-09-15

Additional macOS native certification is discontinued; the dated evidence above
is retained. Existing Linux suites and macOS product/release support are unchanged.

The prior [Windows setup run](https://github.com/BlocksOrg/blue/actions/runs/35027010693/job/104578016803)
failed before scenarios with `Input required and not supplied: aws-region`.
All seven protected environment variables were absent during implementation.
The environment permits the implementation branch (no deployment branch policy).
In the accessible AWS account `767397683479`, region `us-east-1`, the dedicated
role and instance profile still return `NoSuchEntity`; no `BlueE2E=native` tagged
resources or account-owned AMIs were found. No infrastructure state or reviewed
test network/AMI inputs were available locally. No infrastructure was provisioned.
Windows MSVC isolation, governance, gateway, and cloud cleanup remain unverified
until dedicated infrastructure and the protected environment are configured.

Follow-up local checks: all 14 Node helper tests passed; `actionlint` passed for
all workflows; OpenTofu initialization, formatting and validation passed. The
preflight CLI exited 1 and listed all seven missing CI settings. An AWS runtime
attempt without configuration recorded all 16 cells as not certified and failed
before tool setup; cleanup-only succeeded without those inputs. The isolation
wrapper tests reject zero/ignored tests and preserve subprocess failure output;
they do not substitute for Windows execution.

Root workspace build, tests, `cargo fmt --all --check`, and
`cargo clippy --all-targets` passed. Build/test/Clippy used
`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0` to fit
local disk capacity after stopping the initial debug-symbol build.

Manual workflow dispatch was denied with HTTP 403 (`Resource not accessible by
integration`). Pushing implementation commit `b9a25f4` triggered the
[PR native workflow](https://github.com/BlocksOrg/blue/actions/runs/35030256590)
instead. Gateway remains excluded from PR runs by design.

That run scheduled only `governance (windows-2022)` (no macOS job). The
[Windows job](https://github.com/BlocksOrg/blue/actions/runs/35030256590/job/104588464282)
failed at the new preflight step, listing all seven missing variables and the
setup documentation before credentials, tool installation, or backend allocation.
Only image/build artifacts were uploaded: no native coverage, cell, isolation,
or lease artifacts exist. This verifies Windows preflight execution, not Windows
scenario execution or cloud cleanup.

[CI](https://github.com/BlocksOrg/blue/actions/runs/35030256583) and the existing
[Windows CLI suite](https://github.com/BlocksOrg/blue/actions/runs/35030256346)
passed on implementation commit `b9a25f4`.
The existing [Linux smoke workflow](https://github.com/BlocksOrg/blue/actions/runs/35030256629),
[Linux slim governance](https://github.com/BlocksOrg/blue/actions/runs/35030256556),
and [security gates](https://github.com/BlocksOrg/blue/actions/runs/35030256370)
also passed on that commit. Linux full and gateway are not PR jobs and were not
rerun for this follow-up.

## CodeQL follow-up, 2026-09-15

The successful Security gates workflow above did **not** imply that the separate
[CodeQL alert check](https://github.com/BlocksOrg/blue/runs/104587043326) passed.
That check reported `js/insecure-download` on the native readiness probe's HTTP
archive download. The readiness probe now reads a fresh inert `health.txt` object
and verifies its digest with redirects disabled. Only that health object permits
anonymous reads; the executable package archive requires its signed URL. The
shared config matrix continues to exercise package download and SHA-256 checking
through Blue's authenticated artifact flow.

Local verification passed: 14 Node helper tests, workspace build/tests/fmt/Clippy,
and actionlint. A local native backend run verified the health-object digest,
HTTP 403 for anonymous archive access, and all 28 shared governance tests with
all 16 pinned/historical cells passed. This run reused installed agent versions
with Node 24.5.0 and the local image, used Postgres port 15432, and cleaned up the
Compose stack afterwards. This remains Linux evidence, not Windows certification.
