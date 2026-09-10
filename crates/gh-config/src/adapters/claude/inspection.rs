//! Shared Claude-family inspection.
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

pub(crate) fn claude_current(_: &HarnessPolicy, files: &[PathBuf]) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    if let Some(settings) = find(files, "runtime/claude/settings.json")
        .or_else(|| find(files, ".claude/settings.json"))
        .and_then(json)
    {
        if let Some(value) = settings.get("model").and_then(serde_json::Value::as_str) {
            values.insert("model".into(), value.into());
        }
        if let Some(value) = settings
            .get("permissions")
            .and_then(|permissions| permissions.get("defaultMode"))
            .and_then(serde_json::Value::as_str)
        {
            values.insert("permission_mode".into(), value.into());
        }
        if let Some(value) = settings
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(serde_json::Value::as_str)
        {
            values.insert("gateway".into(), value.into());
        }
        if settings.to_string().contains("session-upload claude") {
            values.insert("session_upload_hook".into(), "enabled".into());
        }
    }
    if let Some(root) = find(files, "runtime/claude/mcp.json")
        .or_else(|| find(files, ".claude.json"))
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

pub(crate) fn claude_proposed(
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
        values.insert(
            "permission_mode".into(),
            if value {
                "bypassPermissions".into()
            } else {
                value.to_string()
            },
        );
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
