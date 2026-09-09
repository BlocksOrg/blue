data "aws_availability_zones" "available" {
  state = "available"
}

data "aws_caller_identity" "current" {}

data "aws_region" "current" {}

# Cross-field input validations. terraform_data keeps them compatible with
# OpenTofu 1.8 (variable-to-variable validation requires 1.9+).
resource "terraform_data" "validations" {
  lifecycle {
    precondition {
      condition     = !var.enable_inference_proxy || var.enable_redis
      error_message = "enable_inference_proxy requires enable_redis = true (the proxy consumes Redis)."
    }
    precondition {
      condition     = var.domain_name == "" || var.route53_zone_id != ""
      error_message = "route53_zone_id is required when domain_name is set."
    }
    precondition {
      condition     = var.vpc_id == "" || (length(var.public_subnet_ids) > 0 && length(var.private_subnet_ids) > 0)
      error_message = "public_subnet_ids and private_subnet_ids are required when reusing a VPC (vpc_id set)."
    }
  }
}
