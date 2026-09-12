data "aws_availability_zones" "available" {
  state = "available"
}

data "aws_caller_identity" "current" {}

data "aws_partition" "current" {}

# Cross-field input validations. terraform_data keeps them compatible with
# OpenTofu 1.8 (variable-to-variable validation requires 1.9+).
resource "terraform_data" "validations" {
  lifecycle {
    precondition {
      condition     = var.domain_name == "" || var.route53_zone_id != ""
      error_message = "route53_zone_id is required when domain_name is set."
    }
    precondition {
      condition     = var.vpc_id == "" || length(var.public_subnet_ids) > 0
      error_message = "public_subnet_ids is required when reusing a VPC (vpc_id set)."
    }
  }
}
