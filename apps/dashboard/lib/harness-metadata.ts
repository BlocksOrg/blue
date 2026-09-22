export type HarnessMetadata = {
  key: string;
  aliases: string[];
  label: string;
  description: string;
  binary_names: string[];
  install_command_template: string;
  capabilities: string[];
  component_rules: {
    agents_require_plugin: boolean;
    hooks_require_plugin: boolean;
    hooks_as_plugin_modules: boolean;
  };
  generations: Array<{
    profile: string;
    introduced: string;
    before: string | null;
    verified_before: string;
    lifecycle: "supported" | "deprecated";
    capabilities: string[];
    component_rules: {
      agents_require_plugin: boolean;
      hooks_require_plugin: boolean;
      hooks_as_plugin_modules: boolean;
    };
  }>;
};

export type HarnessMetadataResponse = {
  contract_version: number;
  harnesses: HarnessMetadata[];
  effective_verified_ceilings?: Record<string, Record<string, string>>;
  known_versions_source?: "compiled" | "public_manifest";
  known_versions_refreshed_at?: string | null;
};

export function effectiveHarnesses(response: HarnessMetadataResponse): HarnessMetadata[] {
  return response.harnesses.map((harness) => ({
    ...harness,
    generations: harness.generations.map((generation) => ({
      ...generation,
      verified_before:
        response.effective_verified_ceilings?.[harness.key]?.[generation.profile] ??
        generation.verified_before,
    })),
  }));
}
