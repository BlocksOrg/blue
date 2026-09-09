//! Shared Codex-family inspection.
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

pub(crate) fn codex_current(policy: &HarnessPolicy, files: &[PathBuf]) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    // Once the governed overlay exists it is the effective managed state.
    // Prefer it to the user's native config so an unchanged semantic merge is
    // not presented for approval again on every governed launch.
    if let Some(config) = find(files, ".codex/blue.config.toml")
        .or_else(|| find(files, ".codex/config.toml"))
        .and_then(toml)
    {
        for key in ["model", "approval_policy", "sandbox_mode", "model_provider"] {
            if let Some(value) = config.get(key).and_then(toml::Value::as_str) {
                values.insert(key.into(), value.into());
            }
        }
        if let Some(value) = config
            .get("model_reasoning_effort")
            .and_then(toml::Value::as_str)
        {
            values.insert("reasoning_effort".into(), value.into());
        }
        if let Some(value) = config
            .get("features")
            .and_then(toml::Value::as_table)
            .and_then(|features| features.get("fast_mode"))
            .and_then(toml::Value::as_bool)
        {
            values.insert("fast_mode".into(), value.to_string());
        }
        if let Some(value) = config.get("service_tier").and_then(toml::Value::as_str) {
            values.insert("service_tier".into(), value.into());
        }
        if let Some(value) = config
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get("governed"))
            .and_then(toml::Value::as_table)
            .and_then(|provider| provider.get("base_url"))
            .and_then(toml::Value::as_str)
        {
            values.insert("gateway".into(), value.into());
        }
        for key in policy.managed_config.extra.keys() {
            if let Some(value) = config
                .get(key)
                .and_then(|value| serde_json::to_value(value).ok())
            {
                values.insert(
                    format!("managed.{key}"),
                    serde_json::to_string(&value).unwrap_or_default(),
                );
            }
        }
        let mcp = config
            .get("mcp_servers")
            .and_then(toml::Value::as_table)
            .map(|table| table.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        values.insert(
            "mcp".into(),
            if mcp.is_empty() { "none".into() } else { mcp },
        );
        if format!("{config:?}").contains("session-upload codex") {
            values.insert("session_upload_hook".into(), "enabled".into());
        }
    }
    values
}

pub(crate) fn codex_proposed(
    mut values: BTreeMap<String, String>,
    policy: &HarnessPolicy,
    gateway: Option<&GatewayConfig>,
    opts: WriteOptions,
) -> BTreeMap<String, String> {
    if let Some(model) = &policy.managed_config.model {
        values.insert("model".into(), model.clone());
    }
    values.insert(
        "approval_policy".into(),
        super::writer::approval_policy(policy).into(),
    );
    for (key, value) in &policy.managed_config.extra {
        values.insert(
            format!("managed.{key}"),
            serde_json::to_string(value).unwrap_or_default(),
        );
    }
    if let Some(value) = &policy.managed_config.reasoning_effort {
        values.insert("reasoning_effort".into(), value.clone());
    }
    if let Some(value) = policy.managed_config.fast_mode {
        values.insert("fast_mode".into(), value.to_string());
        if !value {
            values.insert("service_tier".into(), "default".into());
        }
    }
    if let Some(value) = &policy.managed_config.sandbox_mode {
        values.insert("sandbox_mode".into(), value.clone());
    }
    if opts.gateway_enabled {
        if let Some(proxy) = gateway.and_then(|gateway| gateway.proxy_url.as_ref()) {
            values.insert("gateway".into(), proxy.clone());
        }
        if gateway.is_some() {
            values.insert("model_provider".into(), "governed".into());
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
