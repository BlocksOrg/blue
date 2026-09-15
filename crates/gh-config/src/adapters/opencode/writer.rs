//! Shared OpenCode-family launch overlay: `OPENCODE_CONFIG_CONTENT` carries the governed
//! runtime overrides and `OPENCODE_CONFIG_DIR` exposes managed components.

use std::path::Path;

use gh_common::GhError;
use gh_gateway::GatewayWiring;
use gh_service::HarnessPolicy;
use serde_json::{json, Map, Value};

use crate::util::json_pretty;
use crate::HarnessWrite;

const GOVERNED_PROVIDER: &str = "governed";
const GATEWAY_TOKEN_ENV: &str = "BLUE_OPENCODE_GATEWAY_TOKEN";
const ADDITIONAL_GOVERNED_MODELS: &[&str] = &["openai/gpt-5.6-sol"];
const SCHEMA: &str = "https://opencode.ai/config.json";

pub fn write(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    policy: &HarnessPolicy,
    wiring: Option<&GatewayWiring>,
    session_upload_plugin: Option<String>,
) -> Result<HarnessWrite, GhError> {
    let native_path = home.join(".config").join("opencode").join("opencode.json");
    migrate_legacy_global_config(plan, home, &native_path, policy)?;
    let native = plan.read_json_object(&native_path)?;
    let runtime = crate::managed_runtime_dir(home).join("opencode");
    let cfg_path = runtime.join("opencode.json");
    let mut root = Map::new();
    root.insert("$schema".to_string(), json!(SCHEMA));

    let catalog = governed_catalog(policy);
    let mut warnings = Vec::new();
    // In gateway mode the model is always pinned: `OPENCODE_CONFIG_CONTENT`
    // merges over the user's global config, so leaving `model` unset lets a
    // native `anthropic/...` selection survive next to
    // `enabled_providers: ["governed"]` — a model OpenCode cannot reach.
    let effective_model = match (&policy.managed_config.model, wiring) {
        (Some(model), _) => Some(model.clone()),
        (None, None) => None,
        (None, Some(_)) => {
            let model = native_governed_model(&native, &catalog)
                .unwrap_or_else(|| catalog[0].clone());
            warnings.push(format!(
                "`harnesses.opencode.managed_config.model` is unset; defaulting to `{model}`. \
                 Set it in your governance config to pin the model your gateway serves."
            ));
            Some(model)
        }
    };

    if let Some(model) = &effective_model {
        let model_value = if wiring.is_some() {
            format!("{GOVERNED_PROVIDER}/{model}")
        } else {
            model.clone()
        };
        root.insert("model".into(), json!(model_value));
    }

    let mut permissions = Map::new();
    if let Value::Object(desired) = permission_block(policy) {
        permissions.extend(desired);
    }
    root.insert("permission".into(), Value::Object(permissions));
    merge_mcp_block(&mut root, &native, policy);

    let mut files = Vec::new();
    let mut env = std::collections::BTreeMap::new();

    if let Some(w) = wiring {
        // Register a complete OpenAI-compatible provider. OpenCode does not
        // infer a runtime package or model catalog for arbitrary provider IDs,
        // so a baseURL-only entry looks valid but cannot make a request.
        let mut providers = Map::new();
        let mut models = Map::new();
        if let Some(model) = &effective_model {
            models.insert(model.clone(), json!({ "name": model }));
        }
        for available in &catalog {
            models
                .entry(available.clone())
                .or_insert_with(|| json!({ "name": available }));
        }
        providers.insert(
            GOVERNED_PROVIDER.into(),
            json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": "Governed (Blue)",
                "options": {
                    "baseURL": w.base_url,
                    "apiKey": format!("{{env:{GATEWAY_TOKEN_ENV}}}")
                },
                "models": models
            }),
        );
        root.insert("provider".into(), Value::Object(providers));
        root.insert(
            "enabled_providers".into(),
            Value::Array(vec![json!(GOVERNED_PROVIDER)]),
        );
        env.insert(GATEWAY_TOKEN_ENV.into(), w.token.clone());
    }

    let plugin_path = crate::managed_runtime_dir(home)
        .join("opencode")
        .join("plugins")
        .join("blue-session-upload.js");
    if let Some(plugin) = session_upload_plugin {
        plan.write(&plugin_path, plugin)?;
        files.push(plugin_path);
    } else if let Ok(contents) = std::fs::read_to_string(&plugin_path) {
        if contents.starts_with("// Managed by Blue.") {
            plan.remove(&plugin_path)?;
        }
    }

    let config_value = Value::Object(root);
    let mut config = serde_json::to_string_pretty(&config_value)
        .map_err(|error| GhError::Serde(error.to_string()))?;
    config.push('\n');
    plan.write(&cfg_path, config.as_bytes())?;
    files.insert(0, cfg_path);
    // OpenCode loads OPENCODE_CONFIG before project configuration, so a local
    // opencode.json can otherwise replace Blue's provider/model. Inline content
    // is the documented runtime-override tier and loads after project config.
    env.insert("OPENCODE_CONFIG_CONTENT".into(), config);
    // The directory makes adjacent agents, skills, commands and plugins
    // discoverable; it is not a substitute for the runtime config overlay.
    env.insert("OPENCODE_CONFIG_DIR".into(), runtime.display().to_string());

    Ok(HarnessWrite {
        files,
        env,
        launch_args: Vec::new(),
        package_errors: Vec::new(),
        warnings,
    })
}

