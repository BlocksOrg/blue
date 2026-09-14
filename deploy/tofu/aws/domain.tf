# Domain — count-guarded on var.include_domain. The zone's name is read back so
# the two labels stay relative to whatever zone was given, and the certificate
# is issued in the provider region, which is where the ALB will terminate TLS.

data "aws_route53_zone" "blue" {
  count   = var.include_domain ? 1 : 0
  zone_id = var.route53_zone_id

  lifecycle {
    precondition {
      condition     = var.route53_zone_id != ""
      error_message = "include_domain needs route53_zone_id: the hosted zone the hostnames live in."
    }
    precondition {
      condition     = var.dashboard_subdomain != var.api_subdomain
      error_message = "dashboard_subdomain and api_subdomain must differ; both cannot be the same name."
    }
  }
}

locals {
  zone_name          = var.include_domain ? trimsuffix(data.aws_route53_zone.blue[0].name, ".") : ""
  dashboard_hostname = var.include_domain ? (var.dashboard_subdomain == "" ? local.zone_name : "${var.dashboard_subdomain}.${local.zone_name}") : null
  api_hostname       = var.include_domain ? (var.api_subdomain == "" ? local.zone_name : "${var.api_subdomain}.${local.zone_name}") : null
}

resource "aws_acm_certificate" "blue" {
  count                     = var.include_domain ? 1 : 0
  domain_name               = local.dashboard_hostname
  subject_alternative_names = [local.api_hostname]
  validation_method         = "DNS"

  lifecycle { create_before_destroy = true }
}

# One validation CNAME per hostname. Keyed by hostname so the keys are known at
# plan time even though the record values are not.
resource "aws_route53_record" "validation" {
  for_each = var.include_domain ? {
    for option in aws_acm_certificate.blue[0].domain_validation_options :
    option.domain_name => option
  } : {}

  zone_id         = var.route53_zone_id
  name            = each.value.resource_record_name
  type            = each.value.resource_record_type
  ttl             = 60
  records         = [each.value.resource_record_value]
  allow_overwrite = true
}

# Blocks until ACM has seen the records, so certificate_arn is usable the
# moment it is output.
resource "aws_acm_certificate_validation" "blue" {
  count                   = var.include_domain ? 1 : 0
  certificate_arn         = aws_acm_certificate.blue[0].arn
  validation_record_fqdns = [for record in aws_route53_record.validation : record.fqdn]
}

# Alias records work at the zone apex too, unlike CNAMEs, so an empty
# dashboard_subdomain is fine. The ALB's own hosted zone id is per region.
data "aws_elb_hosted_zone_id" "alb" {
  count = var.include_domain && var.alb_hostname != "" ? 1 : 0
}

resource "aws_route53_record" "blue" {
  #checkov:skip=CKV2_AWS_23:the alias target is var.alb_hostname, an ALB created outside this module by the AWS Load Balancer Controller; checkov renders the variable to its "" default and so misses the check's own var. escape hatch
  for_each = var.include_domain && var.alb_hostname != "" ? {
    dashboard = local.dashboard_hostname
    api       = local.api_hostname
  } : {}

  zone_id = var.route53_zone_id
  name    = each.value
  type    = "A"

  alias {
    name                   = var.alb_hostname
    zone_id                = data.aws_elb_hosted_zone_id.alb[0].id
    evaluate_target_health = false
  }
}
