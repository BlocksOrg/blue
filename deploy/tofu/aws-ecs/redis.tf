# Redis is only used in gateway mode. Count-guarded on var.enable_redis.
resource "aws_elasticache_subnet_group" "blue" {
  count      = var.enable_redis ? 1 : 0
  name       = var.name
  subnet_ids = local.private_subnet_ids
}

resource "aws_elasticache_replication_group" "blue" {
  count                      = var.enable_redis ? 1 : 0
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
  subnet_group_name          = aws_elasticache_subnet_group.blue[0].name
  security_group_ids         = [aws_security_group.redis[0].id]
  snapshot_retention_limit   = var.redis_snapshot_retention_days
  apply_immediately          = false
}
