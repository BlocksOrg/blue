locals {
  # Applied to every taggable resource through the provider's default_tags.
  tags = {
    Application = var.name
    Environment = var.environment
  }

  create_vpc    = var.vpc_id == ""
  enable_domain = var.domain_name != ""
  enable_proxy  = var.enable_inference_proxy
  resource_name = substr(var.name, 0, 32)

  az_names = slice(data.aws_availability_zones.available.names, 0, var.az_count)

  # Create-or-reuse: downstream refs use these so the choice is transparent.
  vpc_id             = local.create_vpc ? aws_vpc.this[0].id : var.vpc_id
  public_subnet_ids  = local.create_vpc ? aws_subnet.public[*].id : var.public_subnet_ids
  private_subnet_ids = local.create_vpc ? aws_subnet.private[*].id : var.private_subnet_ids

  # Internal service discovery (Cloud Map private DNS). Provisioned only when
  # the inference proxy is enabled — the only consumer of the :8082 hop.
  internal_namespace       = "${var.name}.internal"
  control_api_internal_url = "http://control-api.${local.internal_namespace}:8082"
  inference_health_url     = "http://inference-proxy.${local.internal_namespace}:8081"

  # Public URLs. With a domain, hostnames route through HTTPS; without one,
  # services are exposed on distinct ALB ports over HTTP.
  website_url         = local.enable_domain ? "https://${var.domain_name}" : "http://${aws_lb.this.dns_name}"
  dashboard_url       = local.enable_domain ? "https://${var.app_subdomain}.${var.domain_name}" : "http://${aws_lb.this.dns_name}:3000"
  control_api_url     = local.enable_domain ? "https://${var.api_subdomain}.${var.domain_name}" : "http://${aws_lb.this.dns_name}:8080"
  inference_proxy_url = local.enable_domain ? "https://${var.inference_subdomain}.${var.domain_name}" : "http://${aws_lb.this.dns_name}:8081"

  # Hostnames for listener host rules and ACM SANs.
  app_host       = "${var.app_subdomain}.${var.domain_name}"
  api_host       = "${var.api_subdomain}.${var.domain_name}"
  inference_host = "${var.inference_subdomain}.${var.domain_name}"
  acm_sans       = concat([local.app_host, local.api_host], local.enable_proxy ? [local.inference_host] : [])

  # Blue config file materialized into control-api/worker containers.
  blue_config = var.blue_config_yaml != "" ? var.blue_config_yaml : templatefile("${path.module}/config/blue.yaml.tftpl", {
    enable_gateway            = local.enable_proxy
    gateway_type              = var.gateway_type
    inference_proxy_client_id = var.inference_proxy_client_id
  })

  storage_env = {
    HARNESS_BLOB_BUCKET    = aws_s3_bucket.sessions.id
    HARNESS_PACKAGE_BUCKET = aws_s3_bucket.packages.id
    HARNESS_BLOB_REGION    = var.aws_region
  }

  # control-api (public) — mirrors deploy/helm/templates/deployment.yaml.
  control_api_env = merge(local.storage_env, {
    BLUE_ENABLE_INFERENCE_PROXY = "false"
    BLUE_HEALTHCHECK_COMPONENT  = "control-api"
    HARNESS_RUN_BACKGROUND_JOBS = "false"
    HARNESS_INTERNAL_LISTEN     = "0.0.0.0:8082"
    BETTER_AUTH_URL             = local.dashboard_url
    CONTROL_API_PUBLIC_URL      = local.control_api_url
    CONTROL_API_URL             = local.control_api_url

    # Better Auth lives in the dashboard. The session and JWKS hops are
    # server-to-server, but ECS publishes no internal DNS record for the
    # dashboard (the Cloud Map namespace exists only with the inference
    # proxy), so they ride the public ALB like the proxy's token hop below.
    # `audience` must equal the dashboard's CONTROL_API_PUBLIC_URL, which is
    # the OAuth resource identifier it mints access tokens for.
    HARNESS_AUTH_PUBLIC_URL  = local.dashboard_url
    HARNESS_AUTH_SESSION_URL = "${local.dashboard_url}/api/auth/get-session"
    HARNESS_AUTH_JWKS_URL    = "${local.dashboard_url}/api/auth/jwks"
    HARNESS_AUTH_ISSUER      = "${local.dashboard_url}/api/auth"
    HARNESS_AUTH_AUDIENCE    = local.control_api_url
    HARNESS_OAUTH_CLIENT_ID  = var.oauth_client_id
    HARNESS_AUTH_MODE        = "password"
    }, local.enable_proxy ? {
    HARNESS_GATEWAY_TYPE               = var.gateway_type
    HARNESS_INTERNAL_TRANSPORT_MODE    = "insecure-http"
    HARNESS_INFERENCE_PROXY_URL        = local.inference_proxy_url
    HARNESS_INFERENCE_PROXY_HEALTH_URL = local.inference_health_url
    HARNESS_INTERNAL_ALLOWED_CLIENT_ID = var.inference_proxy_client_id
    HARNESS_GATEWAY_JWT_ISSUER         = local.control_api_url
    HARNESS_GATEWAY_JWT_AUDIENCE       = "blue-inference-proxy"
  } : {})

  # worker (singleton) — mirrors deploy/helm/templates/worker-deployment.yaml.
  worker_env = merge(local.storage_env, {
    BLUE_HEALTHCHECK_COMPONENT      = "control-api"
    HARNESS_RUN_BACKGROUND_JOBS     = "true"
    HARNESS_INTERNAL_LISTEN         = "127.0.0.1:8082"
    HARNESS_INTERNAL_TRANSPORT_MODE = "insecure-http"
  })

  # dashboard (public) — mirrors deploy/helm/templates/dashboard-deployment.yaml.
  dashboard_env = {
    BLUE_HEALTHCHECK_COMPONENT        = "dashboard"
    CONTROL_API_URL                   = local.control_api_url
    CONTROL_API_PUBLIC_URL            = local.control_api_url
    BETTER_AUTH_URL                   = local.dashboard_url
    HARNESS_AUTH_PUBLIC_URL           = local.dashboard_url
    HARNESS_OAUTH_CLIENT_ID           = var.oauth_client_id
    HARNESS_INFERENCE_PROXY_CLIENT_ID = var.inference_proxy_client_id
  }

  # inference-proxy — mirrors deploy/helm/templates/inference-proxy-deployment.yaml.
  inference_proxy_env = {
    BLUE_HEALTHCHECK_COMPONENT             = "inference-proxy"
    HARNESS_GATEWAY_TYPE                   = var.gateway_type
    HARNESS_INTERNAL_TRANSPORT_MODE        = "insecure-http"
    HARNESS_GATEWAY_RESOLVER_URL           = "${local.control_api_internal_url}/internal/gateway/resolve"
    HARNESS_GATEWAY_EVENT_URL              = "${local.control_api_internal_url}/internal/gateway/events"
    HARNESS_GATEWAY_CREDENTIAL_INVALID_URL = "${local.control_api_internal_url}/internal/gateway/credential-invalid"
    HARNESS_GATEWAY_LOG_URL                = "${local.control_api_internal_url}/internal/gateway/request-logs"
    HARNESS_PROXY_OAUTH_TOKEN_URL          = "${local.dashboard_url}/api/auth/oauth2/token"
    HARNESS_PROXY_OAUTH_CLIENT_ID          = var.inference_proxy_client_id
    HARNESS_PROXY_OAUTH_SCOPE              = "gateway:resolve"
    HARNESS_PROXY_OAUTH_RESOURCE           = local.control_api_url
    HARNESS_GATEWAY_JWKS_URL               = "${local.control_api_internal_url}/internal/gateway/jwks"
    HARNESS_GATEWAY_JWT_ISSUER             = local.control_api_url
    HARNESS_GATEWAY_JWT_AUDIENCE           = "blue-inference-proxy"
  }
}
