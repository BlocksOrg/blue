import { api, requireAdminIdentity } from "@/lib/api";
import { HarnessConfigEditor } from "./harness-config-editor";
import { effectiveHarnesses, type HarnessMetadataResponse } from "@/lib/harness-metadata";

type HarnessManagedConfigs = {
  revision: string;
  configurations: Record<string, string>;
  version_requirements: Record<string, string | null>;
  allow_unverified_versions: Record<string, boolean>;
};

export default async function Harnesses() {
  await requireAdminIdentity();
  const [value, metadata] = await Promise.all([
    api<HarnessManagedConfigs>("/admin/harnesses/managed-configs"),
    api<HarnessMetadataResponse>("/harness-metadata"),
  ]);

  return (
    <HarnessConfigEditor
      revision={value.revision}
      configurations={value.configurations}
      versionRequirements={value.version_requirements}
      unverifiedOverrides={value.allow_unverified_versions}
      harnesses={effectiveHarnesses(metadata)}
    />
  );
}
