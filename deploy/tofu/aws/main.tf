data "aws_eks_cluster" "this" { name = var.eks_cluster_name }

data "aws_iam_openid_connect_provider" "eks" { url = local.oidc_issuer }

resource "aws_kms_key" "blue" {
  description             = "Blue deployment data"
  deletion_window_in_days = 30
  enable_key_rotation     = true
}
resource "aws_kms_alias" "blue" {
  name          = "alias/${local.name_prefix}"
  target_key_id = aws_kms_key.blue.key_id
}

# ---------------------------------------------------------------------------
# Object storage — var.include_bucket. The chart's bundled MinIO replaces this
# for evaluation clusters; production keeps S3.
# ---------------------------------------------------------------------------
resource "aws_s3_bucket" "packages" {
  count         = var.include_bucket ? 1 : 0
  bucket_prefix = "${local.bucket_name}-packages-"
}
resource "aws_s3_bucket" "sessions" {
  count         = var.include_bucket ? 1 : 0
  bucket_prefix = "${local.bucket_name}-sessions-"
}

resource "aws_s3_bucket_public_access_block" "blue" {
  for_each                = local.buckets
  bucket                  = each.value
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "blue" {
  for_each = local.buckets
  bucket   = each.value
  rule {
    apply_server_side_encryption_by_default {
      kms_master_key_id = aws_kms_key.blue.arn
      sse_algorithm     = "aws:kms"
    }
    bucket_key_enabled = true
  }
}

resource "aws_s3_bucket_versioning" "blue" {
  for_each = local.buckets
  bucket   = each.value
  versioning_configuration { status = "Enabled" }
}

resource "aws_s3_bucket_lifecycle_configuration" "sessions" {
  count      = var.include_bucket ? 1 : 0
  bucket     = aws_s3_bucket.sessions[0].id
  depends_on = [aws_s3_bucket_versioning.blue]
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
  count      = var.include_bucket ? 1 : 0
  bucket     = aws_s3_bucket.packages[0].id
  depends_on = [aws_s3_bucket_versioning.blue]
  rule {
    id     = "package-housekeeping"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration { noncurrent_days = var.package_noncurrent_retention_days }
    abort_incomplete_multipart_upload { days_after_initiation = 7 }
  }
}

# ---------------------------------------------------------------------------
# PostgreSQL — var.include_database. The chart's bundled StatefulSet replaces
# this for evaluation clusters; production validation rejects that, so leave
# this on for any deployment that sets blue.production=true.
# ---------------------------------------------------------------------------
resource "aws_db_subnet_group" "blue" {
  count      = var.include_database ? 1 : 0
  name       = local.name_prefix
  subnet_ids = var.private_subnet_ids
}
resource "aws_security_group" "database" {
  count       = var.include_database ? 1 : 0
  name_prefix = "${local.name_prefix}-database-"
  description = "PostgreSQL access from Blue workloads"
  vpc_id      = var.vpc_id
}
resource "aws_vpc_security_group_ingress_rule" "database" {
  for_each                     = var.include_database ? var.database_client_security_group_ids : []
  security_group_id            = aws_security_group.database[0].id
  referenced_security_group_id = each.value
  from_port                    = 5432
  to_port                      = 5432
  ip_protocol                  = "tcp"
  description                  = "PostgreSQL from an approved Blue workload security group"
}

# ---------------------------------------------------------------------------
# Redis — var.include_redis. Blue itself never reads HARNESS_REDIS_URL; it is
# published for the organization-operated LiteLLM gateway used in gateway mode.
# Governance-only deployments should set this to false.
# ---------------------------------------------------------------------------
resource "aws_elasticache_subnet_group" "blue" {
  count      = var.include_redis ? 1 : 0
  name       = local.name_prefix
  subnet_ids = var.private_subnet_ids
}
resource "aws_security_group" "redis" {
  count       = var.include_redis ? 1 : 0
  name_prefix = "${local.name_prefix}-redis-"
  description = "Redis access from Blue workloads"
  vpc_id      = var.vpc_id
}
resource "aws_vpc_security_group_ingress_rule" "redis" {
  for_each                     = var.include_redis ? var.redis_client_security_group_ids : []
  security_group_id            = aws_security_group.redis[0].id
  referenced_security_group_id = each.value
  from_port                    = 6379
  to_port                      = 6379
  ip_protocol                  = "tcp"
  description                  = "Redis TLS from an approved Blue workload security group"
}

resource "random_password" "database" {
  count   = var.include_database ? 1 : 0
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
  count   = var.include_redis ? 1 : 0
  length  = 48
  special = false
}

resource "aws_elasticache_replication_group" "blue" {
  count                      = var.include_redis ? 1 : 0
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
  auth_token                 = random_password.redis_auth[0].result
  subnet_group_name          = aws_elasticache_subnet_group.blue[0].name
  security_group_ids         = [aws_security_group.redis[0].id]
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
  count              = var.include_database ? 1 : 0
  name_prefix        = "${local.iam_name}-rds-monitoring-"
  assume_role_policy = data.aws_iam_policy_document.rds_monitoring_assume.json
}
resource "aws_iam_role_policy_attachment" "rds_monitoring" {
  count      = var.include_database ? 1 : 0
  role       = aws_iam_role.rds_monitoring[0].name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonRDSEnhancedMonitoringRole"
}

resource "aws_db_instance" "blue" {
  count                               = var.include_database ? 1 : 0
  identifier                          = local.resource_name
  engine                              = "postgres"
  engine_version                      = "16"
  instance_class                      = var.database_instance_class
  allocated_storage                   = var.database_allocated_storage
  storage_encrypted                   = true
  kms_key_id                          = aws_kms_key.blue.arn
  db_name                             = var.database_name
  username                            = var.database_username
  password                            = random_password.database[0].result
  db_subnet_group_name                = aws_db_subnet_group.blue[0].name
  vpc_security_group_ids              = [aws_security_group.database[0].id]
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
  monitoring_role_arn                 = aws_iam_role.rds_monitoring[0].arn
  performance_insights_enabled        = true
  performance_insights_kms_key_id     = aws_kms_key.blue.arn
}

resource "aws_secretsmanager_secret" "runtime" {
  name_prefix = "${local.name_prefix}/runtime-"
  kms_key_id  = aws_kms_key.blue.arn
}
resource "aws_secretsmanager_secret_version" "runtime" {
  secret_id     = aws_secretsmanager_secret.runtime.id
  secret_string = jsonencode(local.runtime_secret)
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
  name_prefix        = "${local.iam_name}-"
  assume_role_policy = data.aws_iam_policy_document.assume.json
}
data "aws_iam_policy_document" "blue" {
  # Without buckets the workload role carries KMS only, so both S3 statements
  # are dynamic: an empty `resources` list is rejected at plan time.
  dynamic "statement" {
    for_each = var.include_bucket ? [1] : []
    content {
      actions   = ["s3:ListBucket", "s3:GetBucketLocation"]
      resources = [aws_s3_bucket.packages[0].arn, aws_s3_bucket.sessions[0].arn]
    }
  }
  dynamic "statement" {
    for_each = var.include_bucket ? [1] : []
    content {
      actions   = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"]
      resources = ["${aws_s3_bucket.packages[0].arn}/*", "${aws_s3_bucket.sessions[0].arn}/*"]
    }
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
