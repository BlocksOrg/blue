use super::*;
static SUPPORT: GenerationSupport = GenerationSupport { capabilities: &["mcp", "packages", "skills", "agents", "plugins", "helpers", "gateway", "session_upload", "session_start", "session_resume"], component_rules: ComponentRules { agents_require_plugin: false, hooks_require_plugin: false, hooks_as_plugin_modules: true } };
pub static SPEC: VersionSpec = VersionSpec { operations: &V1_OPERATIONS, session_upload: Feature::Supported(session_upload_plugin), session_start: Feature::Supported(()), session_resume: Feature::Supported(()), support: &SUPPORT };
pub static IMPLEMENTATION: Implementation = Implementation::new(&SPEC);
pub(crate) use super::session_upload_plugin;
