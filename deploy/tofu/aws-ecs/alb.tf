resource "aws_lb" "this" {
  name               = local.resource_name
  load_balancer_type = "application"
  internal           = false
  subnets            = local.public_subnet_ids
  security_groups    = [aws_security_group.alb.id]
  idle_timeout       = var.alb_idle_timeout_seconds
}

# ---------------------------------------------------------------------------
# Target groups (ip targets for Fargate awsvpc tasks)
# ---------------------------------------------------------------------------
resource "aws_lb_target_group" "dashboard" {
  name                 = substr("${var.name}-dashboard", 0, 32)
  port                 = 3000
  protocol             = "HTTP"
  vpc_id               = local.vpc_id
  target_type          = "ip"
  deregistration_delay = var.deregistration_delay_seconds
  slow_start           = var.slow_start_seconds
  health_check {
    path                = "/api/health"
    matcher             = "200"
    healthy_threshold   = 3
    unhealthy_threshold = 3
    interval            = 15
    timeout             = 5
  }
}

resource "aws_lb_target_group" "control_api" {
  name                 = substr("${var.name}-control-api", 0, 32)
  port                 = 8080
  protocol             = "HTTP"
  vpc_id               = local.vpc_id
  target_type          = "ip"
  deregistration_delay = var.deregistration_delay_seconds
  slow_start           = var.slow_start_seconds
  health_check {
    path                = "/ready"
    matcher             = "200,401,403"
    healthy_threshold   = 3
    unhealthy_threshold = 3
    interval            = 15
    timeout             = 5
  }
}

resource "aws_lb_target_group" "inference_proxy" {
  count                = local.enable_proxy ? 1 : 0
  name                 = substr("${var.name}-inference-proxy", 0, 32)
  port                 = 8081
  protocol             = "HTTP"
  vpc_id               = local.vpc_id
  target_type          = "ip"
  deregistration_delay = var.inference_deregistration_delay_seconds
  health_check {
    path                = "/ready"
    matcher             = "200,401,403"
    healthy_threshold   = 3
    unhealthy_threshold = 3
    interval            = 15
    timeout             = 5
  }
}

# ===========================================================================
# Domain path: HTTPS with host-based routing, HTTP -> HTTPS redirect.
# ===========================================================================
resource "aws_lb_listener" "https" {
  count             = local.enable_domain ? 1 : 0
  load_balancer_arn = aws_lb.this.arn
  port              = 443
  protocol          = "HTTPS"
  ssl_policy        = "ELBSecurityPolicy-TLS13-1-2-2021-06"
  certificate_arn   = aws_acm_certificate_validation.this[0].certificate_arn

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.dashboard.arn
  }
}

resource "aws_lb_listener_rule" "app" {
  count        = local.enable_domain ? 1 : 0
  listener_arn = aws_lb_listener.https[0].arn
  priority     = 10
  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.dashboard.arn
  }
  condition {
    host_header { values = [local.app_host] }
  }
}

resource "aws_lb_listener_rule" "api" {
  count        = local.enable_domain ? 1 : 0
  listener_arn = aws_lb_listener.https[0].arn
  priority     = 20
  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.control_api.arn
  }
  condition {
    host_header { values = [local.api_host] }
  }
}

resource "aws_lb_listener_rule" "inference" {
  count        = local.enable_domain && local.enable_proxy ? 1 : 0
  listener_arn = aws_lb_listener.https[0].arn
  priority     = 30
  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.inference_proxy[0].arn
  }
  condition {
    host_header { values = [local.inference_host] }
  }
}

resource "aws_lb_listener" "http_redirect" {
  count             = local.enable_domain ? 1 : 0
  load_balancer_arn = aws_lb.this.arn
  port              = 80
  protocol          = "HTTP"
  default_action {
    type = "redirect"
    redirect {
      port        = "443"
      protocol    = "HTTPS"
      status_code = "HTTP_301"
    }
  }
}

# ===========================================================================
# No-domain path: plain HTTP, one port per public service.
# ===========================================================================
resource "aws_lb_listener" "http_default" {
  count             = local.enable_domain ? 0 : 1
  load_balancer_arn = aws_lb.this.arn
  port              = 80
  protocol          = "HTTP"
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.dashboard.arn
  }
}

resource "aws_lb_listener" "http_dashboard" {
  count             = local.enable_domain ? 0 : 1
  load_balancer_arn = aws_lb.this.arn
  port              = 3000
  protocol          = "HTTP"
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.dashboard.arn
  }
}

resource "aws_lb_listener" "http_control_api" {
  count             = local.enable_domain ? 0 : 1
  load_balancer_arn = aws_lb.this.arn
  port              = 8080
  protocol          = "HTTP"
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.control_api.arn
  }
}

resource "aws_lb_listener" "http_inference" {
  count             = !local.enable_domain && local.enable_proxy ? 1 : 0
  load_balancer_arn = aws_lb.this.arn
  port              = 8081
  protocol          = "HTTP"
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.inference_proxy[0].arn
  }
}
