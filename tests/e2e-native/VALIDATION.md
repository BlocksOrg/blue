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
