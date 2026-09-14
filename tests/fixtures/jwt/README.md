# Shared test-only RSA key

This is the repository's one committed test key. It exists **only** for tests
and local stacks.

- `signing-key.pem` is a 2048-bit RSA **private** key (PKCS#8). It signs test
  sign-in tokens in the e2e suites, and it is the gateway signing key for the
  local and e2e Control API. The Control API works out its own public JWKS from
  it. Unit tests in `control-api` and `inference-proxy` use it too.
- `jwks.json` is the matching **public** JWK set, with the fixed
  `kid = e2e-slim-rsa-1`. The e2e sign-in sidecar (`jwks-server.mjs`) serves it,
  and the inference-proxy unit tests read it. A Control API unit test checks that
  the key it works out from `signing-key.pem` matches this file.

**Safe to commit — it grants no access to anything.** It authenticates only
against throwaway test stacks with no real credentials. **Never reuse this key
anywhere else.** To replace it:

```bash
# Make a new private key.
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out signing-key.pem
# Then update jwks.json to match: n and e as base64url.
```

Tests that need other keys, such as a wrong algorithm or a key that is too
small, build them in code instead of committing more key files.
