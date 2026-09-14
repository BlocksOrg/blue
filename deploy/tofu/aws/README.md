# AWS dependency starter

This OpenTofu root module builds everything Blue needs on AWS, or attaches to
the pieces you already run. It creates monitored Multi-AZ PostgreSQL 16,
TLS/authenticated Multi-AZ Redis, versioned package and session S3 buckets with
lifecycle housekeeping, a KMS key, a runtime secret, and an IRSA role scoped to
the Blue service account, and it can create the EKS cluster and VPC those sit
in. PostgreSQL and Redis retain automated recovery points; deletion protection
defaults on for PostgreSQL.

Prerequisites: OpenTofu 1.8+, AWS credentials, and an encrypted remote state
backend. Generated credentials are stored in state, so restrict state access.
Attaching to an existing cluster or VPC instead adds its own prerequisites —
see below.

## Cluster and network

Both are create-or-reuse, the same convention `deploy/tofu/aws-ecs` uses:

| Variable | Empty (default) | Set |
|---|---|---|
| `eks_cluster_name` | Creates `<name>-<environment>` in EKS Auto Mode, with its IAM OIDC provider | Attaches to that cluster, which must already have an IAM OIDC provider |
| `vpc_id` | Creates a VPC, public and private subnets across `az_count` AZs, an internet gateway, and NAT | Attaches to that VPC; `private_subnet_ids` and `public_subnet_ids` are then required |

So `tofu apply` with neither set goes from an empty account to a cluster running
Blue's dependencies. Set both to land in a platform cluster someone else
operates, which is why the name is a variable at all: an attached cluster was
named by whoever built it and rarely follows this module's `name`/`environment`
convention.

Compute is [EKS Auto Mode]: AWS provisions and patches the nodes and runs the
EBS CSI driver, the load balancer controller, CoreDNS, and the CNI, so nothing
here declares a node group or an addon. It costs a per-vCPU premium over
self-managed nodes in exchange for that. Auto Mode is active only when compute,
block storage, and elastic load balancing are enabled together; a cluster with a
subset of the three is an ordinary cluster with no nodes.

Nodes and the control plane ENIs live in the private subnets, reaching AWS
through NAT — one shared gateway unless `single_nat_gateway = false`. Public
subnets carry ingress load balancers only, and both subnet sets are tagged
`kubernetes.io/role/{elb,internal-elb}` so the load balancer controller can find
them. The API endpoint is reachable privately and, by default, publicly;
`cluster_endpoint_public_access_cidrs` should be narrowed to operator and CI
egress ranges, and `cluster_endpoint_public_access = false` closes it entirely
for operators who reach the VPC directly.

Kubernetes secrets are envelope-encrypted with this module's KMS key, so the
cluster role carries inline `kms:DescribeKey` and `kms:CreateGrant` on it — no
AWS managed policy grants that. Pods reach PostgreSQL and Redis from the cluster
security group, which the module grants automatically when it created the
cluster; an attached cluster supplies its own security groups through
`database_client_security_group_ids` and `redis_client_security_group_ids`.

Once applied, `tofu output -raw kubeconfig_command` prints the
`aws eks update-kubeconfig` invocation for whichever cluster the stack used.

One manual step remains on a created cluster: Auto Mode runs the EBS CSI driver
but ships no *default* StorageClass, and this module deploys no Kubernetes
objects. Anything asking for a PersistentVolumeClaim without naming a class —
including the chart's bundled evaluation PostgreSQL and MinIO — stays Pending
until one exists. Apply it once, after the cluster is up and before the chart:

```bash
kubectl apply -f - <<'YAML'
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: gp3
  annotations: { storageclass.kubernetes.io/is-default-class: "true" }
provisioner: ebs.csi.eks.amazonaws.com
volumeBindingMode: WaitForFirstConsumer
parameters: { type: gp3, encrypted: "true" }
YAML
```

A production stack keeps its data in RDS and S3, so it only needs this if
something else in the namespace claims a volume.

[EKS Auto Mode]: https://docs.aws.amazon.com/eks/latest/userguide/automode.html

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
| `include_domain` | `false` | An ACM certificate for the dashboard and API hostnames, its validation records, and (once `alb_hostname` is set) alias records pointing at the load balancer | DNS lives outside Route 53, or a certificate already exists |

Nothing in Blue reads `HARNESS_REDIS_URL`. It is published for the
organization-operated LiteLLM gateway that gateway mode talks to, which is why
governance-only deployments should set `include_redis = false` rather than pay
for a cluster no component connects to.

The runtime secret carries keys only for the components this module actually
created, so a missing dependency surfaces as a startup failure rather than a
connection to nowhere. `helm_values` mirrors the same choice back to the chart:
skipping the database or the buckets here sets the matching `deployStandalone`
to `true` there. The chart rejects both in production, so a production stack
keeps `include_database` and `include_bucket` on. `helm_values` also fills the
chart's `networkPolicy` CIDR lists with the VPC CIDR, since the load balancer,
RDS and Redis all live there, and leaves HTTPS egress open because S3 and STS
have no fixed range.

`include_domain` takes `route53_zone_id` plus two labels, `dashboard_subdomain`
and `api_subdomain`, relative to that zone (`app` and `api` on `example.com`
give `app.example.com` and `api.example.com`; an empty label means the apex).
The zone's name is read back, so nothing repeats the domain. The load balancer
only exists after the chart's Ingress is installed, so the records are a second
apply: `tofu apply -var alb_hostname=<ingress hostname>`. `certificate_arn`,
`dashboard_hostname` and `api_hostname` are output for the chart and
`IngressClassParams`.

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

## Gateway JWT signing keys

In gateway mode the Control API signs the tokens the inference proxy accepts.
Set `generate_gateway_jwt_key = true` and this module generates that RSA key and
stores it in its own secret, `gateway_jwt_secret_arn`. It is kept out of the
runtime secret because every pod loads that one, and only the Control API may
hold this key. You never write a public key or JWKS: the Control API works it
out from the private key.

Sync `gateway_jwt_secret_arn` to a Kubernetes Secret with its keys mapped one to
one (`signing-key.pem`, plus `previous-signing-key.pem` during a rotation). Name
it as `helm_values` says in `blue.inferenceJwt.secret`. This module does not turn
on gateway mode itself: the rest of the chart's gateway settings are still yours
to set.

To rotate the key:

1. Put a new version first, for example `gateway_jwt_key_versions = ["2", "1"]`,
   apply, and sync. Key 2 signs new tokens, and key 1 stays published so tokens
   it already signed keep working.
2. After the token lifetime has passed (12 hours by default), remove the old
   version, `["2"]`, apply, and sync again.

The private keys are stored in OpenTofu state, like the generated passwords.
Keep state encrypted and access-controlled.

For a self-contained deployment that runs Blue on **ECS Fargate behind an ALB**
(no Kubernetes), see the sibling [`../aws-ecs/`](../aws-ecs/README.md) module.
