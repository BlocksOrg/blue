variable "aws_region" { type = string }

variable "name" {
  type    = string
  default = "blue"
}

variable "environment" {
  type        = string
  default     = "production"
  description = "Deployment environment for this stack. With `name`, it builds the tags applied to every resource."
}

variable "bootstrap_admin_email" {
  type        = string
  description = "Initial Blue administrator email address"
}

# ---------------------------------------------------------------------------
# Container images
# ---------------------------------------------------------------------------
variable "image" {
  type        = string
  default     = "ghcr.io/blocksorg/governance-harness:latest"
  description = "Blue application image (control-api, dashboard, inference-proxy, migrate)"
}

variable "website_image" {
  type        = string
  default     = "ghcr.io/blocksorg/governance-harness-website:latest"
  description = "Separate landing-page (website) image"
}

variable "image_pull_secret_arn" {
  type        = string
  default     = ""
  description = "Secrets Manager ARN holding registry credentials for a private image. Empty means the images are public."
}

# ---------------------------------------------------------------------------
# VPC — create-or-reuse. Leave vpc_id empty to create a new VPC.
# ---------------------------------------------------------------------------
variable "vpc_id" {
  type        = string
  default     = ""
  description = "Reuse an existing VPC by id. Empty means create a new VPC."
}

variable "vpc_cidr" {
  type    = string
  default = "10.20.0.0/16"
}

variable "az_count" {
  type    = number
  default = 2
}

variable "single_nat_gateway" {
  type        = bool
  default     = true
  description = "Provision a single shared NAT gateway instead of one per AZ (created only when this module creates the VPC)."
}

variable "public_subnet_ids" {
  type        = list(string)
  default     = []
  description = "Public subnets for the ALB. Required when reusing a VPC (vpc_id set)."
}

variable "private_subnet_ids" {
  type        = list(string)
  default     = []
  description = "Private subnets for ECS tasks and datastores. Required when reusing a VPC (vpc_id set)."
}

# ---------------------------------------------------------------------------
# Domain / TLS — optional. Empty domain_name serves on the raw ALB DNS name.
# ---------------------------------------------------------------------------
variable "domain_name" {
  type        = string
  default     = ""
  description = "Apex domain served by the website/landing page. Empty means HTTP on the raw ALB DNS name."
}

variable "route53_zone_id" {
  type        = string
  default     = ""
  description = "Route53 hosted zone id for domain_name. Required when domain_name is set."
}

variable "app_subdomain" {
  type    = string
  default = "app"
}

variable "api_subdomain" {
  type    = string
  default = "api"
}

variable "inference_subdomain" {
  type    = string
  default = "inference"
}

variable "ingress_cidrs" {
  type        = list(string)
  default     = ["0.0.0.0/0"]
  description = "CIDRs allowed to reach the public ALB."
}

# ---------------------------------------------------------------------------
# Component toggles
# ---------------------------------------------------------------------------
variable "enable_website" {
  type    = bool
  default = true
}

variable "enable_redis" {
  type    = bool
  default = true
}

variable "enable_inference_proxy" {
  type    = bool
  default = false
}

# ---------------------------------------------------------------------------
# Blue application configuration
# ---------------------------------------------------------------------------
variable "blue_config_yaml" {
  type        = string
  default     = ""
  description = <<-EOT
    Contents of the Blue config file (BLUE_CONFIG_FILE) that the control-api and
    worker containers load at startup. Empty renders the bundled governance-only
    template (config/blue.yaml.tftpl), which enables a gateway block only when
    enable_inference_proxy is true. Override for production gateway provisioner
    wiring.
  EOT
}

variable "docs_url" {
  type    = string
  default = "https://docs.blocks.team"
}

variable "gateway_type" {
  type        = string
  default     = "litellm"
  description = "Gateway implementation used when enable_inference_proxy is true."
}

variable "inference_proxy_client_id" {
  type    = string
  default = "blue-inference-proxy"
}

variable "gateway_jwt_active_kid" {
  type        = string
  default     = "blue-gateway-1"
  description = "Key ID for the active gateway inference JWT signing key."
}

