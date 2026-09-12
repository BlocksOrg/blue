locals {
  # Applied to every taggable resource through the provider's default_tags.
  tags = {
    Application = var.name
    Environment = var.environment
  }

  create_vpc    = var.vpc_id == ""
  enable_domain = var.domain_name != ""
  resource_name = substr(var.name, 0, 32)

  az_names = slice(data.aws_availability_zones.available.names, 0, var.az_count)

  # Create-or-reuse: downstream refs use these so the choice is transparent.
  vpc_id            = local.create_vpc ? aws_vpc.this[0].id : var.vpc_id
  public_subnet_ids = local.create_vpc ? aws_subnet.public[*].id : var.public_subnet_ids

  website_url = local.enable_domain ? "https://${var.domain_name}" : "http://${aws_lb.this.dns_name}"
}
