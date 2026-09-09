//! Shared Codex-family writer → `~/.codex/config.toml`.
//!
//! Managed model/approval/sandbox flags + `[mcp_servers.*]`, and (gateway mode)
//! a `[model_providers.governed]` block whose `env_key` names the env var the
//! pseudotoken is exported into. Mirrors control-sdk `agent_codex.py`.

use std::collections::BTreeMap;
use std::path::Path;

use gh_common::GhError;
use gh_gateway::GatewayWiring;
use gh_service::{HarnessPolicy, McpServer};
use toml::Value as Toml;

use crate::util::json_to_toml;
use crate::HarnessWrite;

const GOVERNED_PROVIDER: &str = "governed";
const STANDALONE_SKILLS_PLUGIN: &str = "blue-managed-standalone-skills";
const STANDALONE_SKILLS_MARKETPLACE: &str = "governance-blue-managed-standalone-skills";
const STANDALONE_SKILLS_VERSION: &str = "1.0.0";

/// Resolve the governed Codex approval mode. Managed Codex launches preserve
/// native approval prompts unless policy explicitly opts into auto-approval.
/// An explicit Codex approval policy always wins.
pub(super) fn approval_policy(policy: &HarnessPolicy) -> &str {
    policy.managed_config.approval_policy.as_deref().unwrap_or(
        if policy.managed_config.auto_approve == Some(true) {
            "never"
        } else {
            "on-request"
        },
    )
}

