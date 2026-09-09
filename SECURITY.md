# Security & trust model

Blue governs local coding agents by **configuration**, not by
sandboxing. It does not contain or confine what an agent can do on the machine;
it centralizes *which* agent may run and *how* it is configured.

## What the client holds

- A short-lived, audience-bound **OAuth access token** and rotating refresh
  token (`~/.config/blue/session.json`, 0600). `blue logout` revokes the
  refresh family when reachable and always deletes the local token file.
- In gateway mode, a **pseudotoken** written into agent config/env. The
  pseudotoken is an opaque, per-user, durable-but-revocable stand-in. It is only
  meaningful to the inference proxy, which maps it to a LiteLLM virtual key. The
  real provider key and the virtual key both stay server-side. Treat the
  pseudotoken as a secret: short-ish TTL, rotation, 0600 on disk.

All managed files are written atomically and, when they may carry secrets,
restricted to owner-only permissions.

In gateway mode, the inference proxy authenticates to the Control API's internal
resolver with a short-lived **OAuth2 client-credentials** token minted by
better-auth (previously a static shared secret); the Control API verifies it
against the better-auth JWKS (signature, issuer, audience, expiry) and a
`gateway:resolve` scope, so it holds no proxy secret at rest.
Production deployments additionally protect the decrypted credential response
with mutually authenticated TLS on the private Control API listener. The Helm
NetworkPolicy permits that listener only from inference-proxy pods.

An explicit `insecure-http` mode exists for trusted private networks. It keeps
OAuth M2M authorization and NetworkPolicy isolation but removes encryption and
client-certificate authentication, so decrypted virtual keys travel in
plaintext. It is never an automatic fallback. An API gateway at cluster ingress
does not secure this internal east-west hop.

Raw-session capture is disabled by default. Enabling a harness's
`session_upload` block sends prompts, responses, tool inputs/results, file paths,
and any secrets present in its native transcript to operator-controlled blob
storage. The client authenticates only to the presign endpoint; storage access
uses a short-lived returned URL, so durable cloud credentials never reach the
developer machine. After upload, the Control API verifies object size and
SHA-256 metadata before registering the artifact in PostgreSQL. Dashboard
visibility is restricted to the owning user or an administrator in the same
organization. Operators should disclose collection and retention, scope access
narrowly, encrypt storage, and treat uploaded sessions as sensitive source data.

## Trust boundaries

- **Config trust.** The service is the source of truth. For self-hosted config
  integrity, an operator may enable signature verification of the config bundle
  (planned). Point the client only at a service you control.
- **Enforcement.** Claude's `managed-settings.json` is only *truly*
  un-overridable with an OS-level managed/MDM profile. Without one it is
  best-effort (a determined local user can edit it). Document your enforced-vs-
  best-effort posture.
- **Attribution proxy (fast-follow).** The `X-Harness-*` headers must be injected
  **only when forwarding to a trusted upstream** (your org gateway/collector),
  never when an agent is pointed directly at a raw provider — the headers carry
  local paths / repo identity. The loopback hop must correctly re-originate TLS.

## Reporting a vulnerability

Please report suspected vulnerabilities privately to the maintainers rather than
opening a public issue. Include a description, reproduction steps, and impact.
We aim to acknowledge within a few business days.
