variable "aws_region" { type = string }
variable "name" {
  type    = string
  default = "blue"
}
variable "environment" {
  type        = string
  default     = "production"
  description = "Deployment environment for this stack. With `name`, it prefixes every resource name and builds the tags applied to every resource."
}
# ---------------------------------------------------------------------------
# Component toggles. Each dependency is optional so a deployment can source it
# elsewhere — the Helm chart bundles evaluation-grade PostgreSQL and MinIO, and
# Redis belongs to the organization-operated gateway rather than to Blue.
# ---------------------------------------------------------------------------
variable "include_database" {
  type        = bool
  default     = true
  description = "Provision RDS PostgreSQL and publish HARNESS_DATABASE_URL. False expects the chart's evaluation StatefulSet or another external database."
}
variable "include_redis" {
  type        = bool
  default     = true
  description = "Provision ElastiCache Redis and publish HARNESS_REDIS_URL. Only gateway mode consumes it; governance-only deployments should set this to false."
}
variable "include_bucket" {
  type        = bool
  default     = true
  description = "Provision the package and session S3 buckets and grant the workload role access to them. False expects the chart's evaluation MinIO or another S3-compatible store."
}
variable "eks_cluster_name" { type = string }
variable "vpc_id" { type = string }
variable "private_subnet_ids" { type = list(string) }
variable "database_client_security_group_ids" {
  type    = set(string)
  default = []
}
variable "redis_client_security_group_ids" {
  type    = set(string)
  default = []
}
variable "kubernetes_namespace" {
  type    = string
  default = "blue"
}
variable "kubernetes_service_account" {
  type    = string
  default = "blue"
}
variable "database_name" {
  type    = string
  default = "governance"
}
variable "database_username" {
  type    = string
  default = "harness"
}
variable "bootstrap_admin_email" {
  type        = string
  description = "Initial Blue administrator email address"
}
variable "database_instance_class" {
  type    = string
  default = "db.t4g.small"
}
variable "database_allocated_storage" {
  type    = number
  default = 20
}
variable "database_backup_retention_days" {
  type    = number
  default = 7
}
variable "redis_node_type" {
  type    = string
  default = "cache.t4g.small"
}
variable "redis_snapshot_retention_days" {
  type    = number
  default = 7
}
variable "session_retention_days" {
  type    = number
  default = 30
}
variable "package_noncurrent_retention_days" {
  type        = number
  default     = 90
  description = "Days to retain superseded package artifact versions"
}
variable "deletion_protection" {
  type    = bool
  default = true
}
