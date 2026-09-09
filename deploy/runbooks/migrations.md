# Database migration policy

Every migration PR must label each change in its description:

- **Expand** adds compatible nullable columns, tables, or indexes. Old and new
  applications can run concurrently.
- **Migrate** moves data. If it cannot finish inside the ten-minute schema Job,
  ship a separately observable, restartable, idempotent operation with a
  persisted cursor.
- **Contract** removes or tightens a shape. Ship only after the oldest supported
  application no longer reads it and after a tested backup. Applying it marks
  the release non-reversible without database restore.

SQLx migrations are append-only. Never edit an applied file: serving replicas
compare each embedded version/checksum with `_sqlx_migrations`. Record every new
migration in `services/control-api/migration-classifications.json`. Contract
migrations must update `public.schema_compatibility.minimum_migration_version`
to their own version; CI enforces that rule. Expand and data-only migrations do
not raise the floor, so the prior application can remain ready and roll back.
Migration SQL must be retry-safe, avoid unbounded locks, and state expected
lock/run time in the PR. The Helm hook holds SQLx's database migration lock, has
a ten-minute deadline, retries once, and fails the release before replica
rollout.

## ECS/Fargate deployments

The `deploy/tofu/aws-ecs/` module runs migrations as a non-essential `migrate`
init container inside the control-api task (`command = ["migrate"]`), and the
control-api container declares `dependsOn: { migrate: SUCCESS }`. ECS runs the
migrate container to completion before starting control-api — effectively
`migrate && control-api` within the same image, on every deployment. The worker
task carries no migrate container; the control-api task owns migrations.

With more than one control-api replica, a rollout can start `migrate` in several
tasks at once. SQLx's advisory migration lock serializes them (later runners
observe an up-to-date schema and no-op), so the same append-only, retry-safe,
backward-compatible rules above still apply — do not rely on exactly one runner.