pub fn write(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    policy: &HarnessPolicy,
    wiring: Option<&GatewayWiring>,
    session_upload_hook: Option<Toml>,
    session_start_hook: Option<Toml>,
) -> Result<HarnessWrite, GhError> {
    let base_path = home.join(".codex").join("config.toml");
    migrate_legacy_global_config(plan, &base_path, policy)?;
    let base = plan.read_toml_table(&base_path)?;
    let path = home.join(".codex").join("blue.config.toml");
    let existing = plan.read_toml_table(&path)?;
    let mut table = toml::map::Map::new();

    // Base: preserve forward-compatible managed extras first, typed keys win.
    for (k, v) in &policy.managed_config.extra {
        if let Some(tv) = json_to_toml(v) {
            table.insert(k.clone(), tv);
        }
    }
    if let Some(model) = &policy.managed_config.model {
        table.insert("model".into(), Toml::String(model.clone()));
    }
    table.insert(
        "approval_policy".into(),
        Toml::String(approval_policy(policy).into()),
    );
    if let Some(sandbox) = &policy.managed_config.sandbox_mode {
        table.insert("sandbox_mode".into(), Toml::String(sandbox.clone()));
    }
    if let Some(reasoning_effort) = &policy.managed_config.reasoning_effort {
        table.insert(
            "model_reasoning_effort".into(),
            Toml::String(reasoning_effort.clone()),
        );
    }
    if let Some(fast_mode) = policy.managed_config.fast_mode {
        let mut features = table
            .remove("features")
            .and_then(|value| value.as_table().cloned())
            .unwrap_or_default();
        features.insert("fast_mode".into(), Toml::Boolean(fast_mode));
        table.insert("features".into(), Toml::Table(features));
        if !fast_mode {
            table.insert("service_tier".into(), Toml::String("default".into()));
        }
    }

    let mut env = BTreeMap::new();

    // Gateway mode: point the active provider at the upstream proxy.
    if let Some(w) = wiring {
        table.insert(
            "model_provider".into(),
            Toml::String(GOVERNED_PROVIDER.into()),
        );

        let mut provider = toml::map::Map::new();
        provider.insert("base_url".into(), Toml::String(w.base_url.clone()));
        provider.insert(
            "wire_api".into(),
            Toml::String(w.wire_api.clone().unwrap_or_else(|| "responses".into())),
        );
        provider.insert("name".into(), Toml::String("Governed (harness)".into()));
        // Codex reads the key from this env var, not the file.
        let env_key = match &w.auth {
            gh_gateway::AuthPlacement::EnvVar(name) => name.clone(),
            gh_gateway::AuthPlacement::InFile => gh_gateway::CODEX_ENV_KEY.to_string(),
        };
        provider.insert("env_key".into(), Toml::String(env_key.clone()));

        let mut providers = table
            .remove("model_providers")
            .and_then(|value| value.as_table().cloned())
            .unwrap_or_default();
        providers.insert(GOVERNED_PROVIDER.into(), Toml::Table(provider));
        table.insert("model_providers".into(), Toml::Table(providers));

        // The pseudotoken must reach Codex's process env (also republished to
        // GUI environments by the daemon).
        env.insert(env_key, w.token.clone());
    }

    // MCP servers → [mcp_servers.<name>].
    if !policy.mcp.is_empty() {
        let local_servers = base
            .get("mcp_servers")
            .and_then(|value| value.as_table().cloned())
            .unwrap_or_default();
        let mut servers = toml::map::Map::new();
        for s in &policy.mcp {
            if s.disabled || local_servers.contains_key(&s.name) {
                continue;
            }
            servers.insert(
                s.name.clone(),
                codex_mcp_entry(s, approval_policy(policy) == "never"),
            );
        }
        if !servers.is_empty() {
            table.insert("mcp_servers".into(), Toml::Table(servers));
        }
    }

    if session_upload_hook.is_some()
        || session_start_hook.is_some()
        || table
            .get("hooks")
            .is_some_and(|hooks| contains_managed_session_upload(hooks) || contains_managed_session_start(hooks))
    {
        let mut hooks = match base.get("hooks").cloned() {
            Some(Toml::Table(hooks)) => hooks,
            Some(_) => {
                return Err(GhError::config(format!(
                    "{} has a non-table hooks value; refusing to overwrite it",
                    path.display()
                )))
            }
            None => toml::map::Map::new(),
        };
        // Codex records approved hook hashes in the config file that declares
        // the hook. Preserve that Codex-owned state when rebuilding the
        // managed overlay so an unchanged hook remains trusted. A changed
        // hook still has to be approved because Codex validates the hash.
        if let Some(existing_state) = existing
            .get("hooks")
            .and_then(Toml::as_table)
            .and_then(|hooks| hooks.get("state"))
            .and_then(Toml::as_table)
        {
            let state = hooks
                .entry("state")
                .or_insert_with(|| Toml::Table(toml::map::Map::new()));
            if let Toml::Table(state) = state {
                state.extend(existing_state.clone());
            }
        }
        let mut session_end = hooks
            .remove("SessionEnd")
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        session_end.retain(|entry| !contains_managed_session_upload(entry));

        if let Some(hook) = session_upload_hook {
            session_end.push(hook);
        }
        if !session_end.is_empty() {
            hooks.insert("SessionEnd".into(), Toml::Array(session_end));
        }
        let mut session_start = hooks
            .remove("SessionStart")
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        session_start.retain(|entry| !contains_managed_session_start(entry));

        if let Some(hook) = session_start_hook {
            session_start.push(hook);
        }
        if !session_start.is_empty() {
            hooks.insert("SessionStart".into(), Toml::Array(session_start));
        }
        if !hooks.is_empty() {
            table.insert("hooks".into(), Toml::Table(hooks));
        }
    }

    let body =
        toml::to_string_pretty(&Toml::Table(table)).map_err(|e| GhError::Serde(e.to_string()))?;
    plan.write(&path, body)?;

    Ok(HarnessWrite {
        files: vec![path],
        env,
        launch_args: vec!["--profile".into(), "blue".into()],
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
    let runtime = home.join(".config/blue/runtime/codex");
    let legacy_standalone_plugin = runtime.join("standalone-skills-plugin");
    if legacy_standalone_plugin.exists() {
        plan.remove(&legacy_standalone_plugin)?;
    }
    let plugin_dirs = report
        .env
        .iter()
        .filter(|(key, _)| key.starts_with("HARNESS_CODEX_PLUGIN_"))
        .map(|(key, value)| (key.clone(), std::path::PathBuf::from(value)))
        .collect::<Vec<_>>();
    for (key, _) in &plugin_dirs {
        report.env.remove(key);
    }
    let catalog_root = runtime.join("marketplaces");
    if catalog_root.exists() {
        plan.remove(&catalog_root)?;
    }
    if !plugin_dirs.is_empty() {
        let config_path = home.join(".codex/blue.config.toml");
        let mut config = plan.read_toml_table(&config_path)?;
        let mut marketplaces = toml::map::Map::new();
        let mut plugins = toml::map::Map::new();
        for (_, plugin_dir) in plugin_dirs {
            let manifest_path = plugin_dir.join(".codex-plugin/plugin.json");
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&manifest_path).map_err(|source| {
                    GhError::Io {
                        path: manifest_path.clone(),
                        source,
                    }
                })?)
                .map_err(|error| {
                    GhError::config(format!("parsing {}: {error}", manifest_path.display()))
                })?;
            let plugin_name = manifest
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    GhError::config(format!("{} has no plugin name", manifest_path.display()))
                })?;
            let slug = plugin_name
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || character == '-' {
                        character.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            let bundled_marketplace = plugin_dir.join(".agents/plugins/marketplace.json");
            let marketplace_name = if bundled_marketplace.is_file() {
                let document: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(&bundled_marketplace).map_err(|source| GhError::Io {
                        path: bundled_marketplace.clone(),
                        source,
                    })?,
                )
                .map_err(|error| {
                    GhError::config(format!(
                        "parsing {}: {error}",
                        bundled_marketplace.display()
                    ))
                })?;
                document
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        GhError::config(format!(
                            "{} has no marketplace name",
                            bundled_marketplace.display()
                        ))
                    })?
            } else {
                format!("governance-{slug}")
            };
            let marketplace_root = if bundled_marketplace.is_file() {
                plugin_dir.clone()
            } else {
                let root = catalog_root.join(&marketplace_name);
                let metadata = root.join(".agents/plugins");

                let document = serde_json::json!({
                    "name": marketplace_name,
                    "interface": { "displayName": format!("Governance: {plugin_name}") },
                    "plugins": [{
                        "name": plugin_name,
                        "source": { "source": "local", "path": plugin_dir },
                        "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" }
                    }]
                });
                plan.write(
                    &metadata.join("marketplace.json"),
                    crate::util::json_pretty(&document)?,
                )?;
                report.files.push(root.clone());
                root
            };
            let mut marketplace = toml::map::Map::new();
            marketplace.insert(
                "source".into(),
                Toml::String(marketplace_root.display().to_string()),
            );
            marketplace.insert("source_type".into(), Toml::String("local".into()));
            if marketplaces
                .insert(marketplace_name.clone(), Toml::Table(marketplace))
                .is_some()
            {
                return Err(GhError::config(format!(
                    "managed Codex marketplace `{marketplace_name}` is declared by multiple packages"
                )));
            }
            let mut enabled = toml::map::Map::new();
            enabled.insert("enabled".into(), Toml::Boolean(true));
            if plugins
                .insert(
                    format!("{plugin_name}@{marketplace_name}"),
                    Toml::Table(enabled),
                )
                .is_some()
            {
                return Err(GhError::config(format!(
                    "managed Codex plugin `{plugin_name}` is declared more than once"
                )));
            }
        }
        config.insert("marketplaces".into(), Toml::Table(marketplaces));
        config.insert("plugins".into(), Toml::Table(plugins));
        let body = toml::to_string_pretty(&Toml::Table(config))
            .map_err(|error| GhError::Serde(error.to_string()))?;
        plan.write(&config_path, body)?;
    }
    reconcile_standalone_skills(plan, home, &runtime, skills_dirs, report)?;
    plan.component_dirs(agents_dirs, &runtime.join("agents"), &mut report.files)?;
    plan.component_files(hooks_files, &runtime.join("hooks"), &mut report.files)?;
    crate::util::prepend_helper_paths(&mut report.env, helpers);
    Ok(())
}

