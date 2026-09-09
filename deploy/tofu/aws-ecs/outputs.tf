output "alb_dns_name" { value = aws_lb.this.dns_name }

output "dashboard_url" { value = local.dashboard_url }
output "control_api_url" { value = local.control_api_url }
output "inference_proxy_url" {
  value = local.enable_proxy ? local.inference_proxy_url : null
}

output "ecs_cluster_name" { value = aws_ecs_cluster.this.name }
output "ecs_cluster_arn" { value = aws_ecs_cluster.this.arn }

output "service_names" {
  value = concat(
    [aws_ecs_service.control_api.name, aws_ecs_service.worker.name, aws_ecs_service.dashboard.name],
    local.enable_proxy ? [aws_ecs_service.inference_proxy[0].name] : [],
  )
}

output "task_execution_role_arn" { value = aws_iam_role.execution.arn }
output "task_role_arn" { value = aws_iam_role.task.arn }
output "runtime_secret_arn" { value = aws_secretsmanager_secret.runtime.arn }

output "database_endpoint" { value = aws_db_instance.blue.endpoint }
output "redis_primary_endpoint" {
  value = var.enable_redis ? aws_elasticache_replication_group.blue[0].primary_endpoint_address : null
}

output "package_bucket" { value = aws_s3_bucket.packages.id }
output "session_bucket" { value = aws_s3_bucket.sessions.id }
output "kms_key_arn" { value = aws_kms_key.blue.arn }

output "vpc_id" { value = local.vpc_id }
output "private_subnet_ids" { value = local.private_subnet_ids }
output "public_subnet_ids" { value = local.public_subnet_ids }

output "acm_certificate_arn" {
  value = local.enable_domain ? aws_acm_certificate.this[0].arn : null
}
