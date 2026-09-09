//! Small shared helpers for the config writers: JSON⇆TOML value conversion and
//! reading an existing JSON file for merge-in-place.

use std::path::{Path, PathBuf};

use gh_common::{GhError, Harness};

/// Absolute command installed into lifecycle-hook configuration. Single-quote
/// escaping keeps paths containing spaces safe when a harness invokes hooks
/// through a shell.
pub fn session_upload_command(harness: Harness, profile: &str) -> Result<String, GhError> {
    let exe = harness_executable()?;
    let quoted = exe.to_string_lossy().replace('\'', "'\\''");
    Ok(format!(
        "'{quoted}' session-upload {} --profile '{}'",
        harness.key(),
        profile.replace('\'', "'\\''")
    ))
}

/// Absolute command installed into session-start lifecycle-hook configuration.
/// Records the mapping from the injected `BLUE_SESSION_ID` to the harness's
/// native session id so the post-exit fallback can find and upload the
/// transcript. Uses the same single-quote escaping as [`session_upload_command`]
/// and MUST NOT contain the `session-upload` substring (adapters distinguish the
/// two managed hooks by their command text).
pub fn session_start_command(harness: Harness, profile: &str) -> Result<String, GhError> {
    let exe = harness_executable()?;
    let quoted = exe.to_string_lossy().replace('\'', "'\\''");
    Ok(format!(
        "'{quoted}' session-start {} --profile '{}'",
        harness.key(),
        profile.replace('\'', "'\\''")
    ))
}

pub fn harness_executable() -> Result<std::path::PathBuf, GhError> {
    std::env::current_exe()
        .map_err(|e| GhError::other(format!("resolving harness executable for hook: {e}")))
}

/// Convert a `serde_json::Value` into a `toml::Value`, dropping `null`s (TOML
/// has no null). Used to fold `managed_config.extra` into Codex/Kimi TOML.
pub fn json_to_toml(v: &serde_json::Value) -> Option<toml::Value> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(toml::Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(toml::Value::Integer(i))
            } else {
                n.as_f64().map(toml::Value::Float)
            }
        }
        serde_json::Value::String(s) => Some(toml::Value::String(s.clone())),
        serde_json::Value::Array(a) => Some(toml::Value::Array(
            a.iter().filter_map(json_to_toml).collect(),
        )),
        serde_json::Value::Object(o) => {
            let mut table = toml::map::Map::new();
            for (k, val) in o {
                if let Some(tv) = json_to_toml(val) {
                    table.insert(k.clone(), tv);
                }
            }
            Some(toml::Value::Table(table))
        }
    }
}

/// Read an existing JSON object file for in-place merge. Returns an empty object
/// if the file is absent, and errors if it exists but isn't a JSON object.
pub fn json_pretty(value: &serde_json::Value) -> Result<Vec<u8>, GhError> {
    let mut s = serde_json::to_string_pretty(value).map_err(|e| GhError::Serde(e.to_string()))?;
    s.push('\n');
    Ok(s.into_bytes())
}

pub(crate) fn prepend_helper_paths(
    env: &mut std::collections::BTreeMap<String, String>,
    helpers: &std::collections::BTreeMap<String, PathBuf>,
) {
    let mut dirs = helpers
        .values()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    dirs.sort();
    dirs.dedup();
    if dirs.is_empty() {
        return;
    }
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    if let Ok(value) = std::env::join_paths(dirs) {
        env.insert("PATH".into(), value.to_string_lossy().into_owned());
    }
}
