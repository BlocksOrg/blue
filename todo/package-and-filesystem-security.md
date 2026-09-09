# Package and filesystem security

## Purpose

Harden the two client paths that consume or persist security-sensitive data:
atomic managed-file writes and package acquisition/extraction. Cover hostile
inputs, concurrent reconciliation, abrupt termination, and Windows permissions.

## Atomic-write contract

Create temporary files in the target directory with randomized names and
exclusive `create_new` semantics. Retry a bounded number of name collisions.
Never truncate or share a predictable temporary path.

On Unix, create the temporary file with mode `0600` before writing. On Windows,
create it with an ACL granting the current user and required system principals
access while excluding other interactive users. A permissions failure is fatal
for files that can contain credentials.

Write the full body, call `sync_all`, atomically replace the destination, and
sync the parent directory where supported. Preserve the existing no-op behavior
for identical user configuration. Backups use the same secure primitive. Clean
up only the temporary path owned by the current operation.

Expose separate public helpers only if callers need distinct modes:

- secret / managed configuration: owner-only permissions;
- executable helper: owner-write and executable/readable according to the
  package manifest;
- ordinary non-secret state: explicit, documented mode.

Do not infer security classification from a filename.

## Archive extraction contract

Keep the 100 MiB compressed download limit and add defaults enforced while
streaming extraction:

- 512 MiB total expanded regular-file bytes;
- 10,000 entries;
- 128 MiB per regular file;
- 32 path components and 4,096 encoded path bytes.

Reject an entry before writing bytes when its header alone exceeds a limit.
Count actual copied bytes as well as declared sizes, stop immediately when a
budget is crossed, and remove the operation's staging directory. Continue to
reject absolute paths, parent traversal, links, devices, FIFOs, and other special
entries. Never follow an existing link in the destination path.

Use a random, exclusively created staging directory per install rather than a
PID-only directory. Check available disk space when the platform exposes it,
but treat extraction budgets—not the free-space probe—as the security boundary.

## Network-source validation

For arbitrary public HTTPS archives, resolve every A and AAAA result and reject
loopback, private, link-local, multicast, unspecified, documentation, and other
non-public ranges. Connect only to a validated resolved address while retaining
the original hostname for TLS and HTTP Host verification. Disable redirects by
default; if a source type permits them, repeat resolution and validation for
every hop and cap the chain at five.

Apply the same validation to package inspection and download. Managed repository
connections keep their explicit host allowlist and credentials, and credentials
must never cross to a different redirect origin. Production deployment should
also enforce outbound network policy as defense in depth.

## Tests

- Race many same-process and cross-process writes to one target and verify only
  complete bodies appear, with no shared temporary-file corruption.
- Verify modes/ACLs before and after replacement, permission-error propagation,
  backup security, cleanup, and platform-specific durability behavior.
- Generate archives that exceed every budget independently, lie about sizes,
  contain deep paths, duplicate paths, traversal, and links.
- Test literal and DNS-resolved IPv4/IPv6 restricted addresses, mixed public and
  private answers, rebinding between validation and connection, and redirect
  credential stripping.
- Retain successful GitHub/Bitbucket/public archive and package drift journeys.

## Acceptance criteria

- Managed secrets are never exposed through an intermediate permissive file.
- Concurrent writers cannot collide on or delete each other's temporary paths.
- Package resource use remains inside fixed budgets regardless of compression
  ratio or archive metadata.
- Public package sources cannot cause a request to a non-public address, including
  through DNS changes or redirects.