/// The models the governed provider advertises, newest policy first: an
/// admin-supplied `managed_config.available_models` when present (the gateway
/// is the only thing that knows what it actually serves), else the built-in
/// default. Never empty, so the provider block is always renderable.
fn governed_catalog(policy: &HarnessPolicy) -> Vec<String> {
    let configured = policy
        .managed_config
        .extra
        .get("available_models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|models| !models.is_empty());
    configured.unwrap_or_else(|| {
        ADDITIONAL_GOVERNED_MODELS
            .iter()
            .map(|model| (*model).to_owned())
            .collect()
    })
}

/// The user's native model, but only when the gateway is known to serve it.
/// Both the bare name and the `<provider>/`-prefixed spelling are accepted, so
/// a native `governed/openai/gpt-x` or `openai/gpt-x` resolves to the same
/// catalog entry. Anything else is a provider `enabled_providers` disables.
fn native_governed_model(native: &Map<String, Value>, catalog: &[String]) -> Option<String> {
    let model = native.get("model").and_then(Value::as_str)?;
    let stripped = model.split_once('/').map(|(_, rest)| rest);
    [Some(model), stripped]
        .into_iter()
        .flatten()
        .find(|candidate| catalog.iter().any(|entry| entry == candidate))
        .map(str::to_owned)
}

pub(super) fn disable_autoupdate(config: &str) -> Result<String, GhError> {
    let mut root: Value = serde_json::from_str(config)
        .map_err(|error| GhError::Serde(format!("parsing managed OpenCode config: {error}")))?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| GhError::config("managed OpenCode config must be a JSON object"))?;
    object.insert("autoupdate".into(), json!(false));
    String::from_utf8(json_pretty(&root)?)
        .map_err(|error| GhError::Serde(format!("encoding managed OpenCode config: {error}")))
}

#[allow(clippy::too_many_arguments)]
pub fn apply_packages(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    report: &mut HarnessWrite,
    plugin_modules: &[std::path::PathBuf],
    skills_dirs: &[std::path::PathBuf],
    agents_dirs: &[std::path::PathBuf],
    hooks_files: &[std::path::PathBuf],
    helpers: &std::collections::BTreeMap<String, std::path::PathBuf>,
) -> Result<(), GhError> {
    let runtime = crate::managed_runtime_dir(home).join("opencode");
    // OpenCode 1.17+ can accept legacy `plugin` array entries without loading
    // them. Files directly under an OPENCODE_CONFIG_DIR plugins directory are
    // the stable, auto-discovered form. Preserve Blue's built-in upload plugin
    // while replacing package-provided plugins on every reconciliation.
    let plugins_target = runtime.join("plugins");
    let session_plugin = plugins_target.join("blue-session-upload.js");
    let session_plugin_contents = plan.read(&session_plugin).ok();
    plan.component_files(plugin_modules, &plugins_target, &mut report.files)?;
    if let Some(contents) = session_plugin_contents {
        plan.write(&session_plugin, contents)?;
    }
    plan_skills(
        plan,
        skills_dirs,
        &runtime.join("skills"),
        &mut report.files,
    )?;
    plan.component_dirs(agents_dirs, &runtime.join("agents"), &mut report.files)?;
    plan.component_files(hooks_files, &runtime.join("hooks"), &mut report.files)?;
    crate::util::prepend_helper_paths(&mut report.env, helpers);
    Ok(())
}

