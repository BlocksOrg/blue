# Native Windows CLI E2E

Run from a **fresh Windows test account** in PowerShell:

```powershell
./tests/e2e-windows/run.ps1
```

Requires Git, stable Rust's MSVC toolchain, Visual Studio C++ Build Tools and a
Windows SDK. The `End-to-end (Windows CLI)` workflow runs on Windows Server 2022
for PRs (including drafts), main and manual dispatch. The same command works on
an EC2 Windows Full/Base VM. It requires no Docker, backend credentials, npm
registry access or inference calls at test runtime.

The suite drives the actual `blue.exe` in a Windows pseudo-console. Blue reads a
local governance policy, discovers a simulated npm installation (`codex`,
`codex.cmd`, `codex.ps1`), reconciles managed configuration and launches the
wrapper through its interactive supervisor and a second ConPTY. A compiled
fixture behind the wrapper reports its argv and receives terminal input.

Coverage:

- First-run version detection and policy reconciliation with npm-style siblings.
- A wrapper directory with spaces; argv with spaces, quotes, backslashes and `!`.
- Terminal input after a resize, output and nonzero exit-code propagation.
- Redirected execution and native `.exe` fallback (including special characters).
- Repeated launches and refusal of command-interpreter metacharacters before
  the fixture launches.

Windows Known Folder APIs do not honor a fake `USERPROFILE`. The suite therefore
refuses existing `.codex`, local `Blue`, or legacy `.config/blue` directories.
It reserves new `.codex` and local `Blue` directories and removes only those on
completion; temporary config/cache and wrapper files are also cleaned up. Do not
run alongside another Blue/Codex process. Use a disposable VM/account for local
runs; an interrupted process can leave directories requiring manual cleanup.

Each launch has a 60-second deadline, progress output and captured terminal
output on failure. CI uploads the run transcript. This standalone Cargo workspace
is deliberately separate from `tests/e2e` and `tests/e2e-slim`; their Linux
workflows and deployment image dependencies are unchanged.

This is client process E2E using a fixture agent and file policy, not backend
login, real inference or a certification of the real Codex UI. Those remain
covered by the existing suites and manual testing with a real agent installation.

The separate [`../e2e-native`](../e2e-native/README.md) runner exercises shared
backend-connected slim governance and real-agent gateway scenarios on native
clients. This ConPTY suite remains a fixture-based launcher regression check;
it alone does not certify real agents, inference or backend session journeys.
Do not run both suites concurrently under the same disposable Windows account.
