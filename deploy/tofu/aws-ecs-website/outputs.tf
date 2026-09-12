output "alb_dns_name" { value = aws_lb.this.dns_name }
output "website_url" { value = local.website_url }

output "ecs_cluster_name" { value = aws_ecs_cluster.this.name }
output "ecs_cluster_arn" { value = aws_ecs_cluster.this.arn }

output "task_execution_role_arn" { value = aws_iam_role.execution.arn }
output "kms_key_arn" { value = aws_kms_key.blue.arn }

output "vpc_id" { value = local.vpc_id }
output "public_subnet_ids" { value = local.public_subnet_ids }

output "acm_certificate_arn" {
  value = local.enable_domain ? aws_acm_certificate.this[0].arn : null
}

# Repository variables for .github/workflows/website-deploy.yml.
output "github_actions_role_arn" {
  value       = local.enable_github_oidc ? aws_iam_role.github_actions[0].arn : null
  description = "AWS_ROLE_ARN for the GitHub Actions deploy workflow."
}

output "website_service_name" {
  value       = aws_ecs_service.website.name
  description = "ECS_SERVICE for the GitHub Actions deploy workflow (ECS_CLUSTER is ecs_cluster_name)."
}
