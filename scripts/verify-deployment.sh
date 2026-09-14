#!/usr/bin/env bash
set -euo pipefail

chart="deploy/helm"
digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
rendered="/tmp/blue-production.yaml"
production_network=(
  --set 'networkPolicy.databaseCidrs[0]=10.0.0.0/24'
  --set 'networkPolicy.externalHttpsCidrs[0]=10.0.1.0/24'
)
gateway_jwt=(
  --set blue.inferenceJwt.secret=blue-gateway-jwt
)

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
# The check above omits every gateway value, so it would still pass if the
# inferenceJwt guard were deleted. Pin that guard on its own.
if helm template blue "$chart" \
  "${production_network[@]}" \
  --set blue.existingSecret=blue-runtime \
  --set image.digest="$digest" \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.serverSecret=blue-internal-server \
  --set blue.internalTransport.clientSecret=blue-internal-client >/dev/null 2>&1; then
  echo "gateway render without blue.inferenceJwt unexpectedly succeeded" >&2
  exit 1
fi
helm template blue "$chart" \
  "${production_network[@]}" \
  "${gateway_jwt[@]}" \
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
grep -q 'name: HARNESS_GATEWAY_JWT_PRIVATE_KEY_FILE' /tmp/blue-gateway-production.yaml
# The Control API derives its JWKS from the signing key, so the chart must not
# configure a JWKS or key ID, and adds the previous key only during a rotation.
if grep -Eq 'HARNESS_GATEWAY_JWT_(ACTIVE_KID|JWKS_FILE|PREVIOUS_PRIVATE_KEY_FILE)' /tmp/blue-gateway-production.yaml; then
  echo "default gateway render configured a JWKS, key ID, or previous key" >&2
  exit 1
fi
helm template blue "$chart" \
  "${production_network[@]}" \
  "${gateway_jwt[@]}" \
  --set blue.inferenceJwt.includePreviousKey=true \
  --set blue.existingSecret=blue-runtime \
  --set image.digest="$digest" \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.serverSecret=blue-internal-server \
  --set blue.internalTransport.clientSecret=blue-internal-client \
  > /tmp/blue-gateway-rotation.yaml
grep -q 'name: HARNESS_GATEWAY_JWT_PREVIOUS_PRIVATE_KEY_FILE' /tmp/blue-gateway-rotation.yaml
grep -q 'secretName: "blue-gateway-jwt"' /tmp/blue-gateway-production.yaml
grep -q 'name: gateway-jwt' /tmp/blue-gateway-production.yaml
grep -q 'name: BLUE_GATEWAY_ENABLED' /tmp/blue-gateway-production.yaml
grep -q 'value: "true"' /tmp/blue-gateway-production.yaml

helm template blue "$chart" \
  "${production_network[@]}" \
  "${gateway_jwt[@]}" \
  --set blue.existingSecret=blue-runtime \
  --set image.digest="$digest" \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.mode=insecure-http \
  > /tmp/blue-gateway-insecure-production.yaml

# The proxy's client identity comes either as one combined client.pem or as the
# tls.crt + tls.key pair every non-cert-manager issuer emits. Each layout must
# render its own variables and none of the other's, or the proxy either reads a
# file that is not mounted or trips its combined-plus-split conflict check.
grep -q 'name: HARNESS_PROXY_CLIENT_IDENTITY_FILE' /tmp/blue-gateway-production.yaml
if grep -Eq 'HARNESS_PROXY_CLIENT_(CERT|KEY)_FILE' /tmp/blue-gateway-production.yaml; then
  echo "default client-secret render emitted split cert/key variables" >&2
  exit 1
fi
helm template blue "$chart" \
  "${production_network[@]}" \
  "${gateway_jwt[@]}" \
  --set blue.existingSecret=blue-runtime \
  --set image.digest="$digest" \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.serverSecret=blue-internal-server \
  --set blue.internalTransport.clientSecret=blue-internal-client \
  --set blue.internalTransport.clientSecretFormat=split \
  > /tmp/blue-gateway-split-identity.yaml
