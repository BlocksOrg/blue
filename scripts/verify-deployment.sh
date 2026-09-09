#!/usr/bin/env bash
set -euo pipefail

chart="deploy/helm"
prerequisites_chart="deploy/helm-prerequisites"
digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
rendered="/tmp/blue-production.yaml"
production_network=(
  --set 'networkPolicy.databaseCidrs[0]=10.0.0.0/24'
  --set 'networkPolicy.externalHttpsCidrs[0]=10.0.1.0/24'
)

if helm template blue-prerequisites "$prerequisites_chart" >/dev/null 2>&1; then
  echo "prerequisite render without a database CIDR unexpectedly succeeded" >&2
  exit 1
fi
helm lint "$prerequisites_chart" --set 'networkPolicy.databaseCidrs[0]=10.0.0.0/24'
helm template blue-prerequisites "$prerequisites_chart" \
  --set 'networkPolicy.databaseCidrs[0]=10.0.0.0/24' > /tmp/blue-prerequisites.yaml
grep -q 'name: blue-namespace-default-deny' /tmp/blue-prerequisites.yaml
grep -q 'blue.blocks.org/migration-access: "true"' /tmp/blue-prerequisites.yaml

if helm template blue "$chart" "${production_network[@]}" --set image.digest="$digest" >/dev/null 2>&1; then
  echo "production render without blue.existingSecret unexpectedly succeeded" >&2
  exit 1
fi
if helm template blue "$chart" "${production_network[@]}" --set blue.existingSecret=blue-runtime >/dev/null 2>&1; then
  echo "production render without image.digest unexpectedly succeeded" >&2
  exit 1
fi
for invalid in \
  'migrations.enabled=false' \
  'networkPolicy.enabled=false' \
  'components.worker.replicas=2'; do
  if helm template blue "$chart" "${production_network[@]}" \
    --set blue.existingSecret=blue-runtime --set image.digest="$digest" \
    --set "$invalid" >/dev/null 2>&1; then
    echo "production render unexpectedly accepted $invalid" >&2
    exit 1
  fi
done
for reserved in BLUE_ENVIRONMENT BLUE_GATEWAY_ENABLED; do
  if helm template blue "$chart" "${production_network[@]}" \
    --set blue.existingSecret=blue-runtime --set image.digest="$digest" \
    --set "blue.env.${reserved}=development" >/dev/null 2>&1; then
    echo "production render unexpectedly accepted reserved blue.env.${reserved}" >&2
    exit 1
  fi
done
if helm template blue "$chart" --set blue.existingSecret=blue-runtime --set image.digest="$digest" >/dev/null 2>&1; then
  echo "production render without dependency CIDRs unexpectedly succeeded" >&2
  exit 1
fi

helm lint "$chart" "${production_network[@]}" --set blue.existingSecret=blue-runtime --set image.digest="$digest"
helm template blue "$chart" \
  "${production_network[@]}" \
  --set blue.existingSecret=blue-runtime \
  --set image.digest="$digest" > "$rendered"

grep -q 'kind: Job' "$rendered"
grep -q 'pre-install,pre-upgrade' "$rendered"
grep -q 'blue.blocks.org/migration-access: "true"' "$rendered"
for key in HARNESS_DATABASE_URL BETTER_AUTH_SECRET HARNESS_BOOTSTRAP_ADMIN_PASSWORD; do
  grep -q "key: $key" "$rendered"
done
grep -q 'name: BLUE_GATEWAY_ENABLED' "$rendered"
grep -q 'value: "false"' "$rendered"
grep -q 'name: BLUE_ENVIRONMENT' "$rendered"
grep -q 'value: production' "$rendered"
grep -q 'name: blue-blue-default-deny' "$rendered"
grep -q 'policyTypes: \[Ingress, Egress\]' "$rendered"
if grep -q 'app.kubernetes.io/component: inference-proxy' "$rendered"; then
  echo "governance-only render contains inference-proxy resources" >&2
  exit 1
fi
if grep -q 'namespaceSelector: {}' "$rendered"; then
  echo "production render contains an unrestricted namespace selector" >&2
  exit 1
fi

if helm template blue "$chart" \
  "${production_network[@]}" \
  --set blue.existingSecret=blue-runtime \
  --set image.digest="$digest" \
  --set blue.enableInferenceProxy=true >/dev/null 2>&1; then
  echo "gateway render without gateway settings unexpectedly succeeded" >&2
  exit 1
fi
helm template blue "$chart" \
  "${production_network[@]}" \
  --set blue.existingSecret=blue-runtime \
  --set image.digest="$digest" \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.serverSecret=blue-internal-server \
  --set blue.internalTransport.clientSecret=blue-internal-client \
  > /tmp/blue-gateway-production.yaml
grep -q 'app.kubernetes.io/component: inference-proxy' /tmp/blue-gateway-production.yaml
for key in HARNESS_GATEWAY_ENCRYPTION_KEY HARNESS_PROXY_OAUTH_CLIENT_SECRET; do
  grep -q "key: $key" /tmp/blue-gateway-production.yaml
done
grep -q 'name: BLUE_GATEWAY_ENABLED' /tmp/blue-gateway-production.yaml
grep -q 'value: "true"' /tmp/blue-gateway-production.yaml

helm template blue "$chart" \
  "${production_network[@]}" \
  --set blue.existingSecret=blue-runtime \
  --set image.digest="$digest" \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.mode=insecure-http \
  > /tmp/blue-gateway-insecure-production.yaml

helm lint "$chart" -f "$chart/values-evaluation.yaml"
helm template blue "$chart" -f "$chart/values-evaluation.yaml" > /tmp/blue-evaluation.yaml
helm template blue "$chart" -f "$chart/values-evaluation.yaml" \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.mode=insecure-http \
  > /tmp/blue-gateway-evaluation.yaml
