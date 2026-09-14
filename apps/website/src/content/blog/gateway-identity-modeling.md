---
title: "Make Blue Fit Your Gateway's Identity and Key Model"
description: "Use a custom gateway provisioner to map authenticated users and create gateway keys with your organization's access policy."
publishDate: 2026-09-13
author: "Blue team"
---

To create the right gateway key for each user, Blue must map the authenticated Blue identity to the correct gateway account. A custom provisioner defines that mapping and creates a key on the user's behalf with the required teams, models, budgets, rate limits, expiration, and other attributes.

Gateway access models vary by organization. Account ownership and key policy may depend on internal employee IDs, tenants, cost centers, group membership, or rules that exist outside Blue. Custom provisioning applies those rules without building them into Blue's authentication flow.

## Where the custom provisioner fits

A custom provisioner is a deployment-trusted executable run by the Blue Control API. It resolves the account, calculates the desired key policy, and makes the gateway match that state.

It does **not** replace Blue's login flow or authenticate the developer itself. Blue supports its configured login and identity-provisioning path first. After the user is authenticated, the provisioner receives a small canonical identity containing:

```json
{
  "id": "7d4c1a92-...",
  "email": "developer@example.com",
  "organization_id": "8a320bdd-...",
  "groups": ["Platform", "AI Early Access"]
}
```

The executable can use those fields to call whatever trusted systems are necessary. Its account-resolution work might:

1. Resolve the Blue email to an immutable employee ID in an internal directory.
2. Look up the employee's gateway tenant and cost center.
3. Find or create the correct gateway account under that tenant.

Its key-management work might then:

1. Translate SCIM groups into gateway teams and a model allowlist.
2. Apply a budget, rate limits, expiration, and organization-specific metadata.
3. Create a key when access is missing, update it when policy changes, or rotate it when invalid.
4. Return the credential once so Blue can encrypt it, along with a non-secret ID for future reconciliation and revocation.

This supports custom OIDC providers, SCIM directories, acquired-company domains, contractor directories, and organization-specific credential policies without changing every coding-agent adapter.

## The provisioning lifecycle

Blue invokes the executable with no command-line arguments. It writes one versioned JSON request to stdin and expects one JSON response on stdout.

An `ensure` request contains the authenticated identity and a reason. When Blue has provisioned the user before, it also includes safe information about the previous credential:

```json
{
  "protocol_version": 1,
  "operation": "ensure",
  "request": {
    "identity": {
      "id": "7d4c1a92-...",
      "email": "developer@example.com",
      "organization_id": "8a320bdd-...",
      "groups": ["Platform"]
    },
    "reason": "reconciliation_due",
    "previous": {
      "external_id": "gateway-key-1842",
      "alias": "blue:employee-1049",
      "metadata": { "tenant_id": "engineering" }
    }
  }
}
```

The reason tells the provisioner why it is running:

- `missing` creates access for a user who has no managed credential.
- `configuration_changed` reapplies policy after the Blue configuration or provisioner policy revision changes.
- `reconciliation_due` checks the existing account and key after the configured interval.
- `credential_invalidated` replaces a key that the gateway has confirmed is deleted or blocked.

For an existing valid key, the provisioner should reconcile the complete desired policy, including teams, models, budgets, rate limits, expiration, and gateway-specific controls. If the gateway can update the key in place, the provisioner returns `credential: null` and Blue retains its encrypted credential. For a new or rotated key, the response includes the plaintext credential once, along with a non-secret external identifier:

```json
{
  "protocol_version": 1,
  "status": "success",
  "result": {
    "credential": "gw-secret-returned-once",
    "external_id": "gateway-key-1842",
    "alias": "blue:employee-1049",
    "metadata": { "tenant_id": "engineering" },
    "expires_at": null
  }
}
```

Blue envelope-encrypts the credential and stores it server-side. It never puts that durable key in the developer's agent configuration. At launch, Blue gives the agent a session-bound inference JWT; the inference proxy validates it, resolves the corresponding encrypted gateway credential, and substitutes that credential when forwarding the request upstream.

The `external_id`, alias, and metadata are deliberately separate from the credential. Use them to remember stable account and key mappings without putting secrets into reconciliation requests. When access is removed, Blue calls the same executable with a `revoke` operation and the external ID so the gateway key can be disabled or deleted.

## Treat the executable as privileged deployment code

The provisioner can create credentials, so Blue treats it as part of the trusted server deployment:

- Configure an absolute executable path, a lowercase SHA-256 digest, and a policy revision. Blue verifies the file and digest during Control API startup.
- Increment `policy_revision` when mapping or key policy changes so existing credentials are reconciled.
- Keep the gateway administrator credential and directory credentials in the Control API environment, not in `blue.yaml`.
- Reserve stdout for exactly one protocol response. Never log request stdin, credentials, or successful response JSON.
- Make operations idempotent. A governed launch, personalized configuration fetch, or retry can run ensure again.
- Use the typed errors to distinguish missing accounts, conflicts, invalid credentials, temporary unavailability, and gateway rejection.

Blue bounds executable runtime and output, serializes lifecycle work per user, and stores no partial provisioning result when ensure fails. Those safeguards protect the host, but the mapping and authorization decisions still belong to your provisioner. Test them against a non-production gateway before deploying the pinned artifact.

## Keep the boundary narrow

The custom provisioner exists so the rest of the system can stay generic. Blue supplies a stable authenticated identity and owns encrypted credential storage, reconciliation triggers, session tokens, and agent-specific routing. Your executable owns the narrow translation into the gateway's account model and key policy.

That boundary lets you change identity providers, directory rules, account layouts, or credential controls without changing the native developer experience or copying long-lived gateway keys onto developer machines.

Read the complete [custom gateway provisioner protocol](https://docs.bluee.sh/next/admin/custom-gateway-provisioners), [gateway access lifecycle](https://docs.bluee.sh/next/admin/gateway-access), and [gateway-mode architecture](https://docs.bluee.sh/next/concepts/gateway-mode) before deploying an implementation.
