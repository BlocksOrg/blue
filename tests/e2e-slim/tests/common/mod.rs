//! Shared assertions for the managed-config tests. Kept here (not in `src/lib.rs`)
//! because they parse TOML via the `toml` dev-dependency, which the library crate
//! cannot see. Every integration test that needs them does `mod common;`.
#![allow(dead_code)]

use std::path::Path;

use e2e_slim::Home;

/// The four governed agents paired with the governed model each one's managed
/// config must carry (real model ids — LiteLLM maps them onto the matching
/// OpenRouter deployments, see fixtures/litellm-config.yaml). Every
/// managed-config test iterates this so all four are covered uniformly — no
/// agent tested more than another.
pub const AGENTS: [(&str, &str); 4] = [
    ("codex", "gpt-5.6-terra"),
    ("claude", "claude-sonnet-5"),
    ("kimi", "kimi-k2.6"),
    ("opencode", "gpt-5.6-terra"),
];

pub fn read_toml(path: &Path) -> toml::Value {
    toml::from_str(&std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("reading {}: {error}", path.display());
    }))
    .unwrap_or_else(|error| panic!("parsing {}: {error}", path.display()))
}

pub fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap_or_else(|error| {
        panic!("reading {}: {error}", path.display());
    }))
    .unwrap_or_else(|error| panic!("parsing {}: {error}", path.display()))
}

/// Assert `blue apply` wrote the governed model into `agent`'s managed config.
pub fn assert_governed_model(home: &Home, agent: &str, expected: &str) {
    let root = home.path();
    let actual = match agent {
        "codex" => read_toml(&root.join(".codex/blue.config.toml"))
            .get("model")
            .and_then(|value| value.as_str().map(str::to_owned)),
        "claude" => read_json(&home.data_path().join("runtime/claude/settings.json"))
            .get("model")
            .and_then(|value| value.as_str().map(str::to_owned)),
        "kimi" => read_toml(&home.data_path().join("runtime/kimi/config.toml"))
            .get("default_model")
            .and_then(|value| value.as_str().map(str::to_owned)),
        "opencode" => read_json(&home.data_path().join("runtime/opencode/opencode.json"))
            .get("model")
            .and_then(|value| value.as_str().map(str::to_owned)),
        other => panic!("unknown agent {other}"),
    };
    assert_eq!(
        actual.as_deref(),
        Some(expected),
        "{agent} managed config should carry governed model {expected}"
    );
}

/// Claude 2.1.242+ must expose exactly Blue's ordered assignment and replace
/// the native picker options, not merely set the selected model.
pub fn assert_claude_catalog(home: &Home, expected: &[&str]) {
    let settings = read_json(&home.data_path().join("runtime/claude/settings.json"));
    assert_eq!(settings["availableModels"], serde_json::json!(expected));
    assert_eq!(settings["enforceAvailableModels"], true);
    assert_eq!(
        settings["modelPicker"],
        serde_json::json!({
            "options": expected.iter().map(|model| serde_json::json!({
                "model": model,
                "label": model,
            })).collect::<Vec<_>>(),
            "replaceBuiltInOptions": true,
        })
    );
}

/// Kimi gateway launches must replace native model/provider definitions with
/// exactly Blue's ordered assignment and select the first entry by default.
pub fn assert_kimi_catalog(home: &Home, expected: &[&str]) {
    let path = home.data_path().join("runtime/kimi/config.toml");
    let body = std::fs::read_to_string(&path).expect("reading Kimi runtime config");
    let config = read_toml(&path);
    assert_eq!(config["default_model"].as_str(), expected.first().copied());
    let models = config["models"].as_table().expect("Kimi models table");
    assert_eq!(models.len(), expected.len());
    for model in expected {
        assert_eq!(models[*model]["provider"].as_str(), Some("governed"));
    }
    assert!(models.get("native-model").is_none());
    let providers = config["providers"].as_table().expect("Kimi providers table");
    assert_eq!(providers.len(), 1);
    assert!(providers.get("governed").is_some());
    assert!(providers.get("native-provider").is_none());
    for pair in expected.windows(2) {
        let first = body.find(&format!("[models.{}]", pair[0])).unwrap();
        let second = body.find(&format!("[models.{}]", pair[1])).unwrap();
        assert!(first < second, "Kimi model entries must retain assignment order");
    }
}

/// Assert the managed `e2e-remote` MCP server was registered in `agent`'s config.
/// The location and shape differ per agent, but every one must reference `node`.
pub fn assert_mcp_registered(home: &Home, agent: &str) {
    let root = home.path();
    match agent {
        "codex" => {
            let config = read_toml(&root.join(".codex/blue.config.toml"));
            let command = config
                .get("mcp_servers")
                .and_then(|servers| servers.get("e2e-remote"))
                .and_then(|server| server.get("command"))
                .and_then(toml::Value::as_str);
            assert_eq!(
                command,
                Some("node"),
                "codex blue.config.toml should register the e2e-remote MCP server: {config:?}"
            );
        }
        "claude" | "kimi" => {
            let file = if agent == "claude" {
                "runtime/claude/mcp.json"
            } else {
                "runtime/kimi/mcp.json"
            };
            let mcp = read_json(&home.data_path().join(file));
            assert_eq!(
                mcp.pointer("/mcpServers/e2e-remote/command")
                    .and_then(serde_json::Value::as_str),
                Some("node"),
                "{agent} mcp.json should register the e2e-remote MCP server: {mcp}"
            );
        }
        "opencode" => {
            let config = read_json(&home.data_path().join("runtime/opencode/opencode.json"));
            // OpenCode collapses command+args into one array under mcp.<name>.
            let first = config
                .pointer("/mcp/e2e-remote/command/0")
                .and_then(serde_json::Value::as_str);
            assert_eq!(
                first,
                Some("node"),
                "opencode.json should register the e2e-remote MCP server: {config}"
            );
        }
        other => panic!("unknown agent {other}"),
    }
}

/// Assert `blue apply` materialized the managed `example` skill somewhere under
/// this HOME. The exact runtime path differs per agent (standalone-skills plugin,
/// runtime/skills, codex marketplace plugin, …), so we search the isolated HOME
/// for the skill folder rather than hard-coding four paths.
pub fn assert_example_skill_materialized(home: &Home, agent: &str) {
    assert!(
        find_example_skill(home.data_path()) || find_example_skill(&home.path().join(".codex")),
        "{agent}: `blue apply` should materialize the managed `example` skill under {}",
        home.path().display()
    );
}

fn find_example_skill(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "example")
                && path.join("SKILL.md").is_file()
            {
                return true;
            }
            if find_example_skill(&path) {
                return true;
            }
        }
    }
    false
}