fn reconcile_standalone_skills(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    runtime: &Path,
    skills_dirs: &[std::path::PathBuf],
    report: &mut HarnessWrite,
) -> Result<(), GhError> {
    let marketplace_root = runtime
        .join("marketplaces")
        .join(STANDALONE_SKILLS_MARKETPLACE);
    let plugin_root = marketplace_root
        .join("plugins")
        .join(STANDALONE_SKILLS_PLUGIN);
    let cache_root = home
        .join(".codex/plugins/cache")
        .join(STANDALONE_SKILLS_MARKETPLACE)
        .join(STANDALONE_SKILLS_PLUGIN);
    if cache_root.exists() {
        plan.remove(&cache_root)?;
    }

    let config_path = home.join(".codex/config.toml");
    let mut config = plan.read_toml_table(&config_path)?;
    let mut marketplaces = config
        .remove("marketplaces")
        .and_then(|value| value.as_table().cloned())
        .unwrap_or_default();
    let mut plugins = config
        .remove("plugins")
        .and_then(|value| value.as_table().cloned())
        .unwrap_or_default();
    marketplaces.remove(STANDALONE_SKILLS_MARKETPLACE);
    plugins.remove(&format!(
        "{STANDALONE_SKILLS_PLUGIN}@{STANDALONE_SKILLS_MARKETPLACE}"
    ));

    if !skills_dirs.is_empty() {
        let skills_target = plugin_root.join("skills");
        plan_skills(plan, skills_dirs, &skills_target, &mut report.files)?;
        let manifest_path = plugin_root.join(".codex-plugin/plugin.json");
        plan.write(
            &manifest_path,
            crate::util::json_pretty(&serde_json::json!({
                "name": STANDALONE_SKILLS_PLUGIN,
                "version": STANDALONE_SKILLS_VERSION,
                "description": "Organization-managed skills distributed by Blue.",
                "author": { "name": "Blue" },
                "skills": "./skills/",
                "interface": {
                    "displayName": "Managed Skills",
                    "shortDescription": "Organization-managed Codex skills.",
                    "longDescription": "Skills selected and distributed through Blue governance.",
                    "developerName": "Blue",
                    "category": "Productivity",
                    "capabilities": [],
                    "defaultPrompt": "Help me use an organization-managed skill."
                }
            }))?,
        )?;
        report.files.push(manifest_path);

        let marketplace_path = marketplace_root.join(".agents/plugins/marketplace.json");
        plan.write(
            &marketplace_path,
            crate::util::json_pretty(&serde_json::json!({
                "name": STANDALONE_SKILLS_MARKETPLACE,
                "interface": { "displayName": "Blue Managed Skills" },
                "plugins": [{
                    "name": STANDALONE_SKILLS_PLUGIN,
                    "source": {
                        "source": "local",
                        "path": format!("./plugins/{STANDALONE_SKILLS_PLUGIN}")
                    },
                    "policy": {
                        "installation": "AVAILABLE",
                        "authentication": "ON_INSTALL"
                    },
                    "category": "Productivity"
                }]
            }))?,
        )?;
        report.files.push(marketplace_path);

        let installed = cache_root.join(STANDALONE_SKILLS_VERSION);
        plan.component_dirs(
            std::slice::from_ref(&plugin_root),
            &installed,
            &mut report.files,
        )?;

        let mut marketplace = toml::map::Map::new();
        marketplace.insert(
            "source".into(),
            Toml::String(marketplace_root.display().to_string()),
        );
        marketplace.insert("source_type".into(), Toml::String("local".into()));
        marketplaces.insert(
            STANDALONE_SKILLS_MARKETPLACE.into(),
            Toml::Table(marketplace),
        );
        let mut enabled = toml::map::Map::new();
        enabled.insert("enabled".into(), Toml::Boolean(true));
        plugins.insert(
            format!("{STANDALONE_SKILLS_PLUGIN}@{STANDALONE_SKILLS_MARKETPLACE}"),
            Toml::Table(enabled),
        );
    }

    if !marketplaces.is_empty() {
        config.insert("marketplaces".into(), Toml::Table(marketplaces));
    }
    if !plugins.is_empty() {
        config.insert("plugins".into(), Toml::Table(plugins));
    }
    let body = toml::to_string_pretty(&Toml::Table(config))
        .map_err(|error| GhError::Serde(error.to_string()))?;
    plan.write(&config_path, body)
}

