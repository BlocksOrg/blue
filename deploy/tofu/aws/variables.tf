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
# ---------------------------------------------------------------------------
# Cluster and network — create-or-reuse, mirroring deploy/tofu/aws-ecs. An empty
# eks_cluster_name or vpc_id means this module creates it; setting either
# attaches to infrastructure someone else owns and named.
# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# Domain — var.include_domain. Names Blue on a Route 53 hosted zone you already
# own: one ACM certificate covering both hostnames (validated with records in
# that zone) and, once the load balancer exists, the records that point at it.
# ---------------------------------------------------------------------------
variable "include_domain" {
  type        = bool
  default     = false
  description = "Issue an ACM certificate for the dashboard and API hostnames and manage their Route 53 records. Requires route53_zone_id."
}
variable "route53_zone_id" {
  type        = string
  default     = ""
  description = "Hosted zone the hostnames live in. Its name is read back, so subdomains are relative to it."
}
variable "dashboard_subdomain" {
  type        = string
  default     = ""
  description = "Dashboard label relative to the zone (\"app\" on example.com gives app.example.com). Empty means the zone apex."
}
variable "api_subdomain" {
  type        = string
  default     = "api"
  description = "Control API label relative to the zone. Empty means the zone apex; it must differ from dashboard_subdomain."
}
variable "alb_hostname" {
  type        = string
  default     = ""
  description = "DNS name of the load balancer the chart's Ingress created. Empty until it exists; set it on a second apply to create the records."
}

variable "eks_cluster_name" {
  type        = string
  default     = ""
  description = "Attach to an existing EKS cluster by name. Empty creates \"<name>-<environment>\" in EKS Auto Mode."
}
variable "kubernetes_version" {
  type        = string
  default     = "1.34"
  description = "Kubernetes minor version for a cluster this module creates."
}
variable "cluster_node_pools" {
  type        = list(string)
  default     = ["general-purpose"]
  description = "Auto Mode node pools. \"system\" adds a pool tainted for critical addons; the default pool alone is enough for Blue."
}
variable "cluster_endpoint_public_access" {
  type        = bool
  default     = true
  description = "Expose the Kubernetes API endpoint publicly. Private access is always on; turn this off only where operators and CI reach the VPC directly."
}
variable "cluster_endpoint_public_access_cidrs" {
  type        = list(string)
  default     = ["0.0.0.0/0"]
  description = "CIDRs allowed to reach the public API endpoint. Narrow this to operator and CI egress ranges."
}
variable "vpc_id" {
  type        = string
  default     = ""
  description = "Reuse an existing VPC by id. Empty means create a new VPC."
}
variable "vpc_cidr" {
  type    = string
  default = "10.30.0.0/16"
}
variable "az_count" {
  type        = number
  default     = 2
  description = "Availability zones to spread subnets across. EKS requires at least two."
}
variable "single_nat_gateway" {
  type        = bool
  default     = true
  description = "Provision a single shared NAT gateway instead of one per AZ (created only when this module creates the VPC)."
}
variable "public_subnet_ids" {
  type        = list(string)
  default     = []
  description = "Public subnets for ingress load balancers. Required when reusing a VPC (vpc_id set)."
}
variable "private_subnet_ids" {
  type        = list(string)
  default     = []
  description = "Private subnets for nodes and datastores. Required when reusing a VPC (vpc_id set)."
}
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
