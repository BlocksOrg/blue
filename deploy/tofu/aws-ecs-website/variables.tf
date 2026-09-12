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

# ---------------------------------------------------------------------------
# Container image
# ---------------------------------------------------------------------------
variable "website_image" {
  type        = string
  default     = "ghcr.io/blocksorg/blue-website:latest"
  description = "Landing-page (website) image"
}

variable "image_pull_secret_arn" {
  type        = string
  default     = ""
  description = "Secrets Manager ARN holding registry credentials for a private image. Empty means the image is public."
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

variable "public_subnet_ids" {
  type        = list(string)
  default     = []
  description = "Public subnets for the ALB and website tasks. Required when reusing a VPC (vpc_id set)."
}

# ---------------------------------------------------------------------------
# Domain / TLS — optional. Empty domain_name serves on the raw ALB DNS name.
# ---------------------------------------------------------------------------
variable "domain_name" {
  type        = string
  default     = ""
  description = "Domain served by the website. Empty means HTTP on the raw ALB DNS name."
}

variable "route53_zone_id" {
  type        = string
  default     = ""
  description = "Route53 hosted zone id for domain_name. Required when domain_name is set."
}

variable "ingress_cidrs" {
  type        = list(string)
  default     = ["0.0.0.0/0"]
  description = "CIDRs allowed to reach the public ALB."
}

# ---------------------------------------------------------------------------
# Sizing (Fargate valid CPU/memory combinations)
# ---------------------------------------------------------------------------
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
# GitHub Actions OIDC deploy role
# ---------------------------------------------------------------------------
variable "github_oidc_repository" {
  type        = string
  default     = ""
  description = "GitHub repository whose deploy workflow may assume the ECS deploy role, written exactly as the repository's OIDC subject prefix minus `repo:` — `gh api repos/OWNER/NAME/actions/oidc/customization/sub -q .sub_claim_prefix`. That is `owner/name`, or `owner@OWNER_ID/name@REPO_ID` for a repository on immutable subject claims. Empty disables the role."
}
