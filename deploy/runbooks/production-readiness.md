# Production readiness runbook

This is the pilot admission checklist for Blue. Record the operator, release
digest, timestamps, and evidence links for every exercise. Do not admit a pilot
with an expired security exception or a missed recovery budget.

## Release invariants

- `blue.production=true`, `blue.existingSecret` names an externally managed
  Secret, and `image.digest` is the reviewed release digest.
- The runtime Secret contains `HARNESS_DATABASE_URL`, `BETTER_AUTH_SECRET`, and
  `HARNESS_BOOTSTRAP_ADMIN_PASSWORD`. Gateway mode also requires its upstream,
  OAuth client secret and encryption material; Better Auth signing/JWKS state
  remains persistent in PostgreSQL.
- The mounted governance baseline sets `required: true` and leaves native agent
  approval prompts enabled unless the organization explicitly approved an
  auto-approval policy. Gateway and session capture sections are present only
  when those modes have been deliberately enabled.
- The migration Job succeeds before Deployments change. Application pods verify
  their embedded migration prefix and compatibility floor. They accept a newer
  expand-only suffix, but fail readiness on an absent, incomplete, checksum-
  mismatched, or contract-incompatible migration.
- The worker has exactly one replica. It retries revocations from the Postgres
  outbox with bounded backoff and reports freshness at `/health/worker`.
- Default-deny ingress and egress is enabled. Dependency CIDRs and ingress or
  monitoring selectors have been narrowed to this cluster.

Run `scripts/verify-deployment.sh` before every release. Scanner exceptions are
declared in `.github/security-exceptions.yml` with an owner, rationale,
compensating control, and expiry.

## Upgrade and rollback

1. Confirm the latest automated database snapshot and S3 versioning. Take a
   manual pre-upgrade database snapshot and record its identifier.
2. Classify every migration using `deploy/runbooks/migrations.md`. Stop if the
   release contains a contract migration needed by the serving version.
3. Archive rendered manifests, redacted values, and the image digest. Run
   `helm upgrade --install --atomic --timeout 15m`.
4. Watch `blue-*-migrate-*`. A failed/timed-out hook fails before Deployments
   roll, leaving the previous replicas serving.
5. Check `/health`, `/health/schema`, `/ready`, `/health/dependencies`,
   `/health/object-storage`, `/health/credential-resolver`, the worker's
   `/health/worker`, and proxy `/health` when enabled.
6. Roll back the image only while all intervening migrations are expand-safe.
   After a contract migration, restore the pre-upgrade snapshot before running
   the old image; this is the point of non-reversibility.

## Restore exercises

Restore a database snapshot into an isolated instance, run the target release's
schema check, compare organization/user/revision counts, and perform a read-only
governance fetch. Pilot recovery budget: 60 minutes.

Recover a deleted package and session object by S3 version ID, verify its stored
SHA-256, and download it through Blue. Pilot recovery budget: 30 minutes. Never
test a restore over the active production database or bucket.

Session-bucket current and noncurrent versions expire after the configured
retention period; delete markers and abandoned multipart uploads are cleaned up.
Perform restore exercises within that window.

## Rotation and revocation exercises

- Better Auth/OAuth: add the new verification key where overlap is supported,
  switch the signer, verify new and existing sessions, wait out the overlap, and
  remove the old key. Rotate the bootstrap password independently. Budget: 15
  minutes with no failed governance fetch.
- Gateway credentials: rotate a test user's upstream credential, verify cache
  invalidation, and confirm the previous credential is rejected. Budget: 5
  minutes.
- User revocation: suspend a test user and prove dashboard, CLI OAuth, refresh,
  and gateway access fail. Revocation-latency budget: 2 minutes.

Never print tokens or credentials in exercise logs.

## Dependency outage exercises

- Redis loss: gateway resolution uses the authenticated Control API/Postgres
  fallback and repopulates after Redis returns. Governance-only is unaffected.
  Budget: no outage; alert within 2 minutes.
- Upstream gateway loss: proxy alerts identify the component, requests fail
  without leaking prompts or credentials, and governance remains available.
  Alert within 2 minutes; recover within 15 minutes of upstream recovery.
- Object-store loss: configuration reads remain available; artifact operations
  report dependency failure. Alert within 2 minutes.

Capture actual detection, revocation, and recovery times and the affected
tenant/component. Never include tokens, credentials, prompts, or session content.

Run each exercise command through `scripts/run-readiness-exercise.sh` using the
budget above and a shared evidence directory. The wrapper records only timing,
status, operator, release digest, and operator-supplied evidence links; it never
records the command or environment. The required evidence names are enforced by
`scripts/verify-readiness-evidence.sh`. Until cloud CI identity exists, a human
operator must run the full suite in production-shaped staging and attach the
verified evidence to the pilot admission record.
