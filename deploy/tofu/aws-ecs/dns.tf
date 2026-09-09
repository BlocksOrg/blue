# Optional ACM certificate + Route53 records. Guarded on local.enable_domain.

resource "aws_acm_certificate" "this" {
  count                     = local.enable_domain ? 1 : 0
  domain_name               = var.domain_name
  subject_alternative_names = local.acm_sans
  validation_method         = "DNS"
  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_route53_record" "cert_validation" {
  for_each = local.enable_domain ? {
    for dvo in aws_acm_certificate.this[0].domain_validation_options : dvo.domain_name => {
      name   = dvo.resource_record_name
      type   = dvo.resource_record_type
      record = dvo.resource_record_value
    }
  } : {}
  zone_id         = var.route53_zone_id
  name            = each.value.name
  type            = each.value.type
  records         = [each.value.record]
  ttl             = 60
  allow_overwrite = true
}

resource "aws_acm_certificate_validation" "this" {
  count                   = local.enable_domain ? 1 : 0
  certificate_arn         = aws_acm_certificate.this[0].arn
  validation_record_fqdns = [for record in aws_route53_record.cert_validation : record.fqdn]
}

# Alias records for each public hostname -> ALB.
locals {
  alias_hosts = local.enable_domain ? concat(
    [var.domain_name, local.app_host, local.api_host],
    local.enable_proxy ? [local.inference_host] : [],
  ) : []
}

resource "aws_route53_record" "alias" {
  for_each = toset(local.alias_hosts)
  zone_id  = var.route53_zone_id
  name     = each.value
  type     = "A"
  alias {
    name                   = aws_lb.this.dns_name
    zone_id                = aws_lb.this.zone_id
    evaluate_target_health = true
  }
}
