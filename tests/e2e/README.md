# End-to-end tests

The E2E suite builds the production Blue image, installs the executable fixture in
`fixtures/custom-provisioner`, and layers only that executable
onto the stock image using the consumer deployment pattern. It boots the result with
PostgreSQL, pinned MinIO server/client releases from Quay, the inference proxy,
and a deterministic LiteLLM-compatible
upstream. A lightweight deterministic endpoint stands in for the separately tested
documentation site. The suite then runs the `blue` binary from the deployment
image's shared Rust build stage alongside fake native agent
binaries and Playwright's browser.

No developer credentials or real agent installations are used. Every CLI run
gets an isolated home directory inside the disposable runner container.
The stack generates a disposable CA plus server, proxy, and rogue client
certificates for every run. Gateway journeys use the real mTLS listener and
verify that plaintext, bearer-only, and untrusted-certificate calls cannot reach
the decrypted credential route, while a trusted certificate still requires the
OAuth M2M token.

```bash
tests/e2e/run.sh smoke
tests/e2e/run.sh full
tests/e2e/run.sh mtls
```

The smoke suite covers deployment health, dashboard password authentication,
OAuth device approval, policy reconciliation, custom-provisioner invocation,
encrypted gateway provisioning, managed packages, a governed Codex launch, and
revision drift repair. It also proves that a valid provisioned credential is
reused without another executable invocation and that an unchanged apply keeps
managed bytes and recorded digests stable. The component reads its credential from inherited
runtime environment and stamps its returned alias; the journey asserts that
marker, so module loading, invocation, and credential use must all work. The full suite
adds every OpenAPI route/authentication boundary, all four harness adapters,
PTY behavior, session capture through MinIO, inference credential swapping and
request logs, administrator workflows, SCIM lifecycle, shims, critical
dashboard pages, and browser-level filtering behavior for members, invitations,
sessions, clients, and gateway request logs. Behavioral cases additionally drive
the supervisor command surface through a live PTY, exercise cancel/confirm reset
and gateway revocation, race governance revisions, reject invalid revisions
without mutation, and verify deterministic upstream status, disconnect,
malformed-body, and interrupted-stream handling. The proxy assertions cover
`Connection`-nominated hop headers and confirm that credentials, prompts, and
URL queries are absent from persisted request logs.

The executable provisioner fixture records invocations under
`artifacts/provisioner/` and accepts a `mode` control file with `success`,
`timeout`, `malformed`, `incomplete`, `unavailable`, `protocol-mismatch`,
`nonzero`, and `revocation-failure` modes. The fake upstream exposes matching
deterministic error and broken-transport routes; these controls are test-only
and do not alter production service interfaces.

Per-agent native certification (launching the real, locked agent CLIs and
driving real inference) lives in `tests/e2e-slim` — see
`tests/e2e-slim/README.md` and `tests/e2e-slim/tests/agent_certification.rs`.
That suite runs each locked agent uniformly against a real LiteLLM gateway
backed by OpenRouter, gated on the `OPENROUTER_API_KEY` secret.

Runs use a unique Compose project, always remove their containers and volumes,
and write Playwright output plus sanitized service logs under `artifacts/`.
The committed `coverage.json` is the registry for supported CLI, harness, API,
and dashboard surfaces.