fn migrate_legacy_global_config(
    plan: &mut crate::adapters::ReconcilePlan,
    path: &Path,
    policy: &HarnessPolicy,
) -> Result<(), GhError> {
    if !path.exists() {
        return Ok(());
    }
    let mut current = plan.read_toml_table(path)?;
    let matching_remote_mcp = current
        .get("mcp_servers")
        .and_then(Toml::as_table)
        .is_some_and(|servers| {
            policy.mcp.iter().any(|server| {
                servers.get(&server.name)
                    == Some(&codex_mcp_entry(server, approval_policy(policy) == "never"))
            })
        });
    let legacy = current.get("model_provider").and_then(Toml::as_str) == Some(GOVERNED_PROVIDER)
        || current
            .get("hooks")
            .is_some_and(|hooks| contains_managed_session_upload(hooks) || contains_managed_session_start(hooks))
        || matching_remote_mcp;
    if !legacy {
        return Ok(());
    }

    let prefix = format!(
        "{}.bak.harness.",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
    );
    let backup = path.parent().and_then(|parent| {
        std::fs::read_dir(parent)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .filter_map(|entry| {
                let table = plan.read_toml_table(&entry.path()).ok()?;
                let clean = table.get("model_provider").and_then(Toml::as_str)
                    != Some(GOVERNED_PROVIDER)
                    && !table
                        .get("hooks")
                        .is_some_and(|hooks| contains_managed_session_upload(hooks) || contains_managed_session_start(hooks));
                clean.then_some((entry.file_name(), table))
            })
            .max_by(|left, right| left.0.cmp(&right.0))
            .map(|(_, table)| table)
    });

    let mut restore_key = |key: &str, desired: Option<Toml>| {
        if desired
            .as_ref()
            .is_some_and(|value| current.get(key) == Some(value))
        {
            if let Some(value) = backup.as_ref().and_then(|table| table.get(key)).cloned() {
                current.insert(key.into(), value);
            } else {
                current.remove(key);
            }
        }
    };
    restore_key(
        "model",
        policy.managed_config.model.clone().map(Toml::String),
    );
    restore_key(
        "approval_policy",
        policy
            .managed_config
            .approval_policy
            .clone()
            .map(Toml::String),
    );
    restore_key(
        "sandbox_mode",
        policy.managed_config.sandbox_mode.clone().map(Toml::String),
    );
    restore_key(
        "model_reasoning_effort",
        policy
            .managed_config
            .reasoning_effort
            .clone()
            .map(Toml::String),
    );
    if policy.managed_config.fast_mode == Some(false) {
        restore_key("service_tier", Some(Toml::String("default".into())));
    }

    if current.get("model_provider").and_then(Toml::as_str) == Some(GOVERNED_PROVIDER) {
        if let Some(value) = backup
            .as_ref()
            .and_then(|table| table.get("model_provider"))
            .cloned()
        {
            current.insert("model_provider".into(), value);
        } else {
            current.remove("model_provider");
        }
    }
    if let Some(Toml::Table(providers)) = current.get_mut("model_providers") {
        providers.remove(GOVERNED_PROVIDER);
        if providers.is_empty() {
            current.remove("model_providers");
        }
    }
    if let Some(Toml::Table(features)) = current.get_mut("features") {
        if policy
            .managed_config
            .fast_mode
            .is_some_and(|value| features.get("fast_mode") == Some(&Toml::Boolean(value)))
        {
            if let Some(value) = backup
                .as_ref()
                .and_then(|table| table.get("features"))
                .and_then(Toml::as_table)
                .and_then(|features| features.get("fast_mode"))
                .cloned()
            {
                features.insert("fast_mode".into(), value);
            } else {
                features.remove("fast_mode");
            }
        }
    }
    if let Some(Toml::Table(servers)) = current.get_mut("mcp_servers") {
        for server in &policy.mcp {
            if servers.get(&server.name)
                != Some(&codex_mcp_entry(server, approval_policy(policy) == "never"))
            {
                continue;
            }
            if let Some(value) = backup
                .as_ref()
                .and_then(|table| table.get("mcp_servers"))
                .and_then(Toml::as_table)
                .and_then(|servers| servers.get(&server.name))
                .cloned()
            {
                servers.insert(server.name.clone(), value);
            } else {
                servers.remove(&server.name);
            }
        }
        if servers.is_empty() {
            current.remove("mcp_servers");
        }
    }
    if let Some(Toml::Table(hooks)) = current.get_mut("hooks") {
        if let Some(Toml::Array(entries)) = hooks.get_mut("SessionEnd") {
            entries.retain(|entry| !contains_managed_session_upload(entry));
            if entries.is_empty() {
                hooks.remove("SessionEnd");
            }
        }
        if let Some(Toml::Array(entries)) = hooks.get_mut("SessionStart") {
            entries.retain(|entry| !contains_managed_session_start(entry));
            if entries.is_empty() {
                hooks.remove("SessionStart");
            }
        }
        if hooks.is_empty() {
            current.remove("hooks");
        }
    }

    let body = toml::to_string_pretty(&Toml::Table(current))
        .map_err(|error| GhError::Serde(error.to_string()))?;
    plan.write(path, body)
}

