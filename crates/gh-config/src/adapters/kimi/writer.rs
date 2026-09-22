//! Shared Kimi-family launch overlay: a merged, isolated `KIMI_CODE_HOME` under the
//! metaharness runtime directory. Native `~/.kimi-code` remains unchanged.

use std::path::Path;

use gh_common::GhError;
use gh_gateway::GatewayWiring;
use gh_service::HarnessPolicy;
use serde_json::{json, Map, Value};
use toml::Value as Toml;

use crate::util::json_pretty;
use crate::HarnessWrite;

const GOVERNED_PROVIDER: &str = "governed";

pub fn write(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    policy: &HarnessPolicy,
    wiring: Option<&GatewayWiring>,
    session_upload_hooks: Option<Vec<Toml>>,
    session_start_hooks: Option<Vec<Toml>>,
) -> Result<HarnessWrite, GhError> {
    let source_path = home.join(".kimi-code").join("config.toml");
    migrate_legacy_hook(plan, &source_path)?;
    let runtime = crate::managed_runtime_dir(home).join("kimi");
    let path = runtime.join("config.toml");
    let mut table = plan.read_toml_table(&source_path)?;
    let mut gateway_catalog = None;

    if let Some(model) = &policy.managed_config.model {
        table.insert("default_model".into(), Toml::String(model.clone()));
    }
    if let Some(auto_approve) = policy.managed_config.auto_approve {
        table.insert(
            "default_permission_mode".into(),
            Toml::String(if auto_approve { "yolo" } else { "manual" }.into()),
        );
        // Remove the pre-0.20 spelling when rebuilding the managed overlay.
        table.remove("default_yolo");
    }
    if let Some(w) = wiring {
        let mut catalog = Vec::new();
        for model in policy.gateway_models.iter().map(|model| model.trim()) {
            if !model.is_empty() && !catalog.iter().any(|existing| existing == model) {
                catalog.push(model.to_owned());
            }
        }
        if catalog.is_empty() {
            return Err(GhError::config(
                "gateway-mode Kimi policy requires at least one gateway_models entry",
            ));
        }
        let effective_model = policy
            .managed_config
            .model
            .clone()
            .or_else(|| catalog.first().cloned());
        if effective_model
            .as_ref()
            .is_some_and(|selected| !catalog.iter().any(|model| model == selected))
        {
            return Err(GhError::config(
                "Kimi selected model must be present in gateway_models",
            ));
        }
        // kimi-code resolves the wire transport from the provider `type`
        // (`openai` vs `openai_responses`). The model-alias `protocol` field
        // only accepts the literal "anthropic": kimi-code 0.20.x–0.31.x hard-
        // reject any other value and salvage-drop the whole `[models.*]` entry,
        // which then surfaces at runtime as the misleading
        // `Model "<m>" is not configured in config.toml. Add a [models."<m>"]
        // entry with max_context_size.` (0.39.x later widened the literal, so
        // the pin masked the gap). Encode the wire API on the provider type and
        // leave the model entry protocol-free so every supported CLI accepts it.
        let provider_type = match w.wire_api.as_deref() {
            Some("responses" | "openai_responses") => "openai_responses",
            _ => "openai",
        };
        // Replace the native catalogs so the gateway picker exposes only the
        // ordered policy assignment, all routed through the governed provider.
        let mut models = toml::map::Map::new();
        for model in &catalog {
            let mut model_entry = toml::map::Map::new();
            model_entry.insert("provider".into(), Toml::String(GOVERNED_PROVIDER.into()));
            model_entry.insert("model".into(), Toml::String(model.clone()));
            let max_context_size = policy
                .managed_config
                .extra
                .get("max_context_size")
                .and_then(Value::as_i64)
                .filter(|value| *value > 0)
                .unwrap_or(262_144);
            model_entry.insert("max_context_size".into(), Toml::Integer(max_context_size));
            models.insert(model.clone(), Toml::Table(model_entry));
        }
        table.insert("models".into(), Toml::Table(models));
        table.insert(
            "default_model".into(),
            Toml::String(effective_model.expect("non-empty catalog has a fallback")),
        );

        let mut provider = toml::map::Map::new();
        provider.insert("type".into(), Toml::String(provider_type.into()));
        provider.insert("base_url".into(), Toml::String(w.base_url.clone()));
        provider.insert("api_key".into(), Toml::String(w.token.clone()));
        table.insert(
            "providers".into(),
            Toml::Table(singleton_table(
                GOVERNED_PROVIDER.into(),
                Toml::Table(provider),
            )),
        );
        gateway_catalog = Some(catalog);
    }

    let mut hooks = table
        .remove("hooks")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    hooks.retain(|entry| {
        entry
            .get("command")
            .and_then(Toml::as_str)
            .map(|command| {
                !command.contains(" session-upload kimi") && !command.contains(" session-start kimi")
            })
            .unwrap_or(true)
    });
    if let Some(managed_hooks) = session_upload_hooks {
        hooks.extend(managed_hooks);
    }
    if let Some(managed_hooks) = session_start_hooks {
        hooks.extend(managed_hooks);
    }
    if !hooks.is_empty() {
        table.insert("hooks".into(), Toml::Array(hooks));
    }
    let body = serialize_config(table, gateway_catalog.as_deref())?;
    plan.write(&path, body)?;

    // MCP → the isolated KIMI_CODE_HOME, seeded from the user's native file.
    let mcp_path = runtime.join("mcp.json");
    let mut mcp_root = plan.read_json_object(&home.join(".kimi-code/mcp.json"))?;
    let mut servers = mcp_root
        .remove("mcpServers")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    for s in &policy.mcp {
        if s.disabled || servers.contains_key(&s.name) {
            continue;
        }
        let mut entry = Map::new();
        if let Some(cmd) = &s.command {
            entry.insert("command".into(), json!(cmd));
            entry.insert("args".into(), json!(s.args));
            if !s.env.is_empty() {
                entry.insert("env".into(), json!(s.env));
            }
        } else if let Some(url) = &s.url {
            entry.insert("url".into(), json!(url));
        }
        servers.insert(s.name.clone(), Value::Object(entry));
    }
    mcp_root.insert("mcpServers".into(), Value::Object(servers));
    plan.write(&mcp_path, json_pretty(&Value::Object(mcp_root))?)?;

    Ok(HarnessWrite {
        files: vec![path, mcp_path],
        env: [("KIMI_CODE_HOME".into(), runtime.display().to_string())]
            .into_iter()
            .collect(),
        launch_args: Vec::new(),
        package_errors: Vec::new(),
        warnings: Vec::new(),
    })
}