fn migrate_legacy_global_config(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    path: &Path,
    policy: &HarnessPolicy,
) -> Result<(), GhError> {
    let mut root = plan.read_json_object(path)?;
    let mut changed = false;
    if let Some(model) = &policy.managed_config.model {
        let governed = format!("{GOVERNED_PROVIDER}/{model}");
        if root
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|value| value == model || value == governed)
        {
            root.remove("model");
            changed = true;
        }
    }
    if let Some(Value::Object(providers)) = root.get_mut("provider") {
        changed |= providers.remove(GOVERNED_PROVIDER).is_some();
        if providers.is_empty() {
            root.remove("provider");
        }
    }
    if let Some(Value::Array(enabled)) = root.get_mut("enabled_providers") {
        let before = enabled.len();
        enabled.retain(|value| value.as_str() != Some(GOVERNED_PROVIDER));
        changed |= enabled.len() != before;
        if enabled.is_empty() {
            root.remove("enabled_providers");
        }
    }
    if changed {
        plan.write(path, json_pretty(&Value::Object(root))?)?;
    }

    let auth_path = home.join(".local/share/opencode/auth.json");
    let mut auth = plan.read_json_object(&auth_path)?;
    if auth.remove(GOVERNED_PROVIDER).is_some() {
        plan.write(&auth_path, json_pretty(&Value::Object(auth))?)?;
    }
    let plugin = home.join(".config/opencode/plugins/blue-session-upload.js");
    if std::fs::read_to_string(&plugin)
        .is_ok_and(|contents| contents.starts_with("// Managed by Blue."))
    {
        plan.remove(&plugin)?;
    }
    Ok(())
}

fn permission_block(policy: &HarnessPolicy) -> Value {
    // Default to the reference "allow everything except question" posture; if
    // the policy disables auto-approve, gate the mutating tools to "ask".
    let action = if policy.managed_config.auto_approve == Some(false) {
        "ask"
    } else {
        "allow"
    };
    json!({
        "edit": action, "write": action, "bash": action, "read": "allow",
        "glob": "allow", "grep": "allow", "fetch": "allow", "mcp": "allow",
        "external_directory": action, "list": "allow", "task": "allow",
        "skill": "allow", "todoread": "allow", "todowrite": "allow",
        "question": "deny"
    })
}

fn merge_mcp_block(
    root: &mut Map<String, Value>,
    native: &Map<String, Value>,
    policy: &HarnessPolicy,
) {
    let local = native
        .get("mcp")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let mut mcp = Map::new();
    for s in &policy.mcp {
        if s.disabled || local.contains_key(&s.name) {
            continue;
        }
        let mut entry = Map::new();
        if let Some(cmd) = &s.command {
            // OpenCode: command+args collapse into one array; env → environment.
            let mut command = vec![cmd.clone()];
            command.extend(s.args.iter().cloned());
            entry.insert("type".into(), json!("local"));
            entry.insert("enabled".into(), json!(true));
            entry.insert("command".into(), json!(command));
            if !s.env.is_empty() {
                entry.insert("environment".into(), json!(s.env));
            }
        } else if let Some(url) = &s.url {
            entry.insert("type".into(), json!("remote"));
            entry.insert("enabled".into(), json!(true));
            entry.insert("url".into(), json!(url));
        }
        mcp.insert(s.name.clone(), Value::Object(entry));
    }
    root.insert("mcp".into(), Value::Object(mcp));
}

