# ---------------------------------------------------------------------------
# Cluster
# ---------------------------------------------------------------------------
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

# ---------------------------------------------------------------------------
# Internal service discovery (Cloud Map). Only the inference proxy consumes the
# control-api :8082 hop, so the namespace exists only when the proxy is enabled.
# ---------------------------------------------------------------------------
resource "aws_service_discovery_private_dns_namespace" "internal" {
  count       = local.enable_proxy ? 1 : 0
  name        = local.internal_namespace
  description = "Blue internal service discovery"
  vpc         = local.vpc_id
}

resource "aws_service_discovery_service" "control_api" {
  count = local.enable_proxy ? 1 : 0
  name  = "control-api"
  dns_config {
    namespace_id = aws_service_discovery_private_dns_namespace.internal[0].id
    dns_records {
      type = "A"
      ttl  = 10
    }
    routing_policy = "MULTIVALUE"
  }
  health_check_custom_config {}
}

resource "aws_service_discovery_service" "inference_proxy" {
  count = local.enable_proxy ? 1 : 0
  name  = "inference-proxy"
  dns_config {
    namespace_id = aws_service_discovery_private_dns_namespace.internal[0].id
    dns_records {
      type = "A"
      ttl  = 10
    }
    routing_policy = "MULTIVALUE"
  }
  health_check_custom_config {}
}

# ---------------------------------------------------------------------------
# Log groups
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_log_group" "control_api" {
  name              = "/ecs/${var.name}/control-api"
  retention_in_days = var.log_retention_days
  kms_key_id        = aws_kms_key.blue.arn
}
resource "aws_cloudwatch_log_group" "worker" {
  name              = "/ecs/${var.name}/worker"
  retention_in_days = var.log_retention_days
  kms_key_id        = aws_kms_key.blue.arn
}
resource "aws_cloudwatch_log_group" "dashboard" {
  name              = "/ecs/${var.name}/dashboard"
  retention_in_days = var.log_retention_days
  kms_key_id        = aws_kms_key.blue.arn
}
resource "aws_cloudwatch_log_group" "inference_proxy" {
  count             = local.enable_proxy ? 1 : 0
  name              = "/ecs/${var.name}/inference-proxy"
  retention_in_days = var.log_retention_days
  kms_key_id        = aws_kms_key.blue.arn
}

# ---------------------------------------------------------------------------
# Helper locals: env/secret list shapes and shared container fields.
# ---------------------------------------------------------------------------
locals {
  repository_credentials = var.image_pull_secret_arn != "" ? { credentialsParameter = var.image_pull_secret_arn } : null

  control_api_env_list = concat(
    [for k, v in local.control_api_env : { name = k, value = v }],
    [{ name = "BLUE_CONFIG_YAML", value = local.blue_config }],
  )
  worker_env_list = concat(
    [for k, v in local.worker_env : { name = k, value = v }],
    [{ name = "BLUE_CONFIG_YAML", value = local.blue_config }],
  )
  dashboard_env_list       = [for k, v in local.dashboard_env : { name = k, value = v }]
  inference_proxy_env_list = [for k, v in local.inference_proxy_env : { name = k, value = v }]

  control_api_secrets = concat(
    [for k in ["HARNESS_DATABASE_URL", "BETTER_AUTH_SECRET", "HARNESS_BOOTSTRAP_ADMIN_EMAIL", "HARNESS_BOOTSTRAP_ADMIN_PASSWORD"] : { name = k, valueFrom = local.secret_ref[k] }],
    var.enable_redis ? [{ name = "HARNESS_REDIS_URL", valueFrom = local.secret_ref["HARNESS_REDIS_URL"] }] : [],
    local.enable_proxy ? [{ name = "HARNESS_GATEWAY_ENCRYPTION_KEY", valueFrom = local.secret_ref["HARNESS_GATEWAY_ENCRYPTION_KEY"] }] : [],
  )
  migrate_secrets = [{ name = "HARNESS_DATABASE_URL", valueFrom = local.secret_ref["HARNESS_DATABASE_URL"] }]
  worker_secrets = concat(
    [{ name = "HARNESS_DATABASE_URL", valueFrom = local.secret_ref["HARNESS_DATABASE_URL"] }],
    local.enable_proxy ? [{ name = "HARNESS_GATEWAY_ENCRYPTION_KEY", valueFrom = local.secret_ref["HARNESS_GATEWAY_ENCRYPTION_KEY"] }] : [],
  )
  dashboard_secrets = concat(
    [for k in ["HARNESS_DATABASE_URL", "BETTER_AUTH_SECRET", "HARNESS_BOOTSTRAP_ADMIN_PASSWORD"] : { name = k, valueFrom = local.secret_ref[k] }],
    local.enable_proxy ? [{ name = "HARNESS_PROXY_OAUTH_CLIENT_SECRET", valueFrom = local.secret_ref["HARNESS_PROXY_OAUTH_CLIENT_SECRET"] }] : [],
  )
  inference_proxy_secrets = [
    { name = "HARNESS_PROXY_OAUTH_CLIENT_SECRET", valueFrom = local.secret_ref["HARNESS_PROXY_OAUTH_CLIENT_SECRET"] },
    { name = "HARNESS_REDIS_URL", valueFrom = local.secret_ref["HARNESS_REDIS_URL"] },
  ]

  # Writes the rendered Blue config, then hands off to the image's normal
  # entrypoint (tini -> blue-entrypoint) so signal handling is preserved.
  config_command = ["printf '%s' \"$BLUE_CONFIG_YAML\" > /etc/blue/blue.yaml && exec /usr/bin/tini -- /usr/local/bin/blue-entrypoint control-api"]
}

