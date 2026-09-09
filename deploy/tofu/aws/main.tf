data "aws_eks_cluster" "this" { name = var.eks_cluster_name }

locals {
  oidc_issuer   = data.aws_eks_cluster.this.identity[0].oidc[0].issuer
  oidc_subject  = "system:serviceaccount:${var.kubernetes_namespace}:${var.kubernetes_service_account}"
  resource_name = substr(var.name, 0, 32)
}

data "aws_iam_openid_connect_provider" "eks" { url = local.oidc_issuer }

resource "aws_kms_key" "blue" {
  description             = "Blue deployment data"
  deletion_window_in_days = 30
  enable_key_rotation     = true
}
resource "aws_kms_alias" "blue" {
  name          = "alias/${var.name}"
  target_key_id = aws_kms_key.blue.key_id
}

resource "aws_s3_bucket" "packages" { bucket_prefix = "${var.name}-packages-" }
resource "aws_s3_bucket" "sessions" { bucket_prefix = "${var.name}-sessions-" }

resource "aws_s3_bucket_public_access_block" "blue" {
  for_each                = { packages = aws_s3_bucket.packages.id, sessions = aws_s3_bucket.sessions.id }
  bucket                  = each.value
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "blue" {
  for_each = { packages = aws_s3_bucket.packages.id, sessions = aws_s3_bucket.sessions.id }
  bucket   = each.value
  rule {
    apply_server_side_encryption_by_default {
      kms_master_key_id = aws_kms_key.blue.arn
      sse_algorithm     = "aws:kms"
    }
    bucket_key_enabled = true
  }
}

resource "aws_s3_bucket_versioning" "packages" {
  bucket = aws_s3_bucket.packages.id
  versioning_configuration { status = "Enabled" }
}
resource "aws_s3_bucket_versioning" "sessions" {
  bucket = aws_s3_bucket.sessions.id
  versioning_configuration { status = "Enabled" }
}

resource "aws_s3_bucket_lifecycle_configuration" "sessions" {
  bucket     = aws_s3_bucket.sessions.id
  depends_on = [aws_s3_bucket_versioning.sessions]
  rule {
    id     = "expire-sessions"
    status = "Enabled"
    filter {}
    expiration { days = var.session_retention_days }
    noncurrent_version_expiration { noncurrent_days = var.session_retention_days }
    abort_incomplete_multipart_upload { days_after_initiation = 7 }
  }
  rule {
    id     = "remove-expired-delete-markers"
    status = "Enabled"
    filter {}
    expiration { expired_object_delete_marker = true }
  }
}
resource "aws_s3_bucket_lifecycle_configuration" "packages" {
  bucket     = aws_s3_bucket.packages.id
  depends_on = [aws_s3_bucket_versioning.packages]
  rule {
    id     = "package-housekeeping"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration { noncurrent_days = var.package_noncurrent_retention_days }
    abort_incomplete_multipart_upload { days_after_initiation = 7 }
  }
}

resource "aws_db_subnet_group" "blue" {
  name       = var.name
  subnet_ids = var.private_subnet_ids
}
resource "aws_security_group" "database" {
  name_prefix = "${var.name}-database-"
  description = "PostgreSQL access from Blue workloads"
  vpc_id      = var.vpc_id
}
resource "aws_vpc_security_group_ingress_rule" "database" {
  for_each                     = var.database_client_security_group_ids
  security_group_id            = aws_security_group.database.id
  referenced_security_group_id = each.value
  from_port                    = 5432
  to_port                      = 5432
  ip_protocol                  = "tcp"
  description                  = "PostgreSQL from an approved Blue workload security group"
}

resource "aws_elasticache_subnet_group" "blue" {
  name       = var.name
  subnet_ids = var.private_subnet_ids
}
resource "aws_security_group" "redis" {
  name_prefix = "${var.name}-redis-"
  description = "Redis access from Blue workloads"
  vpc_id      = var.vpc_id
}
resource "aws_vpc_security_group_ingress_rule" "redis" {
  for_each                     = var.redis_client_security_group_ids
  security_group_id            = aws_security_group.redis.id
  referenced_security_group_id = each.value
  from_port                    = 6379
  to_port                      = 6379
  ip_protocol                  = "tcp"
  description                  = "Redis TLS from an approved Blue workload security group"
}

locals {
  # Generated secret contract: length of the bootstrap admin password. Kept as a
  # named local so it is a single source of truth and can be asserted by the CI
  # "Verify generated secret contracts" check (which cannot read managed-resource
  # attributes without state, but can evaluate a local).
  bootstrap_admin_password_length = 48
}

resource "random_password" "database" {
  length  = 32
  special = false
}
resource "random_password" "auth" {
  length  = 48
  special = false
}
resource "random_password" "bootstrap_admin" {
  length  = local.bootstrap_admin_password_length
  special = false
}
resource "random_password" "redis_auth" {
  length  = 48
  special = false
}

resource "aws_elasticache_replication_group" "blue" {
  replication_group_id       = local.resource_name
  description                = "Blue session and invalidation cache"
  node_type                  = var.redis_node_type
  port                       = 6379
  engine                     = "redis"
  engine_version             = "7.1"
  num_cache_clusters         = 2
  automatic_failover_enabled = true
  multi_az_enabled           = true
  at_rest_encryption_enabled = true
  kms_key_id                 = aws_kms_key.blue.arn
  transit_encryption_enabled = true
  auth_token                 = random_password.redis_auth.result
  subnet_group_name          = aws_elasticache_subnet_group.blue.name
  security_group_ids         = [aws_security_group.redis.id]
  snapshot_retention_limit   = var.redis_snapshot_retention_days
  apply_immediately          = false
}

data "aws_iam_policy_document" "rds_monitoring_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["monitoring.rds.amazonaws.com"]
    }
  }
}
resource "aws_iam_role" "rds_monitoring" {
  name_prefix        = "${var.name}-rds-monitoring-"
  assume_role_policy = data.aws_iam_policy_document.rds_monitoring_assume.json
}
resource "aws_iam_role_policy_attachment" "rds_monitoring" {
  role       = aws_iam_role.rds_monitoring.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonRDSEnhancedMonitoringRole"
}

