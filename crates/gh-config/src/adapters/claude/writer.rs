//! Shared Claude-family launch overlay: harness-owned settings and MCP files.
//! Native `~/.claude*` files remain direct-launch configuration.

use std::path::Path;

use gh_common::GhError;
use gh_gateway::GatewayWiring;
use gh_service::HarnessPolicy;
use serde_json::{json, Map, Value};

use crate::util::json_pretty;
use crate::HarnessWrite;

pub fn write(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    policy: &HarnessPolicy,
    wiring: Option<&GatewayWiring>,
    session_upload_hooks: Option<Value>,
    session_start_hooks: Option<Value>,
    _enforced: bool,
) -> Result<HarnessWrite, GhError> {
    migrate_legacy_global_config(plan, home, policy)?;
    let runtime = home.join(".config/blue/runtime/claude");
    let settings_path = runtime.join("settings.json");
    let settings = build_settings(policy, wiring, session_upload_hooks, session_start_hooks)?;
    plan.write(&settings_path, json_pretty(&settings)?)?;

    let local = plan.read_json_object(&home.join(".claude.json"))?;
    let local_names = local
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut mcp_root = Map::new();
    let mut servers = Map::new();
    for server in &policy.mcp {
        if server.disabled || local_names.contains_key(&server.name) {
            continue;
        }
        let mut entry = Map::new();
        if let Some(command) = &server.command {
            entry.insert("command".into(), json!(command));
            entry.insert("args".into(), json!(server.args));
            if !server.env.is_empty() {
                entry.insert("env".into(), json!(server.env));
            }
        } else if let Some(url) = &server.url {
            entry.insert("url".into(), json!(url));
            if let Some(transport) = &server.transport {
                entry.insert("transport".into(), json!(transport));
            }
        }
        servers.insert(server.name.clone(), Value::Object(entry));
    }
    mcp_root.insert("mcpServers".into(), Value::Object(servers));
    let mcp_path = runtime.join("mcp.json");
    plan.write(&mcp_path, json_pretty(&Value::Object(mcp_root))?)?;

    Ok(HarnessWrite {
        files: vec![settings_path.clone(), mcp_path.clone()],
        env: Default::default(), // token is in-file, no env publish needed
        launch_args: vec![
            "--settings".into(),
            settings_path.display().to_string(),
            "--mcp-config".into(),
            mcp_path.display().to_string(),
        ],
        package_errors: Vec::new(),
        warnings: Vec::new(),
    })
}

pub fn apply_packages(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    report: &mut HarnessWrite,
    skills_dirs: &[std::path::PathBuf],
    helpers: &std::collections::BTreeMap<String, std::path::PathBuf>,
) -> Result<(), GhError> {
    let runtime = home.join(".config/blue/runtime/claude");
    let standalone_plugin = runtime.join("standalone-skills-plugin");
    if standalone_plugin.exists() {
        plan.remove(&standalone_plugin)?;
    }
    if !skills_dirs.is_empty() {
        plan_skills(
            plan,
            skills_dirs,
            &standalone_plugin.join("skills"),
            &mut report.files,
        )?;
        let manifest_path = standalone_plugin.join(".claude-plugin/plugin.json");
        plan.write(
            &manifest_path,
            json_pretty(&json!({
                "name": "blue-managed-standalone-skills",
                "version": "1.0.0"
            }))?,
        )?;
        report.files.push(manifest_path);
        report.launch_args.push("--plugin-dir".into());
        report
            .launch_args
            .push(standalone_plugin.display().to_string());
    }
    crate::util::prepend_helper_paths(&mut report.env, helpers);
    Ok(())
}

fn migrate_legacy_global_config(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    policy: &HarnessPolicy,
) -> Result<(), GhError> {
    let path = home.join(".claude/settings.json");
    let mut settings = plan.read_json_object(&path)?;
    let mut changed = false;
    if policy
        .managed_config
        .model
        .as_ref()
        .is_some_and(|model| settings.get("model").and_then(Value::as_str) == Some(model))
    {
        settings.remove("model");
        changed = true;
    }
    if let Some(Value::Object(env)) = settings.get_mut("env") {
        changed |= env.remove("ANTHROPIC_BASE_URL").is_some();
        changed |= env.remove("ANTHROPIC_AUTH_TOKEN").is_some();
        if env.is_empty() {
            settings.remove("env");
        }
    }
    if policy.managed_config.auto_approve == Some(true) {
        if let Some(Value::Object(permissions)) = settings.get_mut("permissions") {
            if permissions.get("defaultMode").and_then(Value::as_str) == Some("bypassPermissions") {
                permissions.remove("defaultMode");
                changed = true;
            }
            if permissions.is_empty() {
                settings.remove("permissions");
            }
        }
    }
    if let Some(Value::Object(hooks)) = settings.get_mut("hooks") {
        if let Some(Value::Array(entries)) = hooks.get_mut("SessionEnd") {
            let before = entries.len();
            entries.retain(|entry| !contains_session_upload(entry, "claude"));
            changed |= entries.len() != before;
            if entries.is_empty() {
                hooks.remove("SessionEnd");
            }
        }
        if let Some(Value::Array(entries)) = hooks.get_mut("SessionStart") {
            let before = entries.len();
            entries.retain(|entry| !contains_session_start(entry, "claude"));
            changed |= entries.len() != before;
            if entries.is_empty() {
                hooks.remove("SessionStart");
            }
        }
        if hooks.is_empty() {
            settings.remove("hooks");
        }
    }
    if changed {
        plan.write(&path, json_pretty(&Value::Object(settings))?)?;
    }
    Ok(())
}

