# Shared native client E2E

Runs the existing `e2e-slim` Rust scenarios and generated agent/version grid with
a native Blue binary. Backends remain Linux. Windows x64 (`windows-2022`) gets
a separate EC2 backend per suite and run attempt. Existing Linux suites provide
Unix coverage; additional macOS native certification is discontinued. Windows
ARM64 remains unverified.

## Run

Install Node >=22.19, native Rust (MSVC and Windows SDK on Windows), Git (including
Git Bash for Claude on Windows), cargo-nextest, AWS CLI and the AWS Session
Manager plugin. Install exact Node support dependencies:

```sh
npm ci --prefix tests/e2e-native --ignore-scripts
npm test --prefix tests/e2e-native
node tests/e2e-native/run.mjs --backend aws --suite governance --matrix
# OPENROUTER_API_KEY must be set for real inference:
node tests/e2e-native/run.mjs --backend aws --suite gateway --matrix
```

`--backend local` is available on Linux with Docker and the `blue-e2e-base:local`
image already loaded from `images.yml`. For local Linux port conflicts, `E2E_SLIM_DATABASE_URL` may select another
loopback Postgres port; AWS always uses 5432 on both sides. The original `tests/e2e-slim/run.sh`
remains the Linux convenience path. The full container suite is unchanged.

AWS inputs (dedicated test resources, see [infra](infra/README.md)):

| Environment variable | Value |
| --- | --- |
| `E2E_NATIVE_BUCKET` | Private test bucket |
| `E2E_NATIVE_SUBNET_ID` | Subnet with outbound connectivity |
| `E2E_NATIVE_SECURITY_GROUP_ID` | No-ingress test security group |
| `E2E_NATIVE_INSTANCE_PROFILE` | Dedicated SSM/test-storage instance profile |
| `E2E_NATIVE_AMI_ID` | Linux amd64 Docker/Compose/SSM AMI |
| `E2E_NATIVE_IMAGE_TAR` | Downloaded `images.yml` `blue-images.tar` |
| `E2E_NATIVE_RUN_DIR` | Absolute staging/report root, outside agent state roots |
| `E2E_SLIM_DISPOSABLE_ACCOUNT` | `1`, required on Windows |

In CI also configure `E2E_NATIVE_ROLE_ARN` and `E2E_NATIVE_REGION` as variables
in the protected `blue-e2e-native` GitHub environment. Allow its role a two-hour
session. Run `node tests/e2e-native/preflight.mjs --ci` to validate all seven
settings before CI setup. Manual AWS runs validate the five backend variables
and may use the normal AWS credential/region chain. Cleanup-only runs do not
require these setup settings. The gateway secret is `OPENROUTER_API_KEY`. A
missing provider secret means **not run**, never certified. Forks retain the existing Linux and Windows
fixture checks and cannot access this AWS workflow.

## Windows isolation

Use a fresh disposable OS account, never your daily account. The suite resolves
real Known Folders and refuses existing Blue or agent roots, including legacy
Blue state. It removes only its enumerated application roots, never a profile,
`.config`/`.local` parent or agent installation prefix. Installation prefixes and
fixtures live in the run directory. No production path override is added.

Before agent installation or backend allocation, the Windows runner executes
`platform::windows_tests::sequential_native_profiles_remove_owned_state` with
MSVC. Exactly one passing test is required; `windows-isolation.log` retains its
output, including failures.

All native tests are serialized (including retries). A cross-process reservation
rejects simultaneous Homes; a Windows Job Object tracks native descendants,
including upload grandchildren, before state cleanup. If cleanup fails, its
reservation remains and subsequent cases fail. Inspect the process tree and
state before removing a stale `LocalAppData/blue-e2e-slim.lock`; creating another
account is safer. Do not run the Windows ConPTY fixture suite concurrently under
the same account. Tests needing two simultaneous Homes require two OS accounts.

## Evidence and boundaries

Both profiles use exact lock pins plus historical samples; npm validates engine
and platform requirements, and every actual CLI version must match. An unsupported
required package is a failed install with a reason in `agents/install-report.json`.
`coverage.json` lists expected and completed cells. CI cannot pass with missing
endpoints, agents, cells, or zero real-agent cases. Completed gateway cells prove
MCP start/tool use with fresh per-case markers, a nonce-bearing session bundle,
and this logical user's inference and credential swap through the real proxy.

Governance includes signed-session bootstrap verified by the backend, managed
configuration, session recovery and synthetic native transcript uploads. Gateway
uses real agent invocations and inference. Slim does **not** cover browser/device
approval, SCIM, mTLS, full upstream TUIs or the full container topology; those
remain in Linux `tests/e2e`. A checked-in workflow is not proof of Windows
parity: require a successful dispatched gateway grid before making that claim.

## Backend lifecycle

The runner uploads tested tracked source bytes (stage new files before local AWS
runs), generated policies, and exact image artifact to a private run prefix.
Generated policies carry native MCP paths and an HTTP package object with its
exact SHA-256. A test-only database row associates the package with its artifact
ID; Blue uses its existing authenticated artifact API to obtain the presigned
HTTP download. Direct public package fetches still require HTTPS/public hosts.
The disposable MinIO grant permits reading that one fixture only.
Existing full-suite fixtures and the Linux legacy tarball bytes are preserved.

SSM forwards 8080, 5432 and 9000; gateway adds 8081 and 4000. No internal 8082 or
dashboard port is exposed. Health checks traverse the client tunnels and verify
an actual package read/digest and database query. A dead tunnel aborts the tests.
`lease.json` records run, attempt, source SHA, image artifact digest, instance,
prefix, expiry and mappings without secrets.

Cleanup runs in `finally` and an `always()` CI step. It gathers sanitized backend
logs, stops Compose with volumes, closes tunnels, terminates the instance and
deletes run objects. The independent expiry reaper covers lost/cancelled runners.
To retry cleanup after an interrupted process, use the same run directory:

```sh
node tests/e2e-native/run.mjs --backend aws --suite governance --cleanup-only
```

The two-hour CI timeout is below the three-hour lease expiry. Do not reuse a run
ID/attempt/OS/suite concurrently or share the local Compose stack between runs.
