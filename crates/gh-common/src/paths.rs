//! XDG-based path resolution. Every file the harness owns lives under a
//! predictable, shell-inspectable location — file-based contracts are a
//! first-class design goal.

use std::path::PathBuf;

use crate::error::GhError;

/// The user's home directory (`$HOME`, or `%USERPROFILE%` on Windows).
pub fn home_dir() -> Result<PathBuf, GhError> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| GhError::config("HOME is not set"))
}

/// `$XDG_CONFIG_HOME`, falling back to `~/.config`.
pub fn config_home() -> Result<PathBuf, GhError> {
    if let Some(v) = std::env::var_os("XDG_CONFIG_HOME") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    Ok(home_dir()?.join(".config"))
}

/// `$XDG_CACHE_HOME`, falling back to `~/.cache`.
pub fn cache_home() -> Result<PathBuf, GhError> {
    if let Some(v) = std::env::var_os("XDG_CACHE_HOME") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    Ok(home_dir()?.join(".cache"))
}

/// `$XDG_CONFIG_HOME/blue` — where `blue.toml`, the session, and the
/// shared `mcp.json` staging file live.
pub fn blue_config_dir() -> Result<PathBuf, GhError> {
    Ok(config_home()?.join("blue"))
}

/// The client config file, `$XDG_CONFIG_HOME/blue/blue.toml`.
pub fn blue_toml_path() -> Result<PathBuf, GhError> {
    Ok(blue_config_dir()?.join("blue.toml"))
}

/// The persisted login session, `$XDG_CONFIG_HOME/blue/session.json`.
pub fn session_path() -> Result<PathBuf, GhError> {
    Ok(blue_config_dir()?.join("session.json"))
}

/// The cached governance-config, `$XDG_CACHE_HOME/blue/governance-config.json`.
pub fn governance_cache_path() -> Result<PathBuf, GhError> {
    Ok(cache_home()?.join("blue").join("governance-config.json"))
}

/// Shared MCP staging file the per-harness adapters merge from.
pub fn mcp_staging_path() -> Result<PathBuf, GhError> {
    Ok(blue_config_dir()?.join("mcp.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blue_owned_paths_use_the_blue_xdg_namespaces() {
        assert_eq!(
            blue_config_dir().unwrap(),
            config_home().unwrap().join("blue")
        );
        assert_eq!(
            blue_toml_path().unwrap(),
            config_home().unwrap().join("blue/blue.toml")
        );
        assert_eq!(
            session_path().unwrap(),
            config_home().unwrap().join("blue/session.json")
        );
        assert_eq!(
            governance_cache_path().unwrap(),
            cache_home().unwrap().join("blue/governance-config.json")
        );
    }
}
