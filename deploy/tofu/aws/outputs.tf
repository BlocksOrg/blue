output "database_endpoint" { value = aws_db_instance.blue.endpoint }
output "runtime_secret_arn" { value = aws_secretsmanager_secret.runtime.arn }
output "redis_primary_endpoint" { value = aws_elasticache_replication_group.blue.primary_endpoint_address }
output "package_bucket" { value = aws_s3_bucket.packages.id }
output "session_bucket" { value = aws_s3_bucket.sessions.id }
output "kms_key_arn" { value = aws_kms_key.blue.arn }
output "workload_role_arn" { value = aws_iam_role.blue.arn }
output "helm_values" {
  value = {
    serviceAccount = {
      name        = var.kubernetes_service_account
      annotations = { "eks.amazonaws.com/role-arn" = aws_iam_role.blue.arn }
    }
    blue = { env = {
      HARNESS_BLOB_BUCKET    = aws_s3_bucket.sessions.id
      HARNESS_PACKAGE_BUCKET = aws_s3_bucket.packages.id
      HARNESS_BLOB_REGION    = var.aws_region
    } }
  }
}
