//! Shared OpenCode-family inspection.
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

pub(crate) fn opencode_current(_: &HarnessPolicy, files: &[PathBuf]) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    if let Some(config) = find(files, "runtime/opencode/opencode.json")
        .or_else(|| find(files, ".config/opencode/opencode.json"))
        .and_then(json)
    {
        if let Some(value) = config.get("model").and_then(serde_json::Value::as_str) {
            values.insert("model".into(), value.into());
        }
        values.insert(
            "mcp".into(),
            names(config.get("mcp").and_then(serde_json::Value::as_object)),
        );
        if let Some(value) = config
            .get("provider")
            .and_then(|provider| provider.get("governed"))
            .and_then(|governed| governed.get("options"))
            .and_then(|options| options.get("baseURL"))
            .and_then(serde_json::Value::as_str)
        {
            values.insert("gateway".into(), value.into());
        }
    }
    if files.iter().any(|path| {
        path.ends_with("blue-session-upload.js")
            && std::fs::read_to_string(path)
                .is_ok_and(|body| body.contains("session-upload opencode"))
    }) {
        values.insert("session_upload_hook".into(), "enabled".into());
    }
    values
}

pub(crate) fn opencode_proposed(
    mut values: BTreeMap<String, String>,
    policy: &HarnessPolicy,
    gateway: Option<&GatewayConfig>,
    opts: WriteOptions,
) -> BTreeMap<String, String> {
    if let Some(model) = &policy.managed_config.model {
        values.insert(
            "model".into(),
            if opts.gateway_enabled && gateway.is_some() {
                format!("governed/{model}")
            } else {
                model.clone()
            },
        );
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
