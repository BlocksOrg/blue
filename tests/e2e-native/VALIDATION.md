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
