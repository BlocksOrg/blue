use super::*;
static SUPPORT: GenerationSupport = GenerationSupport { capabilities: &["mcp", "packages", "helpers", "gateway"], component_rules: ComponentRules { agents_require_plugin: false, hooks_require_plugin: false, hooks_as_plugin_modules: false } };
pub static SPEC: VersionSpec = VersionSpec { operations: &PRE_HOOKS_OPERATIONS, session_upload: Feature::Unsupported("claude v0_0_0 does not support the session-upload lifecycle hook"), session_start: Feature::Unsupported("Claude before 1.0.38 does not support hooks"), session_resume: Feature::Unsupported("legacy Claude captures are not portable"), support: &SUPPORT };
pub static IMPLEMENTATION: Implementation = Implementation::new(&SPEC);
