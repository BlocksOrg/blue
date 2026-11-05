//! OpenCode 2.x launch overlay. Native v2 configuration travels in
//! `OPENCODE_CONFIG_CONTENT`, and managed components are referenced by absolute
//! path: in v2 `OPENCODE_CONFIG_DIR` replaces the user's global configuration
//! directory instead of layering over it, so Blue no longer sets it.

use std::path::{Path, PathBuf};

use gh_common::GhError;
use gh_gateway::GatewayWiring;
use gh_service::HarnessPolicy;
use serde_json::{json, Map, Value};

use super::writer::{
    governed_catalog, migrate_legacy_global_config, native_governed_model, plan_skills,
    GATEWAY_TOKEN_ENV, GOVERNED_PROVIDER,
};
use crate::adapters::PackageComponents;
use crate::util::json_pretty;
use crate::HarnessWrite;

const SCHEMA: &str = "https://opencode.ai/config.json";
/// Session plugin directory name; v2 loads local plugins from directories.
pub(super) const SESSION_PLUGIN_DIR: &str = "blue-session-upload";
/// Every v2 launch runs a private server. The default background service is
/// shared per user and never sees Blue's launch environment, so the managed
/// configuration, gateway token, and session identity would not reach it.
pub(super) const STANDALONE: &str = "--standalone";

pub fn write(
    plan: &mut crate::adapters::ReconcilePlan,
    home: &Path,
    policy: &HarnessPolicy,
    wiring: Option<&GatewayWiring>,
    session_upload_plugin: Option<String>,
    components: &PackageComponents,
) -> Result<HarnessWrite, GhError> {
    let native_path = home.join(".config").join("opencode").join("opencode.json");
    migrate_legacy_global_config(plan, home, &native_path, policy)?;
    let native = plan.read_json_object(&native_path)?;
    let runtime = crate::managed_runtime_dir(home).join("opencode");
    let cfg_path = runtime.join("opencode.json");
    let mut root = Map::new();
    root.insert("$schema".into(), json!(SCHEMA));

    let catalog = governed_catalog(policy);
    let mut warnings = Vec::new();
    // Gateway mode always pins the model; see the v1 writer for the rationale.
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

    root.insert("permissions".into(), permission_rules(policy));
    root.insert("mcp".into(), mcp_block(&native, policy));

    let mut files = Vec::new();
    let mut env = std::collections::BTreeMap::new();
    if let Some(w) = wiring {
        let mut models = Map::new();
        if let Some(model) = &effective_model {
            models.insert(model.clone(), json!({ "name": model }));
        }
        for available in &catalog {
            models
                .entry(available.clone())
                .or_insert_with(|| json!({ "name": available }));
        }
        root.insert(
            "providers".into(),
            json!({
                GOVERNED_PROVIDER: {
                    // Built in, so the provider needs no runtime package download.
                    "package": "@opencode/ai/providers/openai-compatible",
                    "name": "Governed (Blue)",
                    "settings": {
                        "baseURL": w.base_url,
                        "apiKey": format!("{{env:{GATEWAY_TOKEN_ENV}}}")
                    },
                    "models": models
                }
            }),
        );
        // The v2 replacement for `enabled_providers`: the last matching
        // statement wins, so deny everything and then allow the gateway.
        // Policies from the launch overlay rank below the user's own config
        // files, the same trust boundary as every other overlay setting.
        root.insert(
            "policies".into(),
            json!([
                { "action": "provider.use", "resource": "*", "effect": "deny" },
                { "action": "provider.use", "resource": GOVERNED_PROVIDER, "effect": "allow" }
            ]),
        );
        env.insert(GATEWAY_TOKEN_ENV.into(), w.token.clone());
    }

    // Rebuild the plugin tree on every reconcile. This also removes the flat
    // v1 `plugins/blue-session-upload.js`, which v2 would load as a broken
    // plugin if it were ever discovered.
    let plugins_root = runtime.join("plugins");
    plan.remove(&plugins_root)?;
    let mut plugin_dirs = Vec::new();
    if let Some(plugin) = session_upload_plugin {
        let dir = plugins_root.join(SESSION_PLUGIN_DIR);
        let entry = dir.join("index.js");
        plan.write(&entry, plugin)?;
        files.push(entry);
        plugin_dirs.push(dir);
    }
    for module in &components.plugin_modules {
        let dir = plugins_root.join(plugin_dir_name(module)?);
        if plugin_dirs.contains(&dir) {
            return Err(GhError::config(format!(
                "OpenCode plugin module collision at {}",
                dir.display()
            )));
        }
        let extension = module
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("js");
        plan.write(&dir.join(format!("index.{extension}")), plan.read(module)?)?;
        files.push(dir.clone());
        plugin_dirs.push(dir);
    }
    if !plugin_dirs.is_empty() {
        root.insert("plugins".into(), json!(paths(&plugin_dirs)));
    }

    let skills = runtime.join("skills");
    plan_skills(plan, &components.skills_dirs, &skills, &mut files)?;
    if !components.skills_dirs.is_empty() {
        root.insert("skills".into(), json!(paths(std::slice::from_ref(&skills))));
    }
    plan.component_dirs(&components.agents_dirs, &runtime.join("agents"), &mut files)?;
    root.extend(agent_definitions(plan, &runtime.join("agents"))?);
    // Package hooks are plugin modules for OpenCode; nothing else is emitted.
    plan.remove(&runtime.join("hooks"))?;

    let mut config = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|error| GhError::Serde(error.to_string()))?;
    config.push('\n');
    plan.write(&cfg_path, config.as_bytes())?;
    files.insert(0, cfg_path);
    env.insert("OPENCODE_CONFIG_CONTENT".into(), config);
    env.extend(components.env.clone());
    crate::util::prepend_helper_paths(&mut env, &components.helpers);

    let mut launch_args = vec![STANDALONE.to_owned()];
    launch_args.extend(components.launch_args.iter().cloned());
    Ok(HarnessWrite {
        files,
        env,
        launch_args,
        package_errors: Vec::new(),
        warnings,
    })
}

