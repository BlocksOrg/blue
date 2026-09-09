# Blue deployment repository

This starter publishes a deployment-trusted gateway provisioner executable in a separate OCI image. Helm verifies both the immutable image digest and the executable SHA-256, then installs `/executable/provisioner` read-only at `/var/run/blue/provisioner/provisioner` with mode `0555`.

The shell and Python examples implement protocol version 1. Blue invokes the configured absolute path through its shebang, without arguments, sends one JSON request on stdin, and expects one JSON response on stdout. The interpreter and imported dependencies must be installed in the deployed Control API image. Reserve stdout for the protocol; use stderr only for non-secret diagnostics.

Build and pin the artifact:

```bash
docker build -t ghcr.io/your-organization/blue-gateway-provisioner:v1 .
docker run --rm --entrypoint sha256sum ghcr.io/your-organization/blue-gateway-provisioner:v1 /executable/provisioner
```

Set the resulting file digest in both `blue.provisionerExecutable.executableSha256` in `values.yaml` and `gateway.provisioner.executable_sha256` in `blue/blue.yaml`. Set the immutable OCI digest, a non-empty `policy_revision`, administrator credentials in the runtime Secret, and enable the executable. Never put credentials in configuration or protocol diagnostics.

See the packaged custom gateway provisioner documentation for the complete ensure/revoke envelopes, error codes, exit semantics, timeout behavior, credential retention, and retries.