variable "gateway_jwt_private_key_pem" {
  type        = string
  sensitive   = true
  default     = ""
  description = "RSA private key PEM used by the control API to sign gateway inference JWTs. Required in gateway mode."
}

variable "gateway_jwt_jwks_json" {
  type        = string
  sensitive   = true
  default     = ""
  description = "Public JWKS containing the active gateway JWT key and any retained rotation keys. Required in gateway mode."
}

# The public CLI's OAuth client id. The dashboard seeds the client under this
# id and the control-api advertises it in its discovery document, so both
# containers are given the same value.
variable "oauth_client_id" {
  type    = string
  default = "blue-cli"
}

# ---------------------------------------------------------------------------
# Sizing (Fargate valid CPU/memory combinations)
# ---------------------------------------------------------------------------
variable "control_api_cpu" {
  type    = number
  default = 512
}
variable "control_api_memory" {
  type    = number
  default = 1024
}
variable "control_api_desired_count" {
  type    = number
  default = 2
}

variable "dashboard_cpu" {
  type    = number
  default = 256
}
variable "dashboard_memory" {
  type    = number
  default = 512
}
variable "dashboard_desired_count" {
  type    = number
  default = 2
}

variable "worker_cpu" {
  type    = number
  default = 256
}
variable "worker_memory" {
  type    = number
  default = 512
}

variable "website_cpu" {
  type    = number
  default = 256
}
variable "website_memory" {
  type    = number
  default = 512
}
variable "website_desired_count" {
  type    = number
  default = 2
}

variable "inference_proxy_cpu" {
  type    = number
  default = 1024
}
variable "inference_proxy_memory" {
  type    = number
  default = 2048
}
variable "inference_proxy_desired_count" {
  type    = number
  default = 2
}

variable "migrate_cpu" {
  type    = number
  default = 256
}
variable "migrate_memory" {
  type    = number
  default = 512
}

# ---------------------------------------------------------------------------
# Rolling-deployment / service-stability tuning
# ---------------------------------------------------------------------------
variable "health_check_grace_period_seconds" {
  type    = number
  default = 120
}
variable "deregistration_delay_seconds" {
  type    = number
  default = 120
}
variable "inference_deregistration_delay_seconds" {
  type    = number
  default = 600
}
variable "app_request_timeout_seconds" {
  type    = number
  default = 60
}
variable "alb_idle_timeout_seconds" {
  type    = number
  default = 120
}
variable "slow_start_seconds" {
  type    = number
  default = 30
}
variable "stop_timeout_seconds" {
  type        = number
  default     = 120
  description = "Container stopTimeout in seconds. Fargate caps this at 120."
}
variable "wait_for_steady_state" {
  type    = bool
  default = true
}
variable "log_retention_days" {
  type    = number
  default = 30
}

# ---------------------------------------------------------------------------
# Datastore sizing — reuse existing module defaults
# ---------------------------------------------------------------------------
variable "database_name" {
  type    = string
  default = "governance"
}
variable "database_username" {
  type    = string
  default = "harness"
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

variable "database_client_security_group_ids" {
  type        = set(string)
  default     = []
  description = "Additional security groups (beyond the ECS task SG) allowed to reach PostgreSQL."
}

variable "redis_client_security_group_ids" {
  type        = set(string)
  default     = []
  description = "Additional security groups (beyond the ECS task SG) allowed to reach Redis."
}

# ---------------------------------------------------------------------------
# GitHub Actions OIDC deploy role
# ---------------------------------------------------------------------------
variable "github_oidc_repository" {
  type        = string
  default     = ""
  description = "GitHub repository whose deploy workflow may assume the ECS deploy role, written exactly as the repository's OIDC subject prefix minus `repo:` — `gh api repos/OWNER/NAME/actions/oidc/customization/sub -q .sub_claim_prefix`. That is `owner/name`, or `owner@OWNER_ID/name@REPO_ID` for a repository on immutable subject claims. Empty disables the role."
}