/// The native spelling of v1's `autoupdate: false`. As in 1.x, the updater
/// itself only reads the global config files, so `OPENCODE_DISABLE_AUTOUPDATE`
/// remains the effective suppression and this key is kept for parity.
pub(super) fn disable_update(config: &str) -> Result<String, GhError> {
    let mut root: Value = serde_json::from_str(config)
        .map_err(|error| GhError::Serde(format!("parsing managed OpenCode config: {error}")))?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| GhError::config("managed OpenCode config must be a JSON object"))?;
    object.insert("update".into(), json!("disable"));
    String::from_utf8(json_pretty(&root)?)
        .map_err(|error| GhError::Serde(format!("encoding managed OpenCode config: {error}")))
}

fn paths(dirs: &[PathBuf]) -> Vec<String> {
    dirs.iter().map(|dir| dir.display().to_string()).collect()
}

fn plugin_dir_name(module: &Path) -> Result<String, GhError> {
    let stem = module
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .ok_or_else(|| GhError::config("OpenCode plugin module has no usable file name"))?;
    if stem == SESSION_PLUGIN_DIR {
        return Err(GhError::config(format!(
            "OpenCode plugin module name `{stem}` is reserved by Blue"
        )));
    }
    Ok(stem.to_owned())
}

/// The managed permission posture: allow everything except `question`, and
/// gate mutating tools behind `ask` when auto-approve is disabled. Global rules
/// are appended after each built-in agent's own, so they also override the
/// `build` agent's `question` allow. The v1 `fetch`, `todowrite`, and `todoread`
/// keys map to `webfetch` and `todowrite`; `list` and `mcp` have no v2 action.
fn permission_rules(policy: &HarnessPolicy) -> Value {
    let action = if policy.managed_config.auto_approve == Some(false) {
        "ask"
    } else {
        "allow"
    };
    Value::Array(
        [
            ("read", "allow"),
            ("glob", "allow"),
            ("grep", "allow"),
            ("edit", action),
            ("shell", action),
            ("external_directory", action),
            ("webfetch", "allow"),
            ("subagent", "allow"),
            ("skill", "allow"),
            ("todowrite", "allow"),
            ("question", "deny"),
        ]
        .into_iter()
        .map(|(name, effect)| json!({ "action": name, "resource": "*", "effect": effect }))
        .collect(),
    )
}