grep -q 'name: HARNESS_PROXY_CLIENT_CERT_FILE' /tmp/blue-gateway-split-identity.yaml
grep -q 'name: HARNESS_PROXY_CLIENT_KEY_FILE' /tmp/blue-gateway-split-identity.yaml
grep -q 'name: HARNESS_INTERNAL_CA_FILE' /tmp/blue-gateway-split-identity.yaml
if grep -q 'HARNESS_PROXY_CLIENT_IDENTITY_FILE' /tmp/blue-gateway-split-identity.yaml; then
  echo "split client-secret render still emitted the combined identity variable" >&2
  exit 1
fi
# The mount is what makes either layout readable; it is emitted under a separate
# mtls conditional in this template, so assert it alongside the split render.
grep -q 'name: internal-tls, mountPath: /var/run/blue/internal-tls' /tmp/blue-gateway-split-identity.yaml
grep -q 'secretName: "blue-internal-client"' /tmp/blue-gateway-split-identity.yaml
if helm template blue "$chart" \
  "${production_network[@]}" \
  "${gateway_jwt[@]}" \
  --set blue.existingSecret=blue-runtime \
  --set image.digest="$digest" \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.serverSecret=blue-internal-server \
  --set blue.internalTransport.clientSecret=blue-internal-client \
  --set blue.internalTransport.clientSecretFormat=bogus >/dev/null 2>&1; then
  echo "gateway render unexpectedly accepted an unknown clientSecretFormat" >&2
  exit 1
fi

helm lint "$chart" -f "$chart/values-evaluation.yaml"
helm template blue "$chart" -f "$chart/values-evaluation.yaml" > /tmp/blue-evaluation.yaml
helm template blue "$chart" -f "$chart/values-evaluation.yaml" \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.mode=insecure-http \
  > /tmp/blue-gateway-evaluation.yaml

# Bundled evaluation database: rejected in production, self-contained in evaluation.
if helm template blue "$chart" "${production_network[@]}" \
  --set blue.existingSecret=blue-runtime --set image.digest="$digest" \
  --set database.deployStandalone=true >/dev/null 2>&1; then
  echo "production render unexpectedly accepted database.deployStandalone" >&2
  exit 1
fi
helm template blue "$chart" -f "$chart/values-evaluation.yaml" \
  --set database.deployStandalone=true > /tmp/blue-standalone-database.yaml
grep -q 'kind: StatefulSet' /tmp/blue-standalone-database.yaml
grep -q 'name: "blue-blue-database"' /tmp/blue-standalone-database.yaml
grep -q 'name: wait-for-database' /tmp/blue-standalone-database.yaml
if grep -q 'key: HARNESS_DATABASE_URL' <(grep -A1 'name: "blue-evaluation-runtime"' /tmp/blue-standalone-database.yaml); then
  echo "bundled database render still reads HARNESS_DATABASE_URL from blue.existingSecret" >&2
  exit 1
fi

# Bundled evaluation object store: rejected in production, self-contained in evaluation.
if helm template blue "$chart" "${production_network[@]}" \
  --set blue.existingSecret=blue-runtime --set image.digest="$digest" \
  --set minio.deployStandalone=true >/dev/null 2>&1; then
  echo "production render unexpectedly accepted minio.deployStandalone" >&2
  exit 1
fi
helm template blue "$chart" -f "$chart/values-evaluation.yaml" \
  --set minio.deployStandalone=true > /tmp/blue-standalone-minio.yaml
grep -q 'name: blue-blue-minio' /tmp/blue-standalone-minio.yaml
grep -q 'app.kubernetes.io/component: minio-buckets' /tmp/blue-standalone-minio.yaml
grep -q 'value: "http://blue-blue-minio:9000"' /tmp/blue-standalone-minio.yaml
# The Control API HEADs its buckets and never creates them, so the seeding Job
# must name both of them.
grep -q 'mc mb --ignore-existing blue/raw-sessions' /tmp/blue-standalone-minio.yaml
grep -q 'mc mb --ignore-existing blue/package-artifacts' /tmp/blue-standalone-minio.yaml