fn contains_session_upload(value: &Value, harness: &str) -> bool {
    match value {
        Value::String(value) => value.contains(&format!("session-upload {harness}")),
        Value::Array(values) => values
            .iter()
            .any(|value| contains_session_upload(value, harness)),
        Value::Object(values) => values
            .values()
            .any(|value| contains_session_upload(value, harness)),
        _ => false,
    }
}

fn contains_session_start(value: &Value, harness: &str) -> bool {
    match value {
        Value::String(value) => value.contains(&format!("session-start {harness}")),
        Value::Array(values) => values
            .iter()
            .any(|value| contains_session_start(value, harness)),
        Value::Object(values) => values
            .values()
            .any(|value| contains_session_start(value, harness)),
        _ => false,
    }
}

fn build_settings(
    policy: &HarnessPolicy,
    wiring: Option<&GatewayWiring>,
    session_upload_hooks: Option<Value>,
    session_start_hooks: Option<Value>,
) -> Result<Value, GhError> {
    let mut settings = Map::new();
    if let Some(model) = &policy.managed_config.model {
        settings.insert("model".into(), json!(model));
    }
    // auto_approve ⇒ bypass permissions default mode.
    if policy.managed_config.auto_approve == Some(true) {
        settings.insert(
            "permissions".into(),
            json!({ "defaultMode": "bypassPermissions" }),
        );
    }
    if let Some(w) = wiring {
        settings.insert(
            "env".into(),
            json!({
                "ANTHROPIC_BASE_URL": w.base_url,
                "ANTHROPIC_AUTH_TOKEN": w.token,
            }),
        );
    }
    // Managed SessionEnd (upload) and SessionStart (identity) hooks share the
    // single `hooks` object Claude reads from the managed settings.json.
    let mut hooks = Map::new();
    for source in [session_upload_hooks, session_start_hooks].into_iter().flatten() {
        let Value::Object(map) = source else { continue };
        for (event, entries) in map {
            let Value::Array(entries) = entries else { continue };
            hooks
                .entry(event)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .expect("managed hook event is always an array")
                .extend(entries);
        }
    }
    if !hooks.is_empty() {
        settings.insert("hooks".into(), Value::Object(hooks));
    }
    Ok(Value::Object(settings))
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
        session_upload_hooks: Option<Value>,
        _enforced: bool,
    ) -> Result<HarnessWrite, GhError> {
        let mut plan = crate::adapters::ReconcilePlan::default();
        let result = write(
            &mut plan,
            home,
            policy,
            wiring,
            session_upload_hooks,
            None,
            _enforced,
        )?;
        let mut transaction = crate::FileTransaction::begin(home, &plan)?;
        transaction.apply(&plan)?;
        transaction.commit();
        Ok(result)
    }

    #[test]
    fn preserves_unmanaged_settings_and_existing_mcp_servers() {
        let home = std::env::temp_dir().join(format!("gh-claude-merge-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude.json"),
            br#"{"custom":true,"mcpServers":{"personal":{"command":"mine"}}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".claude/settings.json"),
            br#"{"custom":"keep","model":"claude-governed","permissions":{"deny":["danger"],"defaultMode":"bypassPermissions"},"env":{"PERSONAL":"yes","ANTHROPIC_BASE_URL":"http://proxy","ANTHROPIC_AUTH_TOKEN":"old-inference-token"},"hooks":{"SessionEnd":[{"hooks":[{"type":"command","command":"my-session-hook"}]},{"hooks":[{"type":"command","command":"harness session-upload claude"}]}]}}"#,
        )
        .unwrap();
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "model": "claude-governed", "auto_approve": true },
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
            Some(crate::adapters::claude::v2_0_12::session_upload_hooks("claude-v1").unwrap()),
            false,
        )
        .unwrap();
        let native_root: Value =
            serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
        let native_settings: Value =
            serde_json::from_slice(&std::fs::read(home.join(".claude/settings.json")).unwrap())
                .unwrap();
        let settings: Value = serde_json::from_slice(
            &std::fs::read(home.join(".config/blue/runtime/claude/settings.json")).unwrap(),
        )
        .unwrap();
        let mcp: Value = serde_json::from_slice(
            &std::fs::read(home.join(".config/blue/runtime/claude/mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(native_root["mcpServers"]["personal"]["command"], "mine");
        assert_eq!(native_settings["custom"], "keep");
        assert_eq!(native_settings["permissions"]["deny"][0], "danger");
        assert!(native_settings.get("model").is_none());
        assert!(native_settings["permissions"].get("defaultMode").is_none());
        assert_eq!(native_settings["env"]["PERSONAL"], "yes");
        assert!(native_settings["env"].get("ANTHROPIC_BASE_URL").is_none());
        assert_eq!(
            native_settings["hooks"]["SessionEnd"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(settings["model"], "claude-governed");
        assert_eq!(settings["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
        assert!(mcp["mcpServers"].get("personal").is_none());
        assert_eq!(mcp["mcpServers"]["blocks"]["command"], "npx");
        let _ = std::fs::remove_dir_all(home);
    }
}
