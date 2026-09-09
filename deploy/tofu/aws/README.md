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
