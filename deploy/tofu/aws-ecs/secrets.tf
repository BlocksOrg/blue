resource "random_password" "database" {
  length  = 32
  special = false
}
resource "random_password" "auth" {
  length  = 48
  special = false
}
resource "random_password" "bootstrap_admin" {
  length  = 48
  special = false
}
resource "random_password" "redis_auth" {
  length  = 48
  special = false
}
resource "random_password" "proxy_oauth_client_secret" {
  count   = local.enable_proxy ? 1 : 0
  length  = 48
  special = false
}
# HARNESS_GATEWAY_ENCRYPTION_KEY must be a base64-encoded 32-byte key.
resource "random_id" "gateway_encryption" {
  count       = local.enable_proxy ? 1 : 0
  byte_length = 32
}

resource "aws_secretsmanager_secret" "runtime" {
  name_prefix = "${var.name}/runtime-"
  kms_key_id  = aws_kms_key.blue.arn
}

resource "aws_secretsmanager_secret_version" "runtime" {
  secret_id = aws_secretsmanager_secret.runtime.id
  secret_string = jsonencode(merge(
    {
      HARNESS_DATABASE_URL             = "postgres://${var.database_username}:${random_password.database.result}@${aws_db_instance.blue.address}:${aws_db_instance.blue.port}/${var.database_name}"
      BETTER_AUTH_SECRET               = random_password.auth.result
      HARNESS_BOOTSTRAP_ADMIN_EMAIL    = var.bootstrap_admin_email
      HARNESS_BOOTSTRAP_ADMIN_PASSWORD = random_password.bootstrap_admin.result
    },
    var.enable_redis ? {
      HARNESS_REDIS_URL = "rediss://default:${random_password.redis_auth.result}@${aws_elasticache_replication_group.blue[0].primary_endpoint_address}:6379/0"
    } : {},
    local.enable_proxy ? {
      HARNESS_PROXY_OAUTH_CLIENT_SECRET = random_password.proxy_oauth_client_secret[0].result
      HARNESS_GATEWAY_ENCRYPTION_KEY    = random_id.gateway_encryption[0].b64_std
    } : {},
  ))
}

locals {
  runtime_secret_arn = aws_secretsmanager_secret.runtime.arn

  # ECS has no "mount the whole secret" primitive; each JSON key is referenced
  # individually as "${arn}:KEY::".
  secret_ref = { for k in [
    "HARNESS_DATABASE_URL",
    "BETTER_AUTH_SECRET",
    "HARNESS_BOOTSTRAP_ADMIN_EMAIL",
    "HARNESS_BOOTSTRAP_ADMIN_PASSWORD",
    "HARNESS_REDIS_URL",
    "HARNESS_PROXY_OAUTH_CLIENT_SECRET",
    "HARNESS_GATEWAY_ENCRYPTION_KEY",
  ] : k => "${local.runtime_secret_arn}:${k}::" }
}