pub fn apply_packages(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    report: &mut HarnessWrite,
    skills_dirs: &[std::path::PathBuf],
    agents_dirs: &[std::path::PathBuf],
    hooks_files: &[std::path::PathBuf],
    helpers: &std::collections::BTreeMap<String, std::path::PathBuf>,
) -> Result<(), GhError> {
    let runtime = crate::managed_runtime_dir(home).join("kimi");
    if !skills_dirs.is_empty() {
        let target = runtime.join("skills");
        plan_skills(plan, skills_dirs, &target, &mut report.files)?;
        let config_path = runtime.join("config.toml");
        let mut config = plan.read_toml_table(&config_path)?;
        config.insert(
            "extra_skill_dirs".into(),
            Toml::Array(vec![Toml::String(target.display().to_string())]),
        );
        plan.write(
            &config_path,
            toml::to_string_pretty(&Toml::Table(config))
                .map_err(|error| GhError::Serde(error.to_string()))?,
        )?;
    }
    if !hooks_files.is_empty() {
        let config_path = runtime.join("config.toml");
        let mut config = plan.read_toml_table(&config_path)?;
        let mut hooks = config
            .remove("hooks")
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        for hooks_file in hooks_files {
            let mut fragment = plan.read_toml_table(hooks_file)?;
            let entries = fragment
                .remove("hooks")
                .and_then(|value| value.as_array().cloned())
                .ok_or_else(|| {
                    GhError::config(format!(
                        "{} must contain a hooks array",
                        hooks_file.display()
                    ))
                })?;
            for entry in entries {
                if !hooks.contains(&entry) {
                    hooks.push(entry);
                }
            }
        }
        if !hooks.is_empty() {
            config.insert("hooks".into(), Toml::Array(hooks));
        }
        let body = toml::to_string_pretty(&Toml::Table(config))
            .map_err(|error| GhError::Serde(error.to_string()))?;
        plan.write(&config_path, body)?;
    }
    plan.component_dirs(agents_dirs, &runtime.join("agents"), &mut report.files)?;
    if !agents_dirs.is_empty() {
        let config_path = runtime.join("config.toml");
        let mut config = plan.read_toml_table(&config_path)?;
        config.insert(
            "extra_agent_dirs".into(),
            Toml::Array(vec![Toml::String(
                runtime.join("agents").display().to_string(),
            )]),
        );
        plan.write(
            &config_path,
            toml::to_string_pretty(&Toml::Table(config))
                .map_err(|error| GhError::Serde(error.to_string()))?,
        )?;
    }
    plan.component_files(hooks_files, &runtime.join("hooks"), &mut report.files)?;
    crate::util::prepend_helper_paths(&mut report.env, helpers);
    Ok(())
}