/// Native servers win by name in either the v1 (`mcp.<name>`) or v2
/// (`mcp.servers.<name>`) shape; the v2 normalizer accepts both side by side.
fn mcp_block(native: &Map<String, Value>, policy: &HarnessPolicy) -> Value {
    let local = native.get("mcp").and_then(Value::as_object);
    let native_v2 = local
        .and_then(|mcp| mcp.get("servers"))
        .and_then(Value::as_object);
    let mut servers = Map::new();
    for s in &policy.mcp {
        if s.disabled
            || local.is_some_and(|mcp| mcp.contains_key(&s.name))
            || native_v2.is_some_and(|mcp| mcp.contains_key(&s.name))
        {
            continue;
        }
        let mut entry = Map::new();
        if let Some(cmd) = &s.command {
            let mut command = vec![cmd.clone()];
            command.extend(s.args.iter().cloned());
            entry.insert("type".into(), json!("local"));
            entry.insert("command".into(), json!(command));
            if !s.env.is_empty() {
                entry.insert("environment".into(), json!(s.env));
            }
        } else if let Some(url) = &s.url {
            entry.insert("type".into(), json!("remote"));
            entry.insert("url".into(), json!(url));
        }
        entry.insert("disabled".into(), json!(false));
        servers.insert(s.name.clone(), Value::Object(entry));
    }
    json!({ "servers": servers })
}

/// Fields only the v2 agent schema understands. OpenCode reads a definition
/// that uses any of them as v2; otherwise unknown fields mark it as v1.
const NATIVE_AGENT_KEYS: &[&str] = &["permissions", "request", "system", "disabled"];
/// Keys of the v2 agent schema (plus the v1 `variant`, which v2 joins into `model`).
const AGENT_KEYS: &[&str] = &[
    "description",
    "mode",
    "model",
    "variant",
    "system",
    "permissions",
    "request",
    "disabled",
    "hidden",
    "color",
    "steps",
];

/// v2 has no configuration key for an extra agent directory and only loads
/// agent files from its own config roots, which Blue does not own. Package
/// agents are therefore inlined with the same v1/v2 split OpenCode applies to
/// agent files: native definitions go under `agents` with the body as
/// `system`, and legacy ones under the v1 `agent` map with the body as
/// `prompt`, which OpenCode migrates in memory.
fn agent_definitions(
    plan: &crate::adapters::ReconcilePlan,
    root: &Path,
) -> Result<Map<String, Value>, GhError> {
    let mut sources = plan
        .writes
        .iter()
        .filter(|write| {
            write.path.starts_with(root)
                && write.path.extension().is_some_and(|extension| extension == "md")
        })
        .collect::<Vec<_>>();
    sources.sort_by(|left, right| left.path.cmp(&right.path));
    let mut native = Map::new();
    let mut legacy = Map::new();
    for source in sources {
        let name = source
            .path
            .strip_prefix(root)
            .expect("filtered by prefix")
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let text = String::from_utf8(source.body.clone()).map_err(|_| {
            GhError::config(format!("agent {} is not UTF-8", source.path.display()))
        })?;
        let (mut data, body) = frontmatter(&text)
            .map_err(|error| GhError::config(format!("agent {name}: {error}")))?;
        let is_legacy = data.keys().any(|key| !AGENT_KEYS.contains(&key.as_str()))
            && !data
                .keys()
                .any(|key| NATIVE_AGENT_KEYS.contains(&key.as_str()));
        if is_legacy {
            // The ID comes from the path, as for files; a v1 JSON `name` only
            // produces an "unsupported field" warning in 2.x.
            data.remove("name");
            data.insert("prompt".into(), json!(body));
            legacy.insert(name, Value::Object(data));
            continue;
        }
        if let (Some(Value::String(model)), Some(Value::String(variant))) =
            (data.get("model"), data.get("variant"))
        {
            if !model.contains('#') && !variant.contains('#') {
                let joined = format!("{model}#{variant}");
                data.insert("model".into(), json!(joined));
            }
        }
        data.remove("variant");
        data.insert("system".into(), json!(body));
        native.insert(name, Value::Object(data));
    }
    let mut definitions = Map::new();
    if !native.is_empty() {
        definitions.insert("agents".into(), Value::Object(native));
    }
    if !legacy.is_empty() {
        definitions.insert("agent".into(), Value::Object(legacy));
    }
    Ok(definitions)
}

