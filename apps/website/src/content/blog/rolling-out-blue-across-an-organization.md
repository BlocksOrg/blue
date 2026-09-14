---
title: "What Rolling Out Blue Across an Organization Looks Like"
description: "Connect workforce identity, coding agents, extensions, and an existing inference gateway through one self-hosted governance layer."
publishDate: 2026-09-13
author: "Blue team"
---

Rolling out coding agents across an organization is not just a software-installation problem. Agents must reach approved models through the right gateway, receive compatible configuration and extensions, and keep working as upstream clients change.

Blue gives platform teams one self-hosted layer for that delivery path. They connect Blue to workforce identity and an existing inference gateway, then publish policy for the native agents developers already use. Developers install Blue, sign in, and launch a governed agent without learning a replacement interface.

## Start with infrastructure the organization controls

A Blue rollout starts with infrastructure. Its control plane, policy data, package artifacts, and optional session data remain in an environment chosen by the organization. The maintained production path uses Kubernetes and Helm, with PostgreSQL and S3-compatible storage. Organizations routing inference through their own gateway also deploy Blue's inference proxy.

Existing controls for networking, secrets, encryption, observability, backups, and regional placement continue to apply. The server remains the authority: it decides which agents and versions are allowed, what configuration and extensions they receive, and whether traffic uses the organization-operated gateway. A client can downgrade to governance-only mode, but it cannot enable gateway mode or opt into an unapproved agent.

## Connect workforce identity once

Blue connects governance to the workforce lifecycle through OIDC sign-in and SCIM 2.0 provisioning. SCIM creates accounts, synchronizes organization and group membership, and deactivates access when someone leaves. OIDC authenticates the person through the organization's identity provider.

At the command line, OAuth device authorization sends the developer to a short-lived browser flow. Passwords never pass through the terminal, and Blue issues dedicated CLI credentials rather than reusing a browser session. Employees and contractors get the same entry point, while SCIM groups can determine which models, extensions, and policies each population receives.

## Adapt Blue to the gateway you already operate

Many organizations already encode tenants, teams, budgets, model allowlists, and rate limits in an inference gateway. Blue adapts to that model through a custom gateway provisioner rather than imposing a new identity scheme.

After authentication, Blue passes deployment-trusted code a small canonical identity that includes the user, organization, and SCIM groups. The provisioner maps that identity to the gateway's accounts and policies, and can create, reconcile, rotate, or revoke access as needed.

Durable gateway credentials remain encrypted on the server. The coding agent receives a short-lived, session-bound token, which Blue's inference proxy validates before substituting the real credential upstream. Developers authenticate with the organization instead of managing provider keys on their machines.

## Publish one coding-agent policy

Administrators next publish what each authenticated developer should receive. Blue policy allows Codex, Claude Code, Kimi Code, and OpenCode, constrains compatible versions, and delivers model settings, approval and sandbox rules, MCP servers, skills, plugins, hooks, subagents, and helper binaries.

Blue checks the installed CLI before changing configuration or launching it, then selects a compiled compatibility profile for that version. If an installed version falls outside policy, an interactive launch can offer to install a supported one with the developer's confirmation. The initial native CLI must already be present, usually through the organization's normal software-distribution channel.

Version-aware adapters translate organization-level policy into the files, environment, arguments, and extension layouts each client understands. Every saved policy becomes a revision that clients can reconcile during launch, through `blue apply`, or through background polling. Artifacts are pinned and verified, writes are atomic, and Blue blocks an incompatible state instead of guessing at a vendor format.

## Make the developer path short

Once the organizational setup is in place, the developer path has three steps:

1. Install Blue and receive at least one approved native coding-agent CLI through the organization's normal software distribution.
2. Connect Blue to the company's public Control API URL.
3. Authenticate in the browser with the same workforce identity used for other internal tools.

A bare `blue` launch guides the developer through any missing connection or authentication step, fetches personalized policy, checks compatibility, reconciles configuration and extensions, obtains gateway access when enabled, and starts the native CLI.

The upstream agent still owns its terminal UI, arguments, signals, resize behavior, and exit code. Developers avoid copying gateway keys, following agent-specific setup guides, or manually placing extensions, while retaining the native tools they know.

## Operate the lifecycle, not a one-time rollout

Onboarding is only the beginning. Blue keeps common changes inside explicit control loops:

- SCIM updates group membership and deactivates identities as the workforce directory changes.
- Session revocation invalidates browser, CLI, and gateway sessions together.
- Governance revisions deliver configuration and extension changes without repackaging the Blue client.
- Version ranges and certified compatibility ceilings keep untested agent releases from silently receiving the wrong configuration.
- Gateway reconciliation updates or revokes organization-managed credentials as identity and policy change.
- Client inventory gives administrators visibility into installed agents, versions, and reconciliation health.

Policy follows the authenticated user and is translated locally for an approved agent. The organization operates one governance system; developers sign in and use native tools.

Start with the [production deployment contract](https://docs.bluee.sh/next/deployment/runtime-contract), then review [identity provisioning](https://docs.bluee.sh/next/admin/identity-provisioning), [managed packages](https://docs.bluee.sh/next/admin/managed-packages), and [gateway access](https://docs.bluee.sh/next/admin/gateway-access).