resource "aws_db_instance" "blue" {
  identifier                          = local.resource_name
  engine                              = "postgres"
  engine_version                      = "16"
  instance_class                      = var.database_instance_class
  allocated_storage                   = var.database_allocated_storage
  storage_encrypted                   = true
  kms_key_id                          = aws_kms_key.blue.arn
  db_name                             = var.database_name
  username                            = var.database_username
  password                            = random_password.database.result
  db_subnet_group_name                = aws_db_subnet_group.blue.name
  vpc_security_group_ids              = [aws_security_group.database.id]
  backup_retention_period             = var.database_backup_retention_days
  copy_tags_to_snapshot               = true
  deletion_protection                 = var.deletion_protection
  skip_final_snapshot                 = !var.deletion_protection
  final_snapshot_identifier           = var.deletion_protection ? "${local.resource_name}-final" : null
  auto_minor_version_upgrade          = true
  publicly_accessible                 = false
  multi_az                            = true
  iam_database_authentication_enabled = true
  enabled_cloudwatch_logs_exports     = ["postgresql", "upgrade"]
  monitoring_interval                 = 60
  monitoring_role_arn                 = aws_iam_role.rds_monitoring.arn
  performance_insights_enabled        = true
  performance_insights_kms_key_id     = aws_kms_key.blue.arn
}

resource "aws_secretsmanager_secret" "runtime" {
  name_prefix = "${var.name}/runtime-"
  kms_key_id  = aws_kms_key.blue.arn
}
resource "aws_secretsmanager_secret_version" "runtime" {
  secret_id = aws_secretsmanager_secret.runtime.id
  secret_string = jsonencode({
    HARNESS_DATABASE_URL             = "postgres://${var.database_username}:${random_password.database.result}@${aws_db_instance.blue.address}:${aws_db_instance.blue.port}/${var.database_name}"
    BETTER_AUTH_SECRET               = random_password.auth.result
    HARNESS_BOOTSTRAP_ADMIN_EMAIL    = var.bootstrap_admin_email
    HARNESS_BOOTSTRAP_ADMIN_PASSWORD = random_password.bootstrap_admin.result
    HARNESS_REDIS_URL                = "rediss://default:${random_password.redis_auth.result}@${aws_elasticache_replication_group.blue.primary_endpoint_address}:6379/0"
  })
}

data "aws_iam_policy_document" "assume" {
  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [data.aws_iam_openid_connect_provider.eks.arn]
    }
    condition {
      test     = "StringEquals"
      variable = "${replace(local.oidc_issuer, "https://", "")}:sub"
      values   = [local.oidc_subject]
    }
    condition {
      test     = "StringEquals"
      variable = "${replace(local.oidc_issuer, "https://", "")}:aud"
      values   = ["sts.amazonaws.com"]
    }
  }
}
resource "aws_iam_role" "blue" {
  name_prefix        = "${var.name}-"
  assume_role_policy = data.aws_iam_policy_document.assume.json
}
data "aws_iam_policy_document" "blue" {
  statement {
    actions   = ["s3:ListBucket", "s3:GetBucketLocation"]
    resources = [aws_s3_bucket.packages.arn, aws_s3_bucket.sessions.arn]
  }
  statement {
    actions   = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"]
    resources = ["${aws_s3_bucket.packages.arn}/*", "${aws_s3_bucket.sessions.arn}/*"]
  }
  statement {
    actions   = ["kms:Decrypt", "kms:Encrypt", "kms:GenerateDataKey"]
    resources = [aws_kms_key.blue.arn]
  }
}
resource "aws_iam_role_policy" "blue" {
  role   = aws_iam_role.blue.id
  policy = data.aws_iam_policy_document.blue.json
}
