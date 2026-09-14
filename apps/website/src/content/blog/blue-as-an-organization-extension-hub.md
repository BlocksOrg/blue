---
title: "Use Blue as Your Organization's Extension Hub"
description: "Keep coding-agent extensions governed when gateway-based access replaces vendor-managed entrypoints."
publishDate: 2026-09-13
author: "Blue team"
---

Organizations often begin their coding-agent rollout through a vendor-managed application. Employees authenticate with a company account, administrators approve a set of plugins, and the vendor workspace becomes the place where those extensions are discovered and distributed.

That model changes when the organization moves inference behind its own gateway. Developers may use gateway profiles instead of authenticating each coding agent through the vendor's managed entrypoint. In that setup, the workspace-linked path for distributing organization extensions may no longer follow the user.

The organization then needs to decide where extension curation and delivery will live. Some gateway platforms provide MCP catalogs, skills hubs, or both. Blue offers another option for teams that want to distribute extensions through the same policy layer that governs their coding-agent CLIs.

Here is what that looks like with Blue.

## Put extension choices in Blue policy

With Blue, administrators define the approved extension set alongside the policy for each supported coding agent. That set can include skills, plugins, hooks, subagent definitions, MCP servers, and helper executables.

This does not require the gateway to be limited to inference routing. A gateway can continue to provide its own MCP or skills capabilities. The organization chooses which system is the source of truth for each capability and uses Blue for the extensions that should be applied to local coding-agent clients.

For organizations that previously relied on a vendor workspace to curate and distribute plugins, Blue can take over that function while gateway profiles handle inference access.

## Use one catalog across coding agents

Blue gives the organization one curated extension catalog for Codex, Claude Code, Kimi Code, and OpenCode. Platform teams choose the packages that meet their security and operational requirements, then map each package to the native capabilities supported by each agent.

This matters because the underlying formats differ. One agent may expect a plugin directory, another a skill folder or hook configuration, and another a JavaScript plugin module. Blue translates the organization-level selection into the representation supported by the installed agent and version.

The organization can therefore maintain one approved collection in Blue and let its adapters handle the supported coding-agent formats.

## Publish through policy

Today, a Blue administrator can select extensions in the dashboard and publish them as part of an immutable governance revision. A package can be assigned to everyone or to selected members, with harness-specific settings where required.

Clients reconcile that policy during a governed launch, an explicit apply, or background polling for the default agent. Package contents are pinned by digest before activation. When an administrator removes a package, Blue removes its activation during the next reconciliation and protects locally modified content from silent deletion.

The result is a lifecycle the organization controls:

1. Curate extensions from approved public or private sources.
2. Review the capabilities and executable content they contain.
3. Publish them to the intended people and coding agents.
4. Update or revoke them through the same policy channel.

With this model, developers receive the selected extensions through the same governed launch path they already use for Blue.

## Add controlled self-service

Central publication is the right default for required extensions. It is not the only useful model.

Blue is also preparing developer self-service for organization-approved extensions. Instead of opening every public marketplace, the platform team will define the available catalog and developers will choose from that reviewed set. This preserves organizational control while giving teams room to adopt specialized tools when they need them.

Required packages and optional choices can then share the same source of truth. The organization controls what is available. Policy controls what must be present. Developers choose among approved options where flexibility is appropriate.

## Choose the extension hub that fits the operating model

An organization may place extension discovery in its gateway, in Blue, or across both systems with a clear source of truth for each capability. The right choice depends on how the company wants to govern local coding-agent configuration and package delivery.

When Blue is the extension hub, it provides the catalog, policy, compatibility, and client reconciliation while developers continue using native coding agents. It can replace the extension distribution function that a vendor-managed entrypoint previously supplied while leaving the organization free to use the gateway's other capabilities.

The result is an extension layer tied to the organization's Blue policy rather than a single vendor authentication path. Gateway adoption can then proceed without losing the curated tools and workflows developers rely on.

See [managed packages](https://docs.bluee.sh/next/admin/managed-packages) for the current extension catalog, publication, targeting, and reconciliation workflow.
