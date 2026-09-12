# ---------------------------------------------------------------------------
# ALB security group — public ingress on the listener ports.
# ---------------------------------------------------------------------------
resource "aws_security_group" "alb" {
  name_prefix = "${var.name}-alb-"
  description = "Public ingress to the Blue ALB"
  vpc_id      = local.vpc_id
}

locals {
  # With a domain, traffic arrives on 80 (redirect) and 443; without one, on 80.
  alb_ports = local.enable_domain ? [80, 443] : [80]
  alb_ingress = {
    for pair in setproduct(local.alb_ports, var.ingress_cidrs) :
    "${pair[0]}-${pair[1]}" => { port = pair[0], cidr = pair[1] }
  }
}

resource "aws_vpc_security_group_ingress_rule" "alb" {
  for_each          = local.alb_ingress
  security_group_id = aws_security_group.alb.id
  cidr_ipv4         = each.value.cidr
  from_port         = each.value.port
  to_port           = each.value.port
  ip_protocol       = "tcp"
  description       = "Public ingress on ${each.value.port}"
}

resource "aws_vpc_security_group_egress_rule" "alb" {
  security_group_id = aws_security_group.alb.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "ALB to targets"
}

# ---------------------------------------------------------------------------
# Website task security group. The tasks have public IPs, so this is what keeps
# them private: the only ingress is the ALB on the container port.
# ---------------------------------------------------------------------------
resource "aws_security_group" "service" {
  name_prefix = "${var.name}-service-"
  description = "Blue ECS tasks"
  vpc_id      = local.vpc_id
}

resource "aws_vpc_security_group_ingress_rule" "service_from_alb" {
  for_each                     = toset(["3000"])
  security_group_id            = aws_security_group.service.id
  referenced_security_group_id = aws_security_group.alb.id
  from_port                    = tonumber(each.value)
  to_port                      = tonumber(each.value)
  ip_protocol                  = "tcp"
  description                  = "ALB to task port ${each.value}"
}

resource "aws_vpc_security_group_egress_rule" "service" {
  security_group_id = aws_security_group.service.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Task egress (image pull, logs)"
}
