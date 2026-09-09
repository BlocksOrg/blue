variable "aws_region" { type = string }
variable "name" {
  type    = string
  default = "blue"
}
variable "eks_cluster_name" { type = string }
variable "vpc_id" { type = string }
variable "private_subnet_ids" { type = list(string) }
variable "database_client_security_group_ids" { type = set(string) }
variable "redis_client_security_group_ids" { type = set(string) }
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
variable "tags" {
  type    = map(string)
  default = {}
}
