resource "aws_ecs_cluster" "this" {
  name = var.name
  setting {
    name  = "containerInsights"
    value = "enabled"
  }
}

resource "aws_ecs_cluster_capacity_providers" "this" {
  cluster_name       = aws_ecs_cluster.this.name
  capacity_providers = ["FARGATE"]
  default_capacity_provider_strategy {
    capacity_provider = "FARGATE"
    weight            = 1
  }
}

resource "aws_cloudwatch_log_group" "website" {
  name              = "/ecs/${var.name}/website"
  retention_in_days = var.log_retention_days
  kms_key_id        = aws_kms_key.blue.arn
  depends_on        = [aws_kms_key_policy.blue]
}

resource "aws_ecs_task_definition" "website" {
  family                   = "${var.name}-website"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.website_cpu
  memory                   = var.website_memory
  execution_role_arn       = aws_iam_role.execution.arn

  container_definitions = jsonencode([
    {
      name                  = "website"
      image                 = var.website_image
      essential             = true
      repositoryCredentials = var.image_pull_secret_arn != "" ? { credentialsParameter = var.image_pull_secret_arn } : null
      stopTimeout           = var.stop_timeout_seconds
      portMappings          = [{ containerPort = 3000, protocol = "tcp" }]
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.website.name
          "awslogs-region"        = var.aws_region
          "awslogs-stream-prefix" = "website"
        }
      }
    },
  ])
}

resource "aws_ecs_service" "website" {
  name                               = "${var.name}-website"
  cluster                            = aws_ecs_cluster.this.id
  task_definition                    = aws_ecs_task_definition.website.arn
  desired_count                      = var.website_desired_count
  launch_type                        = "FARGATE"
  deployment_minimum_healthy_percent = 100
  deployment_maximum_percent         = 200
  health_check_grace_period_seconds  = var.health_check_grace_period_seconds
  wait_for_steady_state              = var.wait_for_steady_state
  force_new_deployment               = true

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  # Public subnets with a public IP, so tasks pull the image and ship logs
  # without a NAT gateway. The service security group only admits the ALB.
  network_configuration {
    subnets          = local.public_subnet_ids
    security_groups  = [aws_security_group.service.id]
    assign_public_ip = true
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.website.arn
    container_name   = "website"
    container_port   = 3000
  }

  # website-deploy.yml ships new images by registering task definition
  # revisions outside Tofu. Without this, every apply rolls the site back to
  # website_image.
  lifecycle {
    ignore_changes = [task_definition]
  }

  depends_on = [aws_lb.this]
}
