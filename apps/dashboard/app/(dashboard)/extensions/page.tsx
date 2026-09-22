import { api, requireAdminIdentity } from "../../../lib/api";
import {
  PackageManager,
  type ManagedPackage,
  type PackageAudience,
  type PackageSourceConnection,
} from "../configuration/package-manager";
import type { McpByHarness, McpServer } from "../configuration/mcp-manager";
import { effectiveHarnesses, type HarnessMetadataResponse } from "@/lib/harness-metadata";

type PackageOverride = {
  enabled?: boolean;
  settings?: Record<string, unknown>;
};

type GovernanceDocument = {
  packages?: ManagedPackage[];
  harnesses?: Record<
    string,
    {
      package_overrides?: Record<string, PackageOverride>;
      mcp?: McpServer[];
    }
  >;
};

export default async function Extensions() {
  await requireAdminIdentity();
  const [config, catalog, connections, metadata] = await Promise.all([
    api<{
      revision: string;
      document: GovernanceDocument;
      package_audiences?: Record<string, PackageAudience>;
    }>(
      "/admin/governance-config",
    ),
    api<ManagedPackage[]>("/admin/package-catalog"),
    api<PackageSourceConnection[]>("/admin/package-source/connections"),
    api<HarnessMetadataResponse>("/harness-metadata"),
  ]);
  const overrides = Object.fromEntries(
    Object.entries(config.document.harnesses ?? {}).map(([harness, policy]) => [
      harness,
      policy.package_overrides ?? {},
    ]),
  );
  const mcp = Object.fromEntries(
    Object.entries(config.document.harnesses ?? {}).map(([harness, policy]) => [
      harness,
      policy.mcp ?? [],
    ]),
  ) as McpByHarness;

  return (
    <PackageManager
      revision={config.revision}
      catalog={catalog}
      selected={config.document.packages ?? []}
      audiences={config.package_audiences ?? {}}
      overrides={overrides}
      connections={connections}
      mcp={mcp}
      harnesses={effectiveHarnesses(metadata)}
    />
  );
}
