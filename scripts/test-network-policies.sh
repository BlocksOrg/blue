#!/usr/bin/env bash
set -euo pipefail

namespace="${BLUE_NETWORK_TEST_NAMESPACE:-blue-network-test}"
ingress_namespace="${namespace}-ingress"
other_namespace="${namespace}-other"
image="${BLUE_NETWORK_TEST_IMAGE:-busybox:1.36.1}"

cleanup() {
  kubectl delete namespace "$namespace" "$ingress_namespace" "$other_namespace" --ignore-not-found --wait=true --timeout=120s >/dev/null
}
trap cleanup EXIT
cleanup

kubectl create namespace "$namespace"
kubectl create namespace "$ingress_namespace"
kubectl create namespace "$other_namespace"

helm template blue-prerequisites deploy/helm-prerequisites \
  --namespace "$namespace" \
  --set 'networkPolicy.databaseCidrs[0]=198.51.100.0/24' |
  kubectl apply -n "$namespace" -f -

helm template blue deploy/helm \
  --namespace "$namespace" \
  --set blue.existingSecret=blue-runtime \
  --set image.digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
  --set blue.enableInferenceProxy=true \
  --set blue.gatewayType=litellm \
  --set blue.internalTransport.mode=insecure-http \
  --set "networkPolicy.ingressController.namespaceSelector.matchLabels.kubernetes\\.io/metadata\\.name=$ingress_namespace" \
  --set 'networkPolicy.databaseCidrs[0]=198.51.100.0/24' \
  --set 'networkPolicy.redisCidrs[0]=198.51.101.0/24' \
  --set 'networkPolicy.externalHttpsCidrs[0]=203.0.113.0/24' \
  --show-only templates/networkpolicy.yaml |
  kubectl apply -n "$namespace" -f -

create_component() {
  local component="$1" ports="$2"
  kubectl run "$component" -n "$namespace" --image="$image" --labels="app.kubernetes.io/name=blue,app.kubernetes.io/instance=blue,app.kubernetes.io/component=$component" \
    --command -- sh -c "echo ok >/tmp/index.html; for port in $ports; do httpd -p \"\$port\" -h /tmp; done; sleep 3600"
  for port in $ports; do
    kubectl expose pod "$component" -n "$namespace" --name="$component-$port" --port="$port" --target-port="$port"
  done
}

create_component dashboard "3000"
create_component control-api "8080 8082"
create_component inference-proxy "8081"
kubectl run ingress -n "$ingress_namespace" --image="$image" --labels="app.kubernetes.io/name=ingress-nginx" --command -- sleep 3600
kubectl run outsider -n "$other_namespace" --image="$image" --command -- sleep 3600
kubectl wait --for=condition=Ready pod --all -n "$namespace" --timeout=180s
kubectl wait --for=condition=Ready pod --all -n "$ingress_namespace" --timeout=180s
kubectl wait --for=condition=Ready pod --all -n "$other_namespace" --timeout=180s

allow() {
  local ns="$1" pod="$2" url="$3"
  kubectl exec -n "$ns" "$pod" -- timeout 8 wget -qO- "$url" >/dev/null || {
    echo "expected allowed flow failed: $ns/$pod -> $url" >&2
    exit 1
  }
}
deny() {
  local ns="$1" pod="$2" url="$3"
  if kubectl exec -n "$ns" "$pod" -- timeout 4 wget -qO- "$url" >/dev/null 2>&1; then
    echo "expected denied flow succeeded: $ns/$pod -> $url" >&2
    exit 1
  fi
}

allow "$namespace" dashboard "http://control-api-8080:8080"
allow "$namespace" inference-proxy "http://control-api-8082:8082"
allow "$namespace" control-api "http://dashboard-3000:3000"
allow "$ingress_namespace" ingress "http://dashboard-3000.$namespace.svc.cluster.local:3000"
allow "$ingress_namespace" ingress "http://control-api-8080.$namespace.svc.cluster.local:8080"
kubectl exec -n "$namespace" dashboard -- nslookup kubernetes.default.svc.cluster.local >/dev/null

deny "$other_namespace" outsider "http://control-api-8080.$namespace.svc.cluster.local:8080"
deny "$ingress_namespace" ingress "http://control-api-8082.$namespace.svc.cluster.local:8082"
deny "$namespace" dashboard "http://inference-proxy-8081:8081"
deny "$namespace" dashboard "http://1.1.1.1:80"

echo "NetworkPolicy allow/deny probes passed"
