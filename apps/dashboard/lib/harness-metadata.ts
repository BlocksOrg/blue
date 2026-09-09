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
};