fn plan_skills(
    plan: &mut crate::adapters::ReconcilePlan,
    sources: &[std::path::PathBuf],
    target: &Path,
    files: &mut Vec<std::path::PathBuf>,
) -> Result<(), GhError> {
    plan.remove(target)?;
    for source in sources {
        let destination = if source.join("SKILL.md").is_file() {
            target.join(
                source
                    .file_name()
                    .ok_or_else(|| GhError::config("skill has no directory name"))?,
            )
        } else {
            target.to_path_buf()
        };
        plan.component_tree(source, &destination)?;
    }
    if !sources.is_empty() {
        files.push(target.to_path_buf());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn test_write(
        home: &Path,
        policy: &HarnessPolicy,
        wiring: Option<&GatewayWiring>,
        session_upload_plugin: Option<String>,
    ) -> Result<HarnessWrite, GhError> {
        let mut plan = crate::adapters::ReconcilePlan::default();
        let result = write(&mut plan, home, policy, wiring, session_upload_plugin)?;
        let mut transaction = crate::FileTransaction::begin(home, &plan)?;
        transaction.apply(&plan)?;
        transaction.commit()?;
        Ok(result)
    }

    #[test]
    fn preserves_existing_provider_permissions_and_mcp() {
        let home = std::env::temp_dir().join(format!("gh-opencode-merge-{}", std::process::id()));
        let path = home.join(".config/opencode/opencode.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            br#"{
              "custom": true,
              "model": "governed/model",
              "provider": {"personal": {"options": {"baseURL": "http://localhost"}}, "governed": {"options": {"baseURL": "http://proxy"}}},
              "enabled_providers": ["personal", "governed"],
              "permission": {"custom_tool": "ask"},
              "mcp": {"personal": {"type": "local", "command": ["mine"]}}
            }"#,
        )
        .unwrap();
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "model": "governed/model" },
            "mcp": [
                { "name": "personal", "command": "remote-must-not-win" },
                { "name": "blocks", "command": "npx", "args": ["blocks"] },
                { "name": "disabled-remote", "command": "disabled", "disabled": true }
            ]
        }))
        .unwrap();

        test_write(
            &home,
            &policy,
            None,
            Some(crate::adapters::opencode::v0_0_0::session_upload_plugin("opencode-v1").unwrap()),
        )
        .unwrap();
        let native: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let managed: Value = serde_json::from_slice(
            &std::fs::read(home.join(".config/blue/runtime/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(native["custom"], true);
        assert_eq!(
            native["provider"]["personal"]["options"]["baseURL"],
            "http://localhost"
        );
        assert_eq!(native["permission"]["custom_tool"], "ask");
        assert!(native.get("model").is_none());
        assert!(native["provider"].get("governed").is_none());
        assert_eq!(native["enabled_providers"], json!(["personal"]));
        assert_eq!(native["mcp"]["personal"]["command"][0], "mine");
        assert!(managed["mcp"].get("personal").is_none());
        assert_eq!(managed["mcp"]["blocks"]["command"][0], "npx");
        assert!(managed["mcp"].get("disabled-remote").is_none());
        assert_eq!(managed["model"], "governed/model");
        assert!(managed.get("plugin").is_none());
        assert!(home
            .join(".config/blue/runtime/opencode/plugins/blue-session-upload.js")
            .is_file());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn gateway_provider_has_runtime_models_and_native_auth_shape() {
        let home = std::env::temp_dir().join(format!("gh-opencode-gateway-{}", std::process::id()));
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "model": "e2e/model" }
        }))
        .unwrap();
        let wiring = gh_gateway::GatewayWiring {
            base_url: "https://gateway.example/v1".into(),
            token: "test-inference-jwt".into(),
            wire_api: None,
            auth: gh_gateway::AuthPlacement::InFile,
        };

        let report = test_write(&home, &policy, Some(&wiring), None).unwrap();
        let managed: Value = serde_json::from_slice(
            &std::fs::read(home.join(".config/blue/runtime/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            managed["provider"]["governed"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(
            managed["provider"]["governed"]["models"]["e2e/model"]["name"],
            "e2e/model"
        );
        assert_eq!(
            managed["provider"]["governed"]["models"]["openai/gpt-5.6-sol"]["name"],
            "openai/gpt-5.6-sol"
        );
        assert_eq!(
            managed["provider"]["governed"]["options"]["apiKey"],
            "{env:BLUE_OPENCODE_GATEWAY_TOKEN}"
        );
        assert_eq!(report.env[GATEWAY_TOKEN_ENV], "test-inference-jwt");
        let launch_config =
            disable_autoupdate(&report.env["OPENCODE_CONFIG_CONTENT"]).unwrap();
        let launch_config = serde_json::from_str::<Value>(&launch_config).unwrap();
        assert_eq!(launch_config["autoupdate"], false);
        let mut without_launch_control = launch_config;
        without_launch_control
            .as_object_mut()
            .unwrap()
            .remove("autoupdate");
        assert_eq!(without_launch_control, managed);
        assert!(!report.env.contains_key("OPENCODE_CONFIG"));
        crate::assert_same_path(
            report.env.get("OPENCODE_CONFIG_DIR").map(String::as_str),
            &home.join(".config/blue/runtime/opencode"),
        );
        assert!(report.launch_args.is_empty());
        let _ = std::fs::remove_dir_all(home);
    }

    fn gateway_wiring() -> GatewayWiring {
        gh_gateway::GatewayWiring {
            base_url: "https://gateway.example/v1".into(),
            token: "test-inference-jwt".into(),
            wire_api: None,
            auth: gh_gateway::AuthPlacement::InFile,
        }
    }

    /// `home` seeded with an optional native `model`, and the managed config it
    /// renders under gateway wiring.
    fn gateway_write(
        label: &str,
        native_model: Option<&str>,
        managed_config: Value,
    ) -> (HarnessWrite, Value) {
        let home = std::env::temp_dir().join(format!(
            "gh-opencode-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        if let Some(model) = native_model {
            let path = home.join(".config/opencode/opencode.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_vec(&json!({ "model": model })).unwrap()).unwrap();
        }
        let policy: HarnessPolicy =
            serde_json::from_value(json!({ "managed_config": managed_config })).unwrap();
        let report = test_write(&home, &policy, Some(&gateway_wiring()), None).unwrap();
        let managed: Value = serde_json::from_slice(
            &std::fs::read(home.join(".config/blue/runtime/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(home);
        (report, managed)
    }

    #[test]
    fn gateway_without_a_managed_model_defaults_and_warns() {
        let (report, managed) = gateway_write("gateway-nomodel", None, json!({}));

        assert_eq!(managed["model"], "governed/openai/gpt-5.6-sol");
        assert_eq!(
            managed["provider"]["governed"]["models"]["openai/gpt-5.6-sol"]["name"],
            "openai/gpt-5.6-sol"
        );
        assert_eq!(managed["enabled_providers"], json!(["governed"]));
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0].contains("managed_config.model")
                && report.warnings[0].contains("openai/gpt-5.6-sol"),
            "{}",
            report.warnings[0]
        );
    }

    #[test]
    fn gateway_falls_back_to_a_native_model_only_when_it_is_governed() {
        let (_, governed) = gateway_write(
            "gateway-native-known",
            Some("openai/gpt-5.6-sol"),
            json!({ "available_models": ["openai/gpt-5.6-sol", "openai/other"] }),
        );
        assert_eq!(governed["model"], "governed/openai/gpt-5.6-sol");

        // Prefixed with the governed provider by a previous managed run.
        let (_, prefixed) = gateway_write(
            "gateway-native-prefixed",
            Some("governed/openai/other"),
            json!({ "available_models": ["openai/gpt-5.6-sol", "openai/other"] }),
        );
        assert_eq!(prefixed["model"], "governed/openai/other");

        // A provider `enabled_providers` disables must not be pinned.
        let (_, ungoverned) = gateway_write(
            "gateway-native-unknown",
            Some("anthropic/claude-x"),
            json!({ "available_models": ["openai/gpt-5.6-sol", "openai/other"] }),
        );
        assert_eq!(ungoverned["model"], "governed/openai/gpt-5.6-sol");
    }

    #[test]
    fn policy_available_models_replace_the_built_in_catalog() {
        let (_, managed) = gateway_write(
            "gateway-catalog",
            None,
            json!({ "available_models": ["org/fast", "org/slow"] }),
        );

        let models = managed["provider"]["governed"]["models"].as_object().unwrap();
        assert_eq!(managed["model"], "governed/org/fast");
        assert_eq!(models["org/fast"]["name"], "org/fast");
        assert_eq!(models["org/slow"]["name"], "org/slow");
        assert!(!models.contains_key("openai/gpt-5.6-sol"));
    }

    #[test]
    fn a_managed_model_outranks_the_catalog_and_warns_about_nothing() {
        let (report, managed) = gateway_write(
            "gateway-managed",
            Some("openai/gpt-5.6-sol"),
            json!({ "model": "org/pinned", "available_models": ["org/fast"] }),
        );

        assert_eq!(managed["model"], "governed/org/pinned");
        assert_eq!(
            managed["provider"]["governed"]["models"]["org/pinned"]["name"],
            "org/pinned"
        );
        assert_eq!(
            managed["provider"]["governed"]["models"]["org/fast"]["name"],
            "org/fast"
        );
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn governance_only_leaves_model_selection_to_the_agent() {
        let home = std::env::temp_dir().join(format!(
            "gh-opencode-governance-nomodel-{}",
            std::process::id()
        ));
        let policy: HarnessPolicy = serde_json::from_value(json!({ "managed_config": {} })).unwrap();

        let report = test_write(&home, &policy, None, None).unwrap();
        let managed: Value = serde_json::from_slice(
            &std::fs::read(home.join(".config/blue/runtime/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert!(managed.get("model").is_none());
        assert!(managed.get("provider").is_none());
        assert!(report.warnings.is_empty());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn disables_autoupdate_in_legacy_managed_content_without_writing_it() {
        let legacy = r#"{"model":"personal"}"#;
        let updated: Value = serde_json::from_str(&disable_autoupdate(legacy).unwrap()).unwrap();
        assert_eq!(updated["model"], "personal");
        assert_eq!(updated["autoupdate"], false);
        assert_eq!(legacy, r#"{"model":"personal"}"#);
    }
}