fn contains_managed_session_upload(value: &Toml) -> bool {
    match value {
        Toml::String(value) => value.contains("session-upload codex"),
        Toml::Array(values) => values.iter().any(contains_managed_session_upload),
        Toml::Table(values) => values.values().any(contains_managed_session_upload),
        _ => false,
    }
}

fn contains_managed_session_start(value: &Toml) -> bool {
    match value {
        Toml::String(value) => value.contains("session-start codex"),
        Toml::Array(values) => values.iter().any(contains_managed_session_start),
        Toml::Table(values) => values.values().any(contains_managed_session_start),
        _ => false,
    }
}

fn codex_mcp_entry(s: &McpServer, approve_tools: bool) -> Toml {
    let mut m = toml::map::Map::new();
    if approve_tools {
        m.insert(
            "default_tools_approval_mode".into(),
            Toml::String("approve".into()),
        );
    }
    if let Some(cmd) = &s.command {
        m.insert("command".into(), Toml::String(cmd.clone()));
        m.insert(
            "args".into(),
            Toml::Array(s.args.iter().cloned().map(Toml::String).collect()),
        );
        if !s.env.is_empty() {
            let env: toml::map::Map<String, Toml> = s
                .env
                .iter()
                .map(|(k, v)| (k.clone(), Toml::String(v.clone())))
                .collect();
            m.insert("env".into(), Toml::Table(env));
        }
    } else if let Some(url) = &s.url {
        // Remote MCP — represented by URL; Codex versions vary, kept forward-safe.
        m.insert("url".into(), Toml::String(url.clone()));
    }
    Toml::Table(m)
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
        session_upload_hook: Option<Toml>,
    ) -> Result<HarnessWrite, GhError> {
        let mut plan = crate::adapters::ReconcilePlan::default();
        let result = write(&mut plan, home, policy, wiring, session_upload_hook, None)?;
        let mut transaction = crate::FileTransaction::begin(home, &plan)?;
        transaction.apply(&plan)?;
        transaction.commit();
        Ok(result)
    }

    use serde_json::json;

    #[test]
    fn preserves_unmanaged_existing_toml_and_mcp_servers() {
        let home = std::env::temp_dir().join(format!("gh-codex-merge-{}", std::process::id()));
        let path = home.join(".codex/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"model = "old-model"
model_reasoning_effort = "xhigh"
[features]
web_search = true
[mcp_servers.personal]
command = "personal-mcp"
[hooks]
SessionEnd = [{ hooks = [{ type = "command", command = "my-session-hook" }] }]
"#,
        )
        .unwrap();
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "model": "governed-model", "approval_policy": "never" },
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
            Some(crate::adapters::codex::v0_145_0::session_upload_hook("codex-v1").unwrap()),
        )
        .unwrap();
        let native = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        let managed = std::fs::read_to_string(home.join(".codex/blue.config.toml"))
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        assert_eq!(native["model"].as_str(), Some("old-model"));
        assert_eq!(native["model_reasoning_effort"].as_str(), Some("xhigh"));
        assert_eq!(native["features"]["web_search"].as_bool(), Some(true));
        assert_eq!(
            native["mcp_servers"]["personal"]["command"].as_str(),
            Some("personal-mcp")
        );
        assert_eq!(managed["model"].as_str(), Some("governed-model"));
        assert!(managed["mcp_servers"].get("personal").is_none());
        assert_eq!(
            managed["mcp_servers"]["blocks"]["command"].as_str(),
            Some("npx")
        );
        assert_eq!(
            managed["mcp_servers"]["blocks"]["default_tools_approval_mode"].as_str(),
            Some("approve")
        );
        assert!(managed["mcp_servers"].get("disabled-remote").is_none());
        let hooks = managed["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(hooks.len(), 2);
        assert!(hooks
            .iter()
            .any(|hook| format!("{hook:?}").contains("my-session-hook")));
        assert!(hooks
            .iter()
            .any(|hook| format!("{hook:?}").contains("session-upload codex")));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn preserves_hook_approval_state_until_session_upload_is_disabled() {
        let home = std::env::temp_dir().join(format!("gh-codex-hook-state-{}", std::process::id()));
        let managed_path = home.join(".codex/blue.config.toml");
        let enabled = HarnessPolicy::default();

        test_write(
            &home,
            &enabled,
            None,
            Some(crate::adapters::codex::v0_145_0::session_upload_hook("codex-v1").unwrap()),
        )
        .unwrap();
        let mut managed = std::fs::read_to_string(&managed_path)
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        let mut approval = toml::map::Map::new();
        approval.insert(
            "trusted_hash".into(),
            Toml::String("sha256:approved-hook".into()),
        );
        let mut state = toml::map::Map::new();
        state.insert(
            "blue.config.toml:session_end:0:0".into(),
            Toml::Table(approval),
        );
        managed["hooks"]
            .as_table_mut()
            .unwrap()
            .insert("state".into(), Toml::Table(state));
        std::fs::write(&managed_path, toml::to_string_pretty(&managed).unwrap()).unwrap();

        test_write(
            &home,
            &enabled,
            None,
            Some(crate::adapters::codex::v0_145_0::session_upload_hook("codex-v1").unwrap()),
        )
        .unwrap();
        let reconciled = std::fs::read_to_string(&managed_path)
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        assert_eq!(
            reconciled["hooks"]["state"]["blue.config.toml:session_end:0:0"]["trusted_hash"]
                .as_str(),
            Some("sha256:approved-hook")
        );

        test_write(&home, &HarnessPolicy::default(), None, None).unwrap();
        let disabled = std::fs::read_to_string(&managed_path)
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        assert!(disabled.get("hooks").is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn defaults_to_interactive_approval_without_disabling_the_sandbox() {
        let home =
            std::env::temp_dir().join(format!("gh-codex-auto-approve-{}", std::process::id()));
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "sandbox_mode": "workspace-write" }
        }))
        .unwrap();

        test_write(&home, &policy, None, None).unwrap();
        let managed = std::fs::read_to_string(home.join(".codex/blue.config.toml"))
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        assert_eq!(managed["approval_policy"].as_str(), Some("on-request"));
        assert_eq!(managed["sandbox_mode"].as_str(), Some("workspace-write"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn explicit_auto_approve_false_restores_interactive_approval() {
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "auto_approve": false }
        }))
        .unwrap();
        assert_eq!(approval_policy(&policy), "on-request");
    }

    #[test]
    fn explicit_auto_approve_true_enables_non_interactive_approval() {
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "auto_approve": true }
        }))
        .unwrap();
        assert_eq!(approval_policy(&policy), "never");
    }

    #[test]
    fn explicit_approval_policy_wins_over_portable_auto_approve() {
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": {
                "approval_policy": "on-request",
                "auto_approve": true
            }
        }))
        .unwrap();
        assert_eq!(approval_policy(&policy), "on-request");
    }

    #[test]
    fn interactive_approval_does_not_preapprove_managed_mcp_tools() {
        let home =
            std::env::temp_dir().join(format!("gh-codex-interactive-mcp-{}", std::process::id()));
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "auto_approve": false },
            "mcp": [{ "name": "blocks", "command": "npx", "args": ["blocks"] }]
        }))
        .unwrap();

        test_write(&home, &policy, None, None).unwrap();
        let managed = std::fs::read_to_string(home.join(".codex/blue.config.toml"))
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        assert!(managed["mcp_servers"]["blocks"]
            .get("default_tools_approval_mode")
            .is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn writes_reasoning_effort_and_disables_fast_mode_without_losing_features() {
        let home = std::env::temp_dir().join(format!("gh-codex-policy-{}", std::process::id()));
        let path = home.join(".codex/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "service_tier = \"fast\"\n[features]\nweb_search = true\n",
        )
        .unwrap();
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": {
                "model": "gpt-5.6-sol",
                "reasoning_effort": "medium",
                "fast_mode": false
            }
        }))
        .unwrap();

        test_write(&home, &policy, None, None).unwrap();
        let native = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        let managed = std::fs::read_to_string(home.join(".codex/blue.config.toml"))
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        assert_eq!(native["service_tier"].as_str(), Some("fast"));
        assert_eq!(native["features"]["web_search"].as_bool(), Some(true));
        assert_eq!(managed["model"].as_str(), Some("gpt-5.6-sol"));
        assert_eq!(managed["model_reasoning_effort"].as_str(), Some("medium"));
        assert_eq!(managed["service_tier"].as_str(), Some("default"));
        assert_eq!(managed["features"]["fast_mode"].as_bool(), Some(false));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn migrates_legacy_global_values_from_clean_backup() {
        let home = std::env::temp_dir().join(format!("gh-codex-migrate-{}", std::process::id()));
        let path = home.join(".codex/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path.with_file_name("config.toml.bak.harness.1"),
            "model = \"personal\"\nmodel_reasoning_effort = \"high\"\n",
        )
        .unwrap();
        std::fs::write(
            &path,
            r#"model = "governed-model"
model_provider = "governed"
model_reasoning_effort = "medium"
[model_providers.governed]
base_url = "http://proxy"
[hooks]
SessionEnd = [{ hooks = [{ type = "command", command = "harness session-upload codex" }] }]
"#,
        )
        .unwrap();
        let policy: HarnessPolicy = serde_json::from_value(json!({
            "managed_config": { "model": "governed-model", "reasoning_effort": "medium" }
        }))
        .unwrap();
        test_write(&home, &policy, None, None).unwrap();
        let native = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<Toml>()
            .unwrap();
        assert_eq!(native["model"].as_str(), Some("personal"));
        assert_eq!(native["model_reasoning_effort"].as_str(), Some("high"));
        assert!(native.get("model_provider").is_none());
        assert!(native.get("hooks").is_none());
        let _ = std::fs::remove_dir_all(home);
    }
}
