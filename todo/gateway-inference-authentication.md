# Gateway inference authentication

## Purpose

Replace durable gateway pseudocredentials with session-bound inference JWTs.
This pilot-blocking workstream authenticates inference and resolves the caller to
the server-only managed upstream credential. Coding-session creation, local Git
metadata, attribution headers, and request correlation belong to the dependent
coding-session attribution workstream.

## Contract

- Authenticated `GET /governance-config` requires the source OAuth JWT `sid` and
  an active matching Better Auth session.
- The Control API returns a runtime-only `gateway.token` signed with its
  dedicated RS256 key ring. Claims are `iss`, inference-proxy `aud`, the Blue
  user UUID as `sub`, `iat`, `exp`, `jti`, `blue_oauth_session_id`, and the
  `gateway:infer` scope.
- Each launch renews an active backing Better Auth session and receives a fixed inference JWT valid for at most 12 hours. Running harnesses do not extend that expiry.
- `/gateway/jwks` publishes the active public key and retained rotation keys.
- Contract version 3 and `gateway_inference_jwt` make this an immediate cutover;
  no legacy token wire or acceptance path remains.

## Resolution and revocation

- Store only `oauth_session_id`, Blue user UUID, source expiry, and revocation
  state in `gateway_auth_sessions`. Never store inference JWT text or its hash.
- The proxy validates JWT signature and claims before sending `user_id` and
  `blue_oauth_session_id` through the existing M2M-authenticated resolver.
- The resolver verifies the gateway auth record, active Blue user, and live
  Better Auth session before decrypting the managed upstream credential.
- Cache by OAuth session and user. Credential-version and session-revocation
  events evict matching entries.
- CLI logout revokes the current gateway session before the OAuth refresh grant.
  Admin revocation, suspension, removal, and Better Auth session deletion revoke
  matching gateway sessions through the same transactional database boundary.
- Dynamic production routing fails closed. Static credentials and token maps are
  available only when the resolver is absent for explicit local development.

## Verification

- JWT validation: valid, unknown/rotated key, issuer/audience mismatch, missing
  scope or session, malformed subject UUID, expiry, and signature tampering.
- Session binding: user mismatch, source expiry/deletion, CLI logout, admin
  revocation, suspension/removal, and concurrent cache eviction.
- Contract: `token` replaces the old field; older clients fail negotiation; no
  durable client token columns or resolver inputs remain; managed upstream
  credentials survive migration.
- Gateway: a valid JWT swaps the upstream credential; rejected or revoked JWTs
  never reach upstream; neither JWT nor gateway credential appears in logs.
