# ECS/ALB deployment (Fargate)

This OpenTofu root module deploys the Blue backend to **ECS Fargate behind an
Application Load Balancer**, with **RDS PostgreSQL 16 (Multi-AZ)**, optional
**ElastiCache Redis 7.1**, versioned **S3** package/session buckets, a **KMS**
key, and a **Secrets Manager** runtime secret. It reuses the datastore/KMS/
secrets design of the sibling EKS module (`../aws/`), swapping IRSA for ECS task
roles.

It runs four public/back-end components from the single application image plus a
separate landing-page image:

| Component        | Command / image                | Port | Health       | Exposure           |
|------------------|--------------------------------|------|--------------|--------------------|
| control-api      | `control-api` (+ `migrate` init) | 8080 | `/ready`     | `api.<domain>`     |
| dashboard        | `dashboard`                    | 3000 | `/api/health`| `app.<domain>`     |
| website/landing  | `website_image` (nginx)        | 3000 | `/health`    | apex `<domain>`    |
| worker           | `control-api` (background jobs) | —    | container    | internal only      |
| inference-proxy* | `inference-proxy`              | 8081 | `/ready`     | `inference.<domain>`|

\* optional (`enable_inference_proxy`, default false).

## What it creates

- Optional VPC (create-or-reuse), public/private subnets per AZ, IGW, NAT.
- ALB, per-service target groups, and listeners (HTTPS host-routing with a
  domain; per-service HTTP ports without one).
- ECS cluster (Container Insights on) with a FARGATE capacity provider, task
  definitions, and services with a rolling-deployment stability profile
  (min 100% / max 200%, circuit breaker + rollback, health-check grace,
  `wait_for_steady_state`).
- Multi-AZ PostgreSQL, optional Multi-AZ Redis, two encrypted+versioned S3
  buckets with lifecycle housekeeping, a KMS key, and the runtime secret.
- Task **execution** role (image pull, logs, secret injection, KMS decrypt) and
  task role (S3 + KMS) — the ECS replacement for IRSA.

## Prerequisites

- OpenTofu 1.8+ and an **encrypted remote state backend** (generated database,
  auth, and admin credentials are stored in state — restrict access).
- Pullable images: `ghcr.io/blocksorg/governance-harness` (app) and, when
  `enable_website = true`, `ghcr.io/blocksorg/governance-harness-website`
  (landing page). Both are published by the repo's release workflow. For a
  private registry, set `image_pull_secret_arn`.
- A Route53 hosted zone if you use a domain.

## Usage

```bash
cp terraform.tfvars.example terraform.tfvars
tofu init
tofu plan -out=blue.tfplan
tofu apply blue.tfplan
```

## Routing

- **With a domain** (`domain_name` + `route53_zone_id`): an ACM certificate is
  issued and DNS-validated. `:443` serves the **landing page** at the apex, with
  host rules `app.<domain>` → dashboard, `api.<domain>` → control-api, and
  `inference.<domain>` → inference-proxy (when enabled). `:80` redirects to
  `:443`. If `enable_website = false`, the apex/default serves the dashboard.
- **Without a domain**: services are exposed over HTTP on separate ALB ports —
  `:80` landing page, `:3000` dashboard, `:8080` control-api, `:8081`
  inference-proxy. `control_api_url` becomes `http://<alb_dns>:8080`. The domain
  path is the recommended production configuration.

## Migrations

Database migrations run **automatically** as a non-essential `migrate` init
container inside the control-api task (`command = ["migrate"]`,
`dependsOn: SUCCESS`). ECS runs it to completion before starting the control-api
container — effectively `migrate && control-api` within the same image. The
worker task has no migrate container; the control-api task owns migrations.

**Concurrent-replica caveat:** with `control_api_desired_count > 1`, multiple
tasks may run `migrate` on a rollout. SQLx holds a database migration lock, so
runs serialize, but migrations must remain retry-safe and backward compatible
(see `../../runbooks/migrations.md`).

## Internal transport

The control-api internal `:8082` hop (used only by the inference proxy) is
reached via AWS Cloud Map private DNS (`control-api.<name>.internal:8082`) and is
restricted to the shared task security group — no public exposure and no second
ALB. It is provisioned only when `enable_inference_proxy = true` and uses
`insecure-http` over the private network (mTLS is a follow-up).

**inference-proxy drain caveat:** the proxy carries long-lived streams and its
target group uses a 600s deregistration delay, but Fargate caps container
`stopTimeout` at 120s. In-flight streams must therefore drain within 120s on a
deploy.

## Configuration

The control-api and worker containers load `BLUE_CONFIG_FILE` (`/etc/blue/
blue.yaml`). This module renders `config/blue.yaml.tftpl` and writes it into
those containers at startup (a gateway block is included only when
`enable_inference_proxy = true`). Supply your own config — for example a full
gateway provisioner block for production gateway mode — via the
`blue_config_yaml` variable.

Better Auth runs in the dashboard container, so the control-api's session and
JWKS lookups are server-to-server calls back to the dashboard. There is no
internal DNS record for it (the Cloud Map namespace is provisioned only with
the inference proxy), so those hops go out through NAT and back in via the
public ALB — the same path the inference proxy already uses for its OAuth token
hop. Narrowing `ingress_cidrs` to an office range therefore breaks the
control-api's own session checks, because the request arrives from the NAT
gateway's address.

`identity.mode` is fixed to `password`. An IdP-managed workspace needs a SCIM
bearer token plus the dashboard's `HARNESS_OIDC_*` provider settings, which
this module has no inputs for; supply the whole config via `blue_config_yaml`.

## Secrets in state

All generated credentials (database, auth, bootstrap admin, Redis auth, and —
in gateway mode — the proxy OAuth client secret and gateway encryption key) are
stored in the runtime Secrets Manager secret and in OpenTofu state. Use an
encrypted remote backend and restrict access.
