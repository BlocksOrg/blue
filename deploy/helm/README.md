# Blue Helm chart

This chart deploys the Blue image as independently scalable Control API,
dashboard, inference-proxy, and singleton worker workloads. The image's
all-in-one entrypoint is retained for local development only.
The Control API and worker read `/etc/blue/blue.yaml` and exit at startup when
it is missing. The published image does not bake one in — `deploy/Dockerfile`
creates an empty `/etc/blue` — so `blue.config.existingConfigMap` is required
unless you build an image that supplies the file itself. Runtime secrets are loaded
from `blue.existingSecret`; required keys also use non-optional `secretKeyRef`
entries so Kubernetes reports missing keys before a workload starts.
Offline `helm template` cannot inspect keys inside that externally managed
Secret; it validates the Secret name and renders mandatory key references.

```bash
helm upgrade --install blue-prerequisites ./deploy/helm-prerequisites \
  --namespace blue --create-namespace \
  --set 'networkPolicy.databaseCidrs[0]=10.0.0.0/24'

helm upgrade --install blue ./deploy/helm \
  --namespace blue \
  --set image.repository=ghcr.io/your-org/blue-deployment \
  --set image.digest=sha256:REPLACE_WITH_RELEASE_DIGEST \
  --set blue.existingSecret=blue-runtime
```

The prerequisite release is mandatory in production. It establishes default
deny and migration-only database access before Helm executes the application's
pre-install hook. Uninstall the application release first and prerequisites
last.

Production is the chart default. It rejects mutable image tags, a missing
externally managed Secret, disabled migrations, disabled NetworkPolicy, and more
than one worker replica. For evaluation only, layer
`-f deploy/helm/values-evaluation.yaml`.

The pre-install/pre-upgrade migration Job uses the release image, SQLx migration
lock, a ten-minute deadline, and one retry. Serving replicas never migrate:
startup and `/health/schema` verify every embedded migration and checksum. See
[the migration policy](../runbooks/migrations.md) and the
[production readiness runbook](../runbooks/production-readiness.md).

Production NetworkPolicies default-deny ingress and egress, then permit only the
documented workload flows. Configure database, Redis, object-store, OIDC,
package-host, and upstream-gateway CIDRs under `networkPolicy`; standard
NetworkPolicy cannot select DNS names. Ingress and monitoring selectors extend
the baseline without replacing it.

The chart assumes database, object storage, ingress, DNS, certificates, and an
optional LiteLLM-compatible gateway already exist. See `../tofu/aws` for the
AWS dependency starter.

Gateway deployments should enable `blue.enableInferenceProxy`, set
`blue.gatewayType` to the same value as `gateway.type` in `blue.yaml`, set
`blue.publicUrls.inferenceProxy` and `ingress.proxyHost`, configure the
`blue.inferenceJwt` signing-key Secret, active key ID, and audience, and provide
`HARNESS_GATEWAY_URL`, `HARNESS_PROXY_OAUTH_CLIENT_SECRET`, and gateway
encryption settings through the runtime Secret. The inference proxy authenticates
to the Control API with a short-lived OAuth2 client-credentials token minted by
better-auth (the dashboard); `HARNESS_PROXY_OAUTH_CLIENT_SECRET` is the shared
secret consumed by both the dashboard (to seed the confidential client) and the
proxy (to obtain tokens), so it must live in the shared `blue.existingSecret`.
The client id defaults to `blue.inferenceProxyClientId` (`blue-inference-proxy`).
Internal resolver, invalidation, and batched-log traffic uses a private
ClusterIP Service on port 8082. Choose the transport explicitly with
`blue.internalTransport.mode`:

- `mtls` is the default and recommended mode. It encrypts decrypted virtual
  keys in transit and authenticates both workloads. Set
  `blue.internalTransport.serverSecret` to a Secret containing `ca.crt`,
  `tls.crt`, and `tls.key`; the server certificate SAN must cover
  `<release>-control-api-internal`. Set `blue.internalTransport.clientSecret`
  to a Secret containing `ca.crt` and `client.pem`, where `client.pem` contains
  the proxy certificate followed by its private key.
- `insecure-http` disables transport encryption and certificate authentication.
  OAuth M2M remains mandatory, but decrypted virtual keys cross the pod network
  in plaintext. Use it only on a private, trusted network with enforced
  NetworkPolicy and tightly controlled workload and namespace access. An API
  gateway does not protect this east-west hop. Omit certificate Secrets.

Mounted certificate files are checked every 30 seconds. Valid rotations are
adopted without dropping in-flight requests; invalid or partial updates retain
the last-known-good configuration and produce a warning.

Custom executable provisioners can be delivered independently from the Blue image.
Set `blue.provisionerExecutable.enabled=true`, pin its OCI image by digest, and pin
the SHA-256 of `/executable/provisioner`. An init container verifies and copies
the executable to `/var/run/blue/provisioner/provisioner` with mode `0555`; configure that path
under `gateway.provisioner.executable_path`. The Control API mount is read-only.

The proxy HPA includes the `gateway_proxy_active_streams` Pods metric. Install a
custom-metrics adapter that exposes it, or disable autoscaling until one is
available. Configure ingress streaming timeouts to at least one hour; proxy
pods use a ten-minute termination grace period for connection draining.

Before production cutover, run `scripts/load-gateway.js` twice with k6: once in
the default `rps` mode for 2,500 requests/second and once with `MODE=streams`
for 25,000 concurrent streams. Supply `PROXY_URL`, `INFERENCE_TOKEN`, and a gateway
model intended for load testing.