/// Split optional `---` YAML frontmatter from a Markdown body.
fn frontmatter(text: &str) -> Result<(Map<String, Value>, String), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok((Map::new(), text.trim().to_owned()));
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let data = if rest[..offset].trim().is_empty() {
                Value::Object(Map::new())
            } else {
                serde_yaml::from_str::<Value>(&rest[..offset])
                    .map_err(|error| format!("invalid frontmatter: {error}"))?
            };
            let Value::Object(data) = data else {
                return Err("frontmatter must be a mapping".into());
            };
            return Ok((data, rest[offset + line.len()..].trim().to_owned()));
        }
        offset += line.len();
    }
    Err("unterminated frontmatter".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(label: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "gh-opencode-v2-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn render(
        home: &Path,
        policy: Value,
        wiring: Option<&GatewayWiring>,
        components: &PackageComponents,
    ) -> (HarnessWrite, Value, crate::adapters::ReconcilePlan) {
        let policy: HarnessPolicy = serde_json::from_value(policy).unwrap();
        let mut plan = crate::adapters::ReconcilePlan::default();
        let report = write(&mut plan, home, &policy, wiring, None, components).unwrap();
        let config = serde_json::from_str(&report.env["OPENCODE_CONFIG_CONTENT"]).unwrap();
        (report, config, plan)
    }

    #[test]
    fn governance_renders_native_v2_shape_without_replacing_the_config_dir() {
        let home = home("governance");
        let (report, config, _) = render(
            &home,
            json!({ "managed_config": { "model": "org/model" } }),
            None,
            &PackageComponents::default(),
        );
        assert_eq!(report.launch_args, [STANDALONE]);
        assert!(!report.env.contains_key("OPENCODE_CONFIG_DIR"));
        assert_eq!(config["model"], "org/model");
        assert!(config.get("permission").is_none());
        assert!(config.get("providers").is_none());
        assert!(config.get("policies").is_none());
        assert!(config.get("plugins").is_none());
        let rules = config["permissions"].as_array().unwrap();
        assert_eq!(
            rules.last().unwrap(),
            &json!({ "action": "question", "resource": "*", "effect": "deny" })
        );
        assert!(rules
            .iter()
            .all(|rule| !["bash", "task", "write", "list", "mcp", "todoread"]
                .contains(&rule["action"].as_str().unwrap())));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn gateway_uses_a_native_provider_and_a_provider_policy() {
        let home = home("gateway");
        let wiring = GatewayWiring {
            base_url: "https://gateway.example/v1".into(),
            token: "jwt".into(),
            wire_api: None,
            auth: gh_gateway::AuthPlacement::InFile,
        };
        let (report, config, _) = render(
            &home,
            json!({ "managed_config": { "model": "org/model" } }),
            Some(&wiring),
            &PackageComponents::default(),
        );
        let governed = &config["providers"][GOVERNED_PROVIDER];
        assert_eq!(governed["package"], "@opencode/ai/providers/openai-compatible");
        assert_eq!(governed["settings"]["baseURL"], "https://gateway.example/v1");
        assert_eq!(governed["settings"]["apiKey"], "{env:BLUE_OPENCODE_GATEWAY_TOKEN}");
        assert_eq!(governed["models"]["org/model"]["name"], "org/model");
        assert_eq!(config["model"], "governed/org/model");
        assert_eq!(
            config["policies"],
            json!([
                { "action": "provider.use", "resource": "*", "effect": "deny" },
                { "action": "provider.use", "resource": "governed", "effect": "allow" }
            ])
        );
        assert!(config.get("enabled_providers").is_none());
        assert_eq!(report.env[GATEWAY_TOKEN_ENV], "jwt");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn native_servers_win_in_either_config_shape() {
        let home = home("mcp");
        let native = home.join(".config/opencode/opencode.json");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::write(
            &native,
            json!({ "mcp": { "v1-native": { "type": "local" }, "servers": { "v2-native": {} } } })
                .to_string(),
        )
        .unwrap();
        let (_, config, _) = render(
            &home,
            json!({ "mcp": [
                { "name": "v1-native", "command": "x" },
                { "name": "v2-native", "command": "x" },
                { "name": "off", "command": "x", "disabled": true },
                { "name": "local", "command": "run", "args": ["--stdio"], "env": { "K": "V" } },
                { "name": "remote", "url": "https://mcp.example" }
            ] }),
            None,
            &PackageComponents::default(),
        );
        assert_eq!(
            config["mcp"],
            json!({ "servers": {
                "local": { "type": "local", "command": ["run", "--stdio"], "environment": { "K": "V" }, "disabled": false },
                "remote": { "type": "remote", "url": "https://mcp.example", "disabled": false }
            } })
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn package_components_are_referenced_by_absolute_directory() {
        let home = home("components");
        let source = home.join("package");
        std::fs::create_dir_all(source.join("skills/review")).unwrap();
        std::fs::create_dir_all(source.join("agents/team")).unwrap();
        std::fs::write(source.join("skills/review/SKILL.md"), "# Review\n").unwrap();
        std::fs::write(source.join("tool.js"), "export default { id: 'tool', setup() {} }\n")
            .unwrap();
        std::fs::write(
            source.join("agents/legacy.md"),
            "---\nname: legacy\ndescription: Old\ntools:\n  write: false\n---\nBe careful.\n",
        )
        .unwrap();
        std::fs::write(
            source.join("agents/team/native.md"),
            "---\ndescription: New\nmodel: org/model\nvariant: high\npermissions: []\n---\nBe brief.\n",
        )
        .unwrap();
        std::fs::write(source.join("agents/plain.md"), "Just a prompt.\n").unwrap();
        let components = PackageComponents {
            plugin_modules: vec![source.join("tool.js")],
            skills_dirs: vec![source.join("skills/review")],
            agents_dirs: vec![source.join("agents")],
            ..Default::default()
        };
        let (_, config, plan) = render(&home, json!({}), None, &components);
        let runtime = crate::managed_runtime_dir(&home).join("opencode");
        assert_eq!(
            config["plugins"],
            json!([runtime.join("plugins/tool").display().to_string()])
        );
        assert!(plan
            .writes
            .iter()
            .any(|write| write.path == runtime.join("plugins/tool/index.js")));
        assert_eq!(
            config["skills"],
            json!([runtime.join("skills").display().to_string()])
        );
        assert_eq!(
            config["agent"]["legacy"],
            json!({ "description": "Old", "tools": { "write": false }, "prompt": "Be careful." })
        );
        assert_eq!(
            config["agents"]["team/native"],
            json!({ "description": "New", "model": "org/model#high", "permissions": [], "system": "Be brief." })
        );
        assert_eq!(config["agents"]["plain"], json!({ "system": "Just a prompt." }));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_session_plugin_name_is_reserved_and_frontmatter_must_close() {
        assert!(plugin_dir_name(Path::new("/p/blue-session-upload.js")).is_err());
        assert_eq!(plugin_dir_name(Path::new("/p/tool.ts")).unwrap(), "tool");
        assert!(frontmatter("---\ndescription: x\n").is_err());
        assert!(frontmatter("---\n- list\n---\nbody").is_err());
        assert_eq!(frontmatter("---\n---\nbody").unwrap().1, "body");
    }
}
