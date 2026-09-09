//! Shared Kimi-family inspection.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use gh_service::{GatewayConfig, HarnessPolicy};

use crate::WriteOptions;

fn find<'a>(files: &'a [PathBuf], suffix: &str) -> Option<&'a Path> {
    files
        .iter()
        .find(|path| path.ends_with(suffix))
        .map(PathBuf::as_path)
}

fn toml(path: &Path) -> Option<toml::Value> {
    std::fs::read_to_string(path).ok()?.parse().ok()
}

fn json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn names(value: Option<&serde_json::Map<String, serde_json::Value>>) -> String {
    let names = value
        .into_iter()
        .flat_map(|map| map.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    if names.is_empty() {
        "none".into()
    } else {
        names
    }
}

pub(crate) fn kimi_current(_: &HarnessPolicy, files: &[PathBuf]) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let config = find(files, ".config/blue/runtime/kimi/config.toml")
        .or_else(|| find(files, ".kimi-code/config.toml"))
        .and_then(toml);
    if let Some(config) = config {
        if let Some(value) = config.get("default_model").and_then(toml::Value::as_str) {
            values.insert("model".into(), value.into());
        }
        if let Some(value) = config.get("default_yolo").and_then(toml::Value::as_bool) {
            values.insert("auto_approve".into(), value.to_string());
        }
        if let Some(value) = config
            .get("providers")
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get("governed"))
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get("base_url"))
            .and_then(toml::Value::as_str)
        {
            values.insert("gateway".into(), value.into());
        }
    }
    if find(files, ".config/blue/runtime/kimi/config.toml")
        .or_else(|| find(files, ".kimi-code/config.toml"))
        .and_then(toml)
        .is_some_and(|value| format!("{value:?}").contains("session-upload kimi"))
    {
        values.insert("session_upload_hook".into(), "enabled".into());
    }
    if let Some(root) = find(files, ".config/blue/runtime/kimi/mcp.json")
        .or_else(|| find(files, ".kimi-code/mcp.json"))
        .and_then(json)
    {
        values.insert(
            "mcp".into(),
            names(
                root.get("mcpServers")
                    .and_then(serde_json::Value::as_object),
            ),
        );
    }
    values
}

pub(crate) fn kimi_proposed(
    mut values: BTreeMap<String, String>,
    policy: &HarnessPolicy,
    gateway: Option<&GatewayConfig>,
    opts: WriteOptions,
) -> BTreeMap<String, String> {
    if let Some(model) = &policy.managed_config.model {
        values.insert("model".into(), model.clone());
    }
    if let Some(value) = &policy.managed_config.approval_policy {
        values.insert("approval_policy".into(), value.clone());
    }
    if let Some(value) = policy.managed_config.auto_approve {
        values.insert("auto_approve".into(), value.to_string());
    }
    if let Some(value) = &policy.managed_config.sandbox_mode {
        values.insert("sandbox_mode".into(), value.clone());
    }
    if opts.gateway_enabled {
        if let Some(proxy) = gateway.and_then(|gateway| gateway.proxy_url.as_ref()) {
            values.insert("gateway".into(), proxy.clone());
        }
    }
    if !policy.mcp.is_empty() {
        let mut current = values
            .get("mcp")
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "none")
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        for entry in &policy.mcp {
            if entry.disabled {
                current.remove(&entry.name);
            } else {
                current.insert(entry.name.clone());
            }
        }
        let value = current.into_iter().collect::<Vec<_>>().join(", ");
        values.insert(
            "mcp".into(),
            if value.is_empty() {
                "none".into()
            } else {
                value
            },
        );
    }
    if opts.session_upload_enabled {
        values.insert("session_upload_hook".into(), "enabled".into());
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_inspection_uses_kimi_code_and_runtime_overlay_paths() {
        let home = std::env::temp_dir().join(format!("blue-kimi-inspect-{}", std::process::id()));
        let native = home.join(".kimi-code/config.toml");
        let runtime = home.join(".config/blue/runtime/kimi/config.toml");
        let mcp = home.join(".config/blue/runtime/kimi/mcp.json");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        std::fs::write(&native, "default_model = \"native\"\n").unwrap();
        std::fs::write(&runtime, "default_model = \"managed\"\n").unwrap();
        std::fs::write(&mcp, r#"{"mcpServers":{"blue":{"command":"blue"}}}"#).unwrap();
        let values = kimi_current(&HarnessPolicy::default(), &[native, runtime, mcp]);
        assert_eq!(values.get("model").map(String::as_str), Some("managed"));
        assert_eq!(values.get("mcp").map(String::as_str), Some("blue"));
        let _ = std::fs::remove_dir_all(home);
    }
}
