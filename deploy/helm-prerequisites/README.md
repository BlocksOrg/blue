# Blue namespace prerequisites

Install this chart before the Blue application chart. It establishes namespace
default-deny and the narrowly scoped DNS/PostgreSQL egress used by the
pre-install migration Job. Keep it as a separate Helm release so these policies
exist before application hooks and remain in place during upgrades.

```bash
helm upgrade --install blue-prerequisites deploy/helm-prerequisites \
  --namespace blue --create-namespace \
  --set 'networkPolicy.databaseCidrs[0]=10.0.0.0/24'
```

On teardown, uninstall the `blue` application release first and this release
last.
