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

/// Point every path this module resolves at one throwaway directory.
///
/// `HOME`, `XDG_CONFIG_HOME` and `XDG_CACHE_HOME` are process-global, so a test
/// that touches `session.json` or the governance cache without this guard
/// writes to the developer's real `~/.config/blue`. One mutex covers all three
/// variables on purpose: two independent locks guarding overlapping globals
/// deadlock the moment a test needs both.
///
/// Behind the `test-support` feature rather than `#[cfg(test)]` because
/// `#[cfg(test)]` items are not visible to other crates.
#[cfg(feature = "test-support")]
pub mod test_support {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Every variable [`home_dir`](super::home_dir), [`config_home`](super::config_home)
    /// and [`cache_home`](super::cache_home) read.
    const VARS: [&str; 4] = ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_CACHE_HOME"];

    pub struct XdgHomeGuard {
        _lock: MutexGuard<'static, ()>,
        previous: Vec<(&'static str, Option<OsString>)>,
        dir: PathBuf,
    }

    impl XdgHomeGuard {
        /// The tempdir standing in for `$HOME`.
        pub fn home(&self) -> &std::path::Path {
            &self.dir
        }
    }

    /// Redirect `HOME` and the XDG roots at a fresh directory named after
    /// `name`, restoring the previous values when the guard drops.
    pub fn with_xdg_home(name: &str) -> XdgHomeGuard {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("blue-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        std::fs::create_dir_all(dir.join(".cache")).unwrap();
        let previous = VARS
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        std::env::set_var("HOME", &dir);
        std::env::set_var("USERPROFILE", &dir);
        std::env::set_var("XDG_CONFIG_HOME", dir.join(".config"));
        std::env::set_var("XDG_CACHE_HOME", dir.join(".cache"));
        XdgHomeGuard {
            _lock: lock,
            previous,
            dir,
        }
    }

    impl Drop for XdgHomeGuard {
        fn drop(&mut self) {
            for (name, value) in self.previous.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
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