# ---------------------------------------------------------------------------
# control-api task definition (migrate init container + app container)
# ---------------------------------------------------------------------------
resource "aws_ecs_task_definition" "control_api" {
  family                   = "${var.name}-control-api"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.control_api_cpu
  memory                   = var.control_api_memory
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.task.arn

  container_definitions = jsonencode([
    {
      name                  = "migrate"
      image                 = var.image
      essential             = false
      command               = ["migrate"]
      repositoryCredentials = local.repository_credentials
      secrets               = local.migrate_secrets
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.control_api.name
          "awslogs-region"        = var.aws_region
          "awslogs-stream-prefix" = "migrate"
        }
      }
    },
    {
      name                  = "control-api"
      image                 = var.image
      essential             = true
      entryPoint            = ["/bin/sh", "-c"]
      command               = local.config_command
      repositoryCredentials = local.repository_credentials
      stopTimeout           = var.stop_timeout_seconds
      dependsOn             = [{ containerName = "migrate", condition = "SUCCESS" }]
      environment           = local.control_api_env_list
      secrets               = local.control_api_secrets
      portMappings = [
        { containerPort = 8080, protocol = "tcp" },
        { containerPort = 8082, protocol = "tcp" },
      ]
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.control_api.name
          "awslogs-region"        = var.aws_region
          "awslogs-stream-prefix" = "control-api"
        }
      }
    },
  ])
}

# ---------------------------------------------------------------------------
# worker task definition (control-api binary, background jobs, no migrate)
# ---------------------------------------------------------------------------
resource "aws_ecs_task_definition" "worker" {
  family                   = "${var.name}-worker"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.worker_cpu
  memory                   = var.worker_memory
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.task.arn

  container_definitions = jsonencode([
    {
      name                  = "worker"
      image                 = var.image
      essential             = true
      entryPoint            = ["/bin/sh", "-c"]
      command               = local.config_command
      repositoryCredentials = local.repository_credentials
      stopTimeout           = var.stop_timeout_seconds
      environment           = local.worker_env_list
      secrets               = local.worker_secrets
      portMappings          = [{ containerPort = 8080, protocol = "tcp" }]
      healthCheck = {
        command     = ["CMD", "/usr/local/bin/blue-healthcheck"]
        interval    = 30
        timeout     = 10
        retries     = 3
        startPeriod = 60
      }
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.worker.name
          "awslogs-region"        = var.aws_region
          "awslogs-stream-prefix" = "worker"
        }
      }
    },
  ])
}