fn migrate_legacy_hook(
    plan: &mut crate::adapters::ReconcilePlan,
    path: &Path,
) -> Result<(), GhError> {
    let mut table = plan.read_toml_table(path)?;
    let Some(Toml::Array(hooks)) = table.get_mut("hooks") else {
        return Ok(());
    };
    let before = hooks.len();
    hooks.retain(|entry| {
        entry
            .get("command")
            .and_then(Toml::as_str)
            .map(|command| {
                !command.contains(" session-upload kimi") && !command.contains(" session-start kimi")
            })
            .unwrap_or(true)
    });
    if hooks.len() == before {
        return Ok(());
    }
    if hooks.is_empty() {
        table.remove("hooks");
    }
    let body = toml::to_string_pretty(&Toml::Table(table))
        .map_err(|error| GhError::Serde(error.to_string()))?;
    plan.write(path, body)
}

fn serialize_config(
    mut table: toml::map::Map<String, Toml>,
    gateway_catalog: Option<&[String]>,
) -> Result<String, GhError> {
    let Some(catalog) = gateway_catalog else {
        return toml::to_string_pretty(&Toml::Table(table))
            .map_err(|error| GhError::Serde(error.to_string()));
    };

    let mut models = table
        .remove("models")
        .and_then(|value| value.as_table().cloned())
        .unwrap_or_default();
    let providers = table.remove("providers");
    let mut body = toml::to_string_pretty(&Toml::Table(table))
        .map_err(|error| GhError::Serde(error.to_string()))?;
    for model in catalog {
        let entry = models
            .remove(model)
            .expect("every gateway catalog model has a generated entry");
        let fragment = Toml::Table(singleton_table(
            "models".into(),
            Toml::Table(singleton_table(model.clone(), entry)),
        ));
        body.push('\n');
        body.push_str(
            &toml::to_string_pretty(&fragment)
                .map_err(|error| GhError::Serde(error.to_string()))?,
        );
    }
    if let Some(providers) = providers {
        body.push('\n');
        let providers = Toml::Table(singleton_table("providers".into(), providers));
        body.push_str(
            &toml::to_string_pretty(&providers)
                .map_err(|error| GhError::Serde(error.to_string()))?,
        );
    }
    Ok(body)
}

