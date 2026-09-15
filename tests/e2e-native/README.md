# Shared native client E2E

Runs the existing `e2e-slim` Rust scenarios and generated agent/version grid with
a native Blue binary, on demand. There is no automated native Windows E2E
workflow. Windows x64 uses a disposable Linux EC2 backend per suite/run; existing
Linux suites provide Unix coverage. Windows ARM64 remains unverified.

## Run on demand

Use a fresh disposable Windows account with Node >=22.19, native Rust MSVC and
Windows SDK, Git (including Git Bash for Claude), cargo-nextest, AWS CLI, and the
AWS Session Manager plugin installed. Configure authorized dedicated-test AWS
credentials through the normal AWS credential chain (for example `AWS_PROFILE`)
and select the region with `AWS_REGION`. The runner does not assume a GitHub role.

Set the five backend inputs from the dedicated test resources (see
[infra](infra/README.md)):

| Environment variable | Value |
| --- | --- |
| `E2E_NATIVE_BUCKET` | Private test bucket |
| `E2E_NATIVE_SUBNET_ID` | Subnet with outbound connectivity |
| `E2E_NATIVE_SECURITY_GROUP_ID` | No-ingress test security group |
| `E2E_NATIVE_INSTANCE_PROFILE` | Dedicated SSM/test-storage instance profile |
| `E2E_NATIVE_AMI_ID` | Linux amd64 Docker/Compose/SSM AMI |

Obtain `blue-images.tar` from an existing Linux E2E run's `blue-images` artifact
for the revision being tested, using the Actions UI or:

```sh
gh run download <run-id> -n blue-images -D <image-directory>
```

The native suite does not trigger an image build. In PowerShell, set the staging paths and acknowledge the disposable account:

```powershell
$env:E2E_NATIVE_IMAGE_TAR = 'C:\e2e\images\blue-images.tar'
$env:E2E_NATIVE_RUN_DIR = 'C:\e2e\governance'
$env:E2E_SLIM_DISPOSABLE_ACCOUNT = '1'
node tests/e2e-native/preflight.mjs
npm ci --prefix tests/e2e-native --ignore-scripts
npm test --prefix tests/e2e-native
node tests/e2e-native/run.mjs --backend aws --suite governance --matrix

# Set OPENROUTER_API_KEY in the environment for real inference first.
$env:E2E_NATIVE_RUN_DIR = 'C:\e2e\gateway'
node tests/e2e-native/run.mjs --backend aws --suite gateway --matrix
```

Keep staging/report directories outside agent state roots. Preflight validates
all five backend inputs; cleanup-only does not require those setup settings.
Gateway without `OPENROUTER_API_KEY` is **not run**, never certified. Retain
`coverage.json`, `cells/*.json`, `windows-isolation.log`, `nextest.log`,
`agents/install-report.json`, `backend.log`, and `lease.json` from each run directory
as evidence. Reports are local; there is no automatic artifact upload.

`--backend local` is available on Linux with Docker and the `blue-e2e-base:local`
image already loaded. For local Linux port conflicts, `E2E_SLIM_DATABASE_URL` may
select another loopback Postgres port; AWS always uses 5432 on both sides. The
original `tests/e2e-slim/run.sh` remains the Linux convenience path.

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
`coverage.json` lists expected and completed cells. A run cannot pass with missing
endpoints, agents, cells, or zero real-agent cases. Completed gateway cells prove
MCP start/tool use with fresh per-case markers, a nonce-bearing session bundle,
and this logical user's inference and credential swap through the real proxy.

Governance includes signed-session bootstrap verified by the backend, managed
configuration, session recovery and synthetic native transcript uploads. Gateway
uses real agent invocations and inference. Slim does **not** cover browser/device
approval, SCIM, mTLS, full upstream TUIs or the full container topology; those
remain in Linux `tests/e2e`. Windows parity requires a successful manual gateway
grid, not just fixtures or a passing CLI check.

## Backend lifecycle

The runner uploads tested tracked source bytes (stage new files before local AWS
runs), generated policies, and exact image artifact to a private run prefix.
Generated policies carry native MCP paths and an HTTP package object with its
exact SHA-256. A test-only database row associates the package with its artifact
ID; Blue uses its existing authenticated artifact API to obtain the presigned
HTTP download. Direct public package fetches still require HTTPS/public hosts.
The executable archive is private in MinIO and requires a signed URL. The
anonymous grant permits reading only a fresh, inert `health.txt` fixture.
Existing full-suite fixtures and the Linux legacy tarball bytes are preserved.

SSM forwards 8080, 5432 and 9000; gateway adds 8081 and 4000. No internal 8082 or
dashboard port is exposed. Health checks traverse the client tunnels and verify
the fresh health object's bytes/digest and a database query. The shared config
matrix separately exercises archive download and SHA-256 verification through
Blue's authenticated artifact flow. A dead tunnel aborts the tests.
`lease.json` records run, attempt, source SHA, image artifact digest, instance,
prefix, expiry and mappings without secrets.

Cleanup runs in the runner's `finally` block. It gathers sanitized backend
logs, stops Compose with volumes, closes tunnels, terminates the instance and
deletes run objects. The independent expiry reaper covers lost/cancelled runners.
To retry cleanup after an interrupted process, use the same run directory:

```sh
node tests/e2e-native/run.mjs --backend aws --suite governance --cleanup-only
```

Each backend lease expires after three hours. Finish manual runs within that
window; use cleanup-only after interruption. Do not reuse a run ID/attempt/OS/suite
concurrently or share the local Compose stack between runs.
