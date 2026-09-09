# ---------------------------------------------------------------------------
# ALB security group — public ingress on the listener ports.
# ---------------------------------------------------------------------------
resource "aws_security_group" "alb" {
  name_prefix = "${var.name}-alb-"
  description = "Public ingress to the Blue ALB"
  vpc_id      = local.vpc_id
}

locals {
  # With a domain, traffic arrives on 80 (redirect) and 443. Without one,
  # services are exposed directly on per-service HTTP ports.
  alb_ports = local.enable_domain ? [80, 443] : concat([80, 3000, 8080], local.enable_proxy ? [8081] : [])
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
# Shared ECS task security group. ALB reaches the container ports; the proxy
# reaches control-api's internal :8082 hop via a self-referencing rule.
# ---------------------------------------------------------------------------
resource "aws_security_group" "service" {
  name_prefix = "${var.name}-service-"
  description = "Blue ECS tasks"
  vpc_id      = local.vpc_id
}

locals {
  service_ports_from_alb = concat([8080, 3000], local.enable_proxy ? [8081] : [])
}

resource "aws_vpc_security_group_ingress_rule" "service_from_alb" {
  for_each                     = toset([for p in local.service_ports_from_alb : tostring(p)])
  security_group_id            = aws_security_group.service.id
  referenced_security_group_id = aws_security_group.alb.id
  from_port                    = tonumber(each.value)
  to_port                      = tonumber(each.value)
  ip_protocol                  = "tcp"
  description                  = "ALB to task port ${each.value}"
}

resource "aws_vpc_security_group_ingress_rule" "service_internal" {
  security_group_id            = aws_security_group.service.id
  referenced_security_group_id = aws_security_group.service.id
  from_port                    = 8082
  to_port                      = 8082
  ip_protocol                  = "tcp"
  description                  = "Internal control-api hop between Blue tasks"
}

resource "aws_vpc_security_group_egress_rule" "service" {
  security_group_id = aws_security_group.service.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Task egress (image pull, datastores, AWS APIs)"
}

# ---------------------------------------------------------------------------
# Datastore security groups. Ingress from the task SG plus any approved extra
# client SGs (reuses the existing module's for_each pattern).
# ---------------------------------------------------------------------------
resource "aws_security_group" "database" {
  name_prefix = "${var.name}-database-"
  description = "PostgreSQL access from Blue workloads"
  vpc_id      = local.vpc_id
}

resource "aws_vpc_security_group_ingress_rule" "database_service" {
  security_group_id            = aws_security_group.database.id
  referenced_security_group_id = aws_security_group.service.id
  from_port                    = 5432
  to_port                      = 5432
  ip_protocol                  = "tcp"
  description                  = "PostgreSQL from Blue ECS tasks"
}

resource "aws_vpc_security_group_ingress_rule" "database_extra" {
  for_each                     = var.database_client_security_group_ids
  security_group_id            = aws_security_group.database.id
  referenced_security_group_id = each.value
  from_port                    = 5432
  to_port                      = 5432
  ip_protocol                  = "tcp"
  description                  = "PostgreSQL from an approved Blue workload security group"
}

resource "aws_security_group" "redis" {
  count       = var.enable_redis ? 1 : 0
  name_prefix = "${var.name}-redis-"
  description = "Redis access from Blue workloads"
  vpc_id      = local.vpc_id
}

resource "aws_vpc_security_group_ingress_rule" "redis_service" {
  count                        = var.enable_redis ? 1 : 0
  security_group_id            = aws_security_group.redis[0].id
  referenced_security_group_id = aws_security_group.service.id
  from_port                    = 6379
  to_port                      = 6379
  ip_protocol                  = "tcp"
  description                  = "Redis from Blue ECS tasks"
}

resource "aws_vpc_security_group_ingress_rule" "redis_extra" {
  for_each                     = var.enable_redis ? var.redis_client_security_group_ids : []
  security_group_id            = aws_security_group.redis[0].id
  referenced_security_group_id = each.value
  from_port                    = 6379
  to_port                      = 6379
  ip_protocol                  = "tcp"
  description                  = "Redis TLS from an approved Blue workload security group"
}