fn singleton_table(key: String, value: Toml) -> toml::map::Map<String, Toml> {
    let mut table = toml::map::Map::new();
    table.insert(key, value);
    table
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
        session_upload_hooks: Option<Vec<Toml>>,
    ) -> Result<HarnessWrite, GhError> {
        let mut plan = crate::adapters::ReconcilePlan::default();
        let result = write(&mut plan, home, policy, wiring, session_upload_hooks, None)?;
        let mut transaction = crate::FileTransaction::begin(home, &plan)?;
        transaction.apply(&plan)?;
        transaction.commit()?;
        Ok(result)
    }

    use gh_gateway::{AuthPlacement, GatewayWiring};

    #[test]
    fn preserves_existing_toml_and_mcp_servers() {
        let home = std::env::temp_dir().join(format!("gh-kimi-merge-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".kimi-code")).unwrap();
        std::fs::write(
            home.join(".kimi-code/config.toml"),
            "personal = true\nhooks = [{ event = \"SessionEnd\", command = \"my-hook\" }, { event = \"SessionEnd\", command = \"harness session-upload kimi\" }]\n[models.personal]\nprovider = \"personal\"\nmodel = \"personal\"\nmax_context_size = 4096\n[providers.personal]\ntype = \"openai\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".kimi-code/mcp.json"),
            br#"{"custom":true,"mcpServers":{"personal":{"command":"mine"}}}"#,
        )
        .unwrap();
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "model": "kimi-governed", "auto_approve": true },
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
            Some(crate::adapters::kimi::v0_0_0::session_upload_hooks("kimi-v1").unwrap()),
        )
        .unwrap();
        let native = std::fs::read_to_string(home.join(".kimi-code/config.toml"))
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        let config = std::fs::read_to_string(home.join(".config/blue/runtime/kimi/config.toml"))
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        let native_mcp: Value =
            serde_json::from_slice(&std::fs::read(home.join(".kimi-code/mcp.json")).unwrap())
                .unwrap();
        let mcp: Value = serde_json::from_slice(
            &std::fs::read(home.join(".config/blue/runtime/kimi/mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(native["personal"].as_bool(), Some(true));
        assert_eq!(native["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(
            config["providers"]["personal"]["type"].as_str(),
            Some("openai")
        );
        assert_eq!(
            config["models"]["personal"]["provider"].as_str(),
            Some("personal")
        );
        assert_eq!(config["default_model"].as_str(), Some("kimi-governed"));
        assert_eq!(config["default_permission_mode"].as_str(), Some("yolo"));
        assert!(config.get("default_yolo").is_none());
        assert!(format!("{:?}", config["hooks"]).contains("session-upload kimi"));
        assert_eq!(native_mcp["mcpServers"]["personal"]["command"], "mine");
        assert_eq!(mcp["custom"], true);
        assert_eq!(mcp["mcpServers"]["personal"]["command"], "mine");
        assert_eq!(mcp["mcpServers"]["blocks"]["command"], "npx");
        assert!(mcp["mcpServers"].get("disabled-remote").is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn gateway_replaces_native_catalog_preserves_assignment_order_and_uses_first_fallback() {
        let home =
            std::env::temp_dir().join(format!("gh-kimi-global-gateway-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".kimi-code")).unwrap();
        std::fs::write(
            home.join(".kimi-code/config.toml"),
            "default_model = \"personal-default\"\npersonal = true\n[models.native]\nprovider = \"personal\"\nmodel = \"native\"\nmax_context_size = 1000\n[providers.personal]\ntype = \"openai\"\nbase_url = \"https://native.example\"\napi_key = \"native-secret\"\n",
        )
        .unwrap();
        let wiring = GatewayWiring {
            base_url: "https://inference.example".into(),
            token: "test-inference-jwt".into(),
            wire_api: None,
            auth: AuthPlacement::InFile,
        };

        let policy = HarnessPolicy {
            gateway_models: vec![
                "z-policy-first".into(),
                "a-policy-second".into(),
                "z-policy-first".into(),
            ],
            ..HarnessPolicy::default()
        };
        test_write(&home, &policy, Some(&wiring), None).unwrap();
        let body =
            std::fs::read_to_string(home.join(".config/blue/runtime/kimi/config.toml")).unwrap();
        let config = body.parse::<Toml>().unwrap();
        assert_eq!(config["default_model"].as_str(), Some("z-policy-first"));
        assert_eq!(config["personal"].as_bool(), Some(true));
        assert_eq!(config["models"].as_table().unwrap().len(), 2);
        assert!(config["models"].get("native").is_none());
        assert_eq!(config["providers"].as_table().unwrap().len(), 1);
        assert!(config["providers"].get("personal").is_none());
        assert_eq!(
            config["models"]["z-policy-first"]["provider"].as_str(),
            Some("governed")
        );
        assert_eq!(
            config["models"]["a-policy-second"]["provider"].as_str(),
            Some("governed")
        );
        assert!(
            body.find("[models.z-policy-first]").unwrap()
                < body.find("[models.a-policy-second]").unwrap(),
            "generated model tables must retain policy order"
        );
        // The model alias must NOT carry a `protocol` field: kimi-code's schema
        // only accepts `protocol = "anthropic"`, and 0.20.x–0.31.x salvage-drop
        // an alias that declares any other value. The wire transport is carried
        // by the provider `type` instead (asserted below).
        assert!(
            config["models"]["z-policy-first"]
                .as_table()
                .is_some_and(|entry| !entry.contains_key("protocol")),
            "model alias must not declare `protocol` (kimi-code only allows \"anthropic\")"
        );
        assert_eq!(
            config["models"]["z-policy-first"]["max_context_size"].as_integer(),
            Some(262_144)
        );
        assert_eq!(
            config["providers"]["governed"]["type"].as_str(),
            Some("openai")
        );
        assert_eq!(
            config["providers"]["governed"]["base_url"].as_str(),
            Some("https://inference.example")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn gateway_rejects_empty_catalog_and_unassigned_explicit_default() {
        let home = std::env::temp_dir().join(format!("gh-kimi-invalid-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".kimi-code")).unwrap();
        let wiring = GatewayWiring {
            base_url: "https://inference.example".into(),
            token: "test-inference-jwt".into(),
            wire_api: None,
            auth: AuthPlacement::InFile,
        };

        let empty = test_write(&home, &HarnessPolicy::default(), Some(&wiring), None).unwrap_err();
        assert!(empty.to_string().contains("requires at least one gateway_models"));

        let policy = HarnessPolicy {
            gateway_models: vec!["assigned".into()],
            managed_config: serde_json::from_value(json!({ "model": "unassigned" })).unwrap(),
            ..HarnessPolicy::default()
        };
        let unassigned = test_write(&home, &policy, Some(&wiring), None).unwrap_err();
        assert!(unassigned.to_string().contains("must be present in gateway_models"));
        let _ = std::fs::remove_dir_all(home);
    }
}
