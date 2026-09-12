resource "aws_lb" "this" {
  name               = local.resource_name
  load_balancer_type = "application"
  internal           = false
  subnets            = local.public_subnet_ids
  security_groups    = [aws_security_group.alb.id]
  idle_timeout       = var.alb_idle_timeout_seconds
}

resource "aws_lb_target_group" "website" {
  name                 = substr("${var.name}-website", 0, 32)
  port                 = 3000
  protocol             = "HTTP"
  vpc_id               = local.vpc_id
  target_type          = "ip"
  deregistration_delay = var.deregistration_delay_seconds
  slow_start           = var.slow_start_seconds
  health_check {
    path                = "/health"
    matcher             = "200"
    healthy_threshold   = 3
    unhealthy_threshold = 3
    interval            = 15
    timeout             = 5
  }
}

# ===========================================================================
# Domain path: HTTPS, HTTP -> HTTPS redirect.
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
    target_group_arn = aws_lb_target_group.website.arn
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
# No-domain path: plain HTTP on port 80.
# ===========================================================================
resource "aws_lb_listener" "http_default" {
  count             = local.enable_domain ? 0 : 1
  load_balancer_arn = aws_lb.this.arn
  port              = 80
  protocol          = "HTTP"
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.website.arn
  }
}
