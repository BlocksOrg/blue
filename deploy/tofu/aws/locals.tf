locals {
  # Applied to every taggable resource through the provider's default_tags.
  tags = {
    Application = var.name
    Environment = var.environment
  }

  # Every physical name this module creates starts with this prefix, so two
  # environments can be applied into the same account without colliding.
  name_prefix = "${var.name}-${var.environment}"

  # Several AWS identifiers are capped below what the prefix can grow to. This
  # mirrors `truncate()` in orchestration-svc/infra/utils: over the limit, keep
  # the head and end in the md5 tail so two long prefixes stay distinct.
  name_hash = substr(md5(local.name_prefix), 24, 8)

  # RDS instance identifier and ElastiCache replication group id (40 chars;
  # 32 leaves room for the "-final" snapshot suffix).
  resource_name = length(local.name_prefix) > 32 ? "${substr(local.name_prefix, 0, 24)}${local.name_hash}" : local.name_prefix

  # S3 `bucket_prefix` caps at 37 characters, including the "-packages-" suffix.
  bucket_name = length(local.name_prefix) > 26 ? "${substr(local.name_prefix, 0, 18)}${local.name_hash}" : local.name_prefix

  # IAM `name_prefix` caps at 38 characters, including "-rds-monitoring-".
  iam_name = length(local.name_prefix) > 22 ? "${substr(local.name_prefix, 0, 14)}${local.name_hash}" : local.name_prefix

  oidc_issuer  = data.aws_eks_cluster.this.identity[0].oidc[0].issuer
  oidc_subject = "system:serviceaccount:${var.kubernetes_namespace}:${var.kubernetes_service_account}"

  # Bucket id per logical name, empty when var.include_bucket is false. Drives
  # the per-bucket policy/encryption/versioning resources in one place.
  buckets = var.include_bucket ? {
    packages = aws_s3_bucket.packages[0].id
    sessions = aws_s3_bucket.sessions[0].id
  } : {}

  # Runtime secret contract consumed by the chart's `blue.existingSecret`. Keys
  # for components this module did not create are omitted rather than left
  # empty, so a missing dependency fails loudly instead of connecting nowhere.
  runtime_secret = merge(
    {
      BETTER_AUTH_SECRET               = random_password.auth.result
      HARNESS_BOOTSTRAP_ADMIN_EMAIL    = var.bootstrap_admin_email
      HARNESS_BOOTSTRAP_ADMIN_PASSWORD = random_password.bootstrap_admin.result
    },
    var.include_database ? {
      HARNESS_DATABASE_URL = "postgres://${var.database_username}:${random_password.database[0].result}@${aws_db_instance.blue[0].address}:${aws_db_instance.blue[0].port}/${var.database_name}"
    } : {},
    var.include_redis ? {
      HARNESS_REDIS_URL = "rediss://default:${random_password.redis_auth[0].result}@${aws_elasticache_replication_group.blue[0].primary_endpoint_address}:6379/0"
    } : {},
  )

  # Generated secret contract: length of the bootstrap admin password. Kept as a
  # named local so it is a single source of truth and can be asserted by the CI
  # "Verify generated secret contracts" check (which cannot read managed-resource
  # attributes without state, but can evaluate a local).
  bootstrap_admin_password_length = 48
}