# ---------------------------------------------------------------------------
# dashboard task definition
# ---------------------------------------------------------------------------
resource "aws_ecs_task_definition" "dashboard" {
  family                   = "${var.name}-dashboard"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.dashboard_cpu
  memory                   = var.dashboard_memory
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.task.arn

  container_definitions = jsonencode([
    {
      name                  = "dashboard"
      image                 = var.image
      essential             = true
      command               = ["dashboard"]
      repositoryCredentials = local.repository_credentials
      stopTimeout           = var.stop_timeout_seconds
      environment           = local.dashboard_env_list
      secrets               = local.dashboard_secrets
      portMappings          = [{ containerPort = 3000, protocol = "tcp" }]
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.dashboard.name
          "awslogs-region"        = var.aws_region
          "awslogs-stream-prefix" = "dashboard"
        }
      }
    },
  ])
}

# ---------------------------------------------------------------------------
# inference-proxy task definition (optional)
# ---------------------------------------------------------------------------
resource "aws_ecs_task_definition" "inference_proxy" {
  count                    = local.enable_proxy ? 1 : 0
  family                   = "${var.name}-inference-proxy"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.inference_proxy_cpu
  memory                   = var.inference_proxy_memory
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.task.arn

  container_definitions = jsonencode([
    {
      name                  = "inference-proxy"
      image                 = var.image
      essential             = true
      command               = ["inference-proxy"]
      repositoryCredentials = local.repository_credentials
      stopTimeout           = var.stop_timeout_seconds
      environment           = local.inference_proxy_env_list
      secrets               = local.inference_proxy_secrets
      portMappings          = [{ containerPort = 8081, protocol = "tcp" }]
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.inference_proxy[0].name
          "awslogs-region"        = var.aws_region
          "awslogs-stream-prefix" = "inference-proxy"
        }
      }
    },
  ])
}

# ---------------------------------------------------------------------------
# Services
# ---------------------------------------------------------------------------
resource "aws_ecs_service" "control_api" {
  name                               = "${var.name}-control-api"
  cluster                            = aws_ecs_cluster.this.id
  task_definition                    = aws_ecs_task_definition.control_api.arn
  desired_count                      = var.control_api_desired_count
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

  network_configuration {
    subnets          = local.private_subnet_ids
    security_groups  = [aws_security_group.service.id]
    assign_public_ip = false
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.control_api.arn
    container_name   = "control-api"
    container_port   = 8080
  }

  dynamic "service_registries" {
    for_each = local.enable_proxy ? [1] : []
    content {
      registry_arn = aws_service_discovery_service.control_api[0].arn
    }
  }

  depends_on = [aws_lb.this]
}

resource "aws_ecs_service" "worker" {
  name                               = "${var.name}-worker"
  cluster                            = aws_ecs_cluster.this.id
  task_definition                    = aws_ecs_task_definition.worker.arn
  desired_count                      = 1
  launch_type                        = "FARGATE"
  deployment_minimum_healthy_percent = 0
  deployment_maximum_percent         = 100
  force_new_deployment               = true

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  network_configuration {
    subnets          = local.private_subnet_ids
    security_groups  = [aws_security_group.service.id]
    assign_public_ip = false
  }
}

resource "aws_ecs_service" "dashboard" {
  name                               = "${var.name}-dashboard"
  cluster                            = aws_ecs_cluster.this.id
  task_definition                    = aws_ecs_task_definition.dashboard.arn
  desired_count                      = var.dashboard_desired_count
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

  network_configuration {
    subnets          = local.private_subnet_ids
    security_groups  = [aws_security_group.service.id]
    assign_public_ip = false
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.dashboard.arn
    container_name   = "dashboard"
    container_port   = 3000
  }

  depends_on = [aws_lb.this]
}

resource "aws_ecs_service" "inference_proxy" {
  count                              = local.enable_proxy ? 1 : 0
  name                               = "${var.name}-inference-proxy"
  cluster                            = aws_ecs_cluster.this.id
  task_definition                    = aws_ecs_task_definition.inference_proxy[0].arn
  desired_count                      = var.inference_proxy_desired_count
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

  network_configuration {
    subnets          = local.private_subnet_ids
    security_groups  = [aws_security_group.service.id]
    assign_public_ip = false
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.inference_proxy[0].arn
    container_name   = "inference-proxy"
    container_port   = 8081
  }

  service_registries {
    registry_arn = aws_service_discovery_service.inference_proxy[0].arn
  }

  depends_on = [aws_lb.this]
}
