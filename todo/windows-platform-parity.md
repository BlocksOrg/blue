# Windows platform parity

## Purpose

Make Windows a fully supported Blue client platform with the same governed
lifecycle as Linux and macOS. A Windows release artifact alone does not satisfy
this plan; launch, reconciliation, shims, authentication, gateway routing, and
session behavior must be certified on Windows.

## Platform contract

The user-visible lifecycle matrix is:

```text
setup -> login -> doctor -> apply -> run -> status -> verify
      -> shim install/run/uninstall -> session capture/resume -> reset/logout
```

All commands preserve native Windows paths, Unicode arguments, spaces, quotes,
environment variables, stdout/stderr, process exit codes, and cancellation.
Where POSIX signals have no Windows equivalent, document and test the equivalent
console-control/process-tree behavior instead of claiming byte-for-byte parity.

## Native shims and paths

- Use `%LOCALAPPDATA%\Blue\bin` as the default Windows shim directory.
- Install `codex.cmd`, `claude.cmd`, `kimi.cmd`, and `opencode.cmd`; retain the
  extensionless Bash shims on Unix.
- Generate CMD wrappers with a marker, an absolute quoted Blue executable, the
  fixed harness argument, `--`, and lossless forwarding of `%*`.
- Detect an existing non-Blue file before writing and fail without changing it.
  Uninstall only a file whose exact marker and expected command shape validate.
- Update PATH guidance for the current user, but do not mutate PATH automatically.
  `blue doctor` reports whether the shim directory precedes native agent paths.

Resolve home/config/cache/data paths through platform APIs. Normalize paths only
for comparison; retain native spelling when invoking tools or presenting errors.
Atomic managed files follow the Windows ACL contract in
[Package and filesystem security](package-and-filesystem-security.md).

## Process and terminal behavior

Encapsulate platform behavior behind the existing harness launch boundary. On
Windows, use the `portable-pty` ConPTY backend for interactive agents, propagate
resize events, and restore console modes through RAII on success, error, panic,
and cancellation. Forward Ctrl-C/Ctrl-Break using Windows console semantics and
terminate the complete child process tree when graceful shutdown expires.

Non-interactive invocation must continue using pipes and exact exit-code
propagation. Browser login uses the system browser with the existing printed-URL
fallback. Executable discovery honors `PATHEXT` and rejects the Blue shim itself
when locating the upstream binary.

Audit locks, executable ownership detection, helper execution, package modes,
session-path discovery, Git invocation, and daemon/background worker startup.
Replace silent `cfg(not(unix))` no-ops with a tested Windows implementation or an
explicit unsupported error; no security-sensitive operation may silently weaken.

## CI and certification

Add a `windows-latest` job that builds all workspace targets, runs unit tests,
Clippy, and the slim end-to-end harness using fake `.cmd`/`.exe` agents. Add an
interactive ConPTY test executable that changes screen modes, resizes, emits
stdout/stderr, handles cancellation, spawns a child, and exits with a chosen
code.

The Windows journey covers:

- install and version discovery for all four harnesses;
- transactional reconcile, rollback, package activation, and secure state;
- direct and shimmed launches with difficult Unicode/quoted arguments;
- terminal resize, Ctrl-C, process-tree cleanup, and exit status;
- OAuth browser fallback, gateway configuration, and session capture/resume.

Release certification must run on Windows for every tagged release rather than
cross-compiling only. Upload logs and test reports without user paths or secrets.

## Acceptance criteria

- The complete lifecycle matrix passes on current Windows Server/GitHub runner
  and a supported Windows desktop release.
- Shims are safe, reversible, correctly ordered on PATH, and cannot overwrite
  unrelated commands.
- Interactive and non-interactive native agents receive exact arguments and
  return exact exit status; cancellation leaves no child process or console-mode
  damage.
- Documentation lists Windows-specific paths and semantics without experimental
  qualifiers.

