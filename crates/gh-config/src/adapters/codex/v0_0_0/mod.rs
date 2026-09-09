use super::*;
static SUPPORT: GenerationSupport = GenerationSupport { capabilities: &["mcp", "packages", "skills", "plugins", "hooks", "helpers", "gateway"], component_rules: ComponentRules { agents_require_plugin: true, hooks_require_plugin: true, hooks_as_plugin_modules: false } };
pub static SPEC: VersionSpec = VersionSpec { operations: &V1_OPERATIONS, session_upload: Feature::Unsupported("codex v0_0_0 does not support the session-upload lifecycle hook"), session_start: Feature::Unsupported("codex before 0.114.0 has no SessionStart lifecycle hook"), session_resume: Feature::Unsupported("legacy Codex captures are not portable"), support: &SUPPORT };
pub static IMPLEMENTATION: Implementation = Implementation::new(&SPEC);
