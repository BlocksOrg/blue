# Test-only RSA keys

These keys exist **only** to sign JWTs for the `e2e-slim` suite so that
control-api's real JWKS-based verification path can be exercised without a
dashboard / Better Auth stack.

- `jwt-signing-key.pem` — 2048-bit RSA **private** key (PKCS#8). The Rust test
  minter signs RS256 user tokens with it (`src/lib.rs`, `include_bytes!`).
- `jwks.json` — the matching **public** JWK set, served in-network by
  `jwks-server.mjs` at `HARNESS_AUTH_JWKS_URL`. Fixed `kid = e2e-slim-rsa-1`.

**Safe to commit — they grant no access to anything.** They authenticate only
against the throwaway `e2e-slim` compose stack, which itself has no real
credentials. **Never reuse these keys anywhere else**; regenerate with:

```bash
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out jwt-signing-key.pem
# then re-derive jwks.json (n,e as base64url) — see tests/e2e-slim/README.md
```
