use super::*;
static SUPPORT: GenerationSupport = GenerationSupport { capabilities: &["mcp", "packages", "skills", "plugins", "hooks", "helpers", "gateway", "session_upload", "session_start", "session_resume"], component_rules: ComponentRules { agents_require_plugin: true, hooks_require_plugin: true, hooks_as_plugin_modules: false } };
pub static SPEC: VersionSpec = VersionSpec { operations: &PLUGIN_OPERATIONS, session_upload: Feature::Supported(session_upload_hooks), session_start: Feature::Supported(session_start_hooks), session_resume: Feature::Supported(()), support: &SUPPORT };
pub static IMPLEMENTATION: Implementation = Implementation::new(&SPEC);
pub(crate) use super::{session_start_hooks, session_upload_hooks};
