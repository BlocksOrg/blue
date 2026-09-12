output "database_endpoint" { value = var.include_database ? aws_db_instance.blue[0].endpoint : null }
output "runtime_secret_arn" { value = aws_secretsmanager_secret.runtime.arn }
output "redis_primary_endpoint" { value = var.include_redis ? aws_elasticache_replication_group.blue[0].primary_endpoint_address : null }
output "package_bucket" { value = var.include_bucket ? aws_s3_bucket.packages[0].id : null }
output "session_bucket" { value = var.include_bucket ? aws_s3_bucket.sessions[0].id : null }
output "kms_key_arn" { value = aws_kms_key.blue.arn }
output "workload_role_arn" { value = aws_iam_role.blue.arn }
output "helm_values" {
  value = {
    serviceAccount = {
      name        = var.kubernetes_service_account
      annotations = { "eks.amazonaws.com/role-arn" = aws_iam_role.blue.arn }
    }
    blue = { env = var.include_bucket ? {
      HARNESS_BLOB_BUCKET    = aws_s3_bucket.sessions[0].id
      HARNESS_PACKAGE_BUCKET = aws_s3_bucket.packages[0].id
      HARNESS_BLOB_REGION    = var.aws_region
    } : {} }
    # The chart's bundled datastores are evaluation-only; a module that skipped
    # one is telling the chart to render it.
    database = { deployStandalone = !var.include_database }
    minio    = { deployStandalone = !var.include_bucket }
  }
}
