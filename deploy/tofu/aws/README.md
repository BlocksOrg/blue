# AWS dependency starter

This OpenTofu root module attaches to an existing EKS cluster and VPC. It
creates monitored Multi-AZ PostgreSQL 16, TLS/authenticated Multi-AZ Redis,
versioned package and session S3 buckets with lifecycle housekeeping, a KMS
key, a runtime secret, and an IRSA role scoped to the Blue service account.
PostgreSQL and Redis retain automated recovery points; deletion protection
defaults on for PostgreSQL.

Prerequisites: OpenTofu 1.8+, an existing EKS cluster with an IAM OIDC provider,
private subnets, workload security groups, and an encrypted remote state
backend. Generated credentials are stored in state, so restrict state access.

## Optional components

The database and object-store toggles let the Helm chart deploy its bundled
evaluation components instead of this module's AWS resources (or use
dependencies managed elsewhere). Redis can be omitted in governance-only
deployments:

| Variable | Default | Creates | Turn it off when |
|---|---|---|---|
| `include_database` | `true` | RDS PostgreSQL, its subnet group, security group, monitoring role, and `HARNESS_DATABASE_URL` | The chart renders its evaluation StatefulSet (`database.deployStandalone`), or another database already exists |
| `include_bucket` | `true` | Package and session buckets, their encryption/versioning/lifecycle rules, and the workload role's S3 grants | The chart renders its evaluation MinIO (`minio.deployStandalone`), or another S3-compatible store already exists |
| `include_redis` | `true` | ElastiCache Redis and `HARNESS_REDIS_URL` | Governance-only deployments — see below |

Nothing in Blue reads `HARNESS_REDIS_URL`. It is published for the
organization-operated LiteLLM gateway that gateway mode talks to, which is why
governance-only deployments should set `include_redis = false` rather than pay
for a cluster no component connects to.

The runtime secret carries keys only for the components this module actually
created, so a missing dependency surfaces as a startup failure rather than a
connection to nowhere. `helm_values` mirrors the same choice back to the chart:
skipping the database or the buckets here sets the matching `deployStandalone`
to `true` there. The chart rejects both in production, so a production stack
keeps `include_database` and `include_bucket` on.

## Naming and tags

`name` and `environment` together prefix every resource this module creates
(`blue-production-...`) and build the `Application` / `Environment` tags applied
to every taggable resource through the provider's `default_tags`. Two
environments can therefore share an AWS account without colliding, and there is
no `tags` variable to keep in sync. Identifiers that AWS caps below the prefix
length (the RDS instance, the ElastiCache group, the S3 bucket prefixes, the IAM
roles) are truncated and suffixed with a hash of the full prefix, so they stay
unique; see `locals.tf`.

Changing either variable renames the resources and replaces the stateful ones.
Review `tofu plan` before applying it to a deployment that already exists.

```bash
cp terraform.tfvars.example terraform.tfvars
tofu init
tofu plan -out=blue.tfplan
tofu apply blue.tfplan
tofu output -json helm_values > generated-values.json
```

Sync the JSON object at `runtime_secret_arn` to a Kubernetes Secret named by
`blue.existingSecret`, using External Secrets or your existing secret delivery
system. Do not commit the secret value or rendered Kubernetes Secret.

For a self-contained deployment that runs Blue on **ECS Fargate behind an ALB**
(no Kubernetes), see the sibling [`../aws-ecs/`](../aws-ecs/README.md) module.
