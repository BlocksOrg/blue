//! Native client path resolution. Unix keeps the original XDG layout, while
//! Windows separates roaming configuration from machine-local state.

use std::path::{Path, PathBuf};

use crate::error::GhError;

/// Fully-resolved filesystem authority used by the Blue client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPaths {
    pub profile: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub shims: PathBuf,
}

impl ClientPaths {
    pub fn resolve() -> Result<Self, GhError> {
        Self::resolve_for_profile(home_dir()?)
    }

    pub fn resolve_for_profile(profile: PathBuf) -> Result<Self, GhError> {
        #[cfg(windows)]
        {
            let roaming = explicit_xdg("XDG_CONFIG_HOME")?
                .unwrap_or(known_folder(KnownFolder::RoamingAppData)?);
            let local = known_folder(KnownFolder::LocalAppData)?;
            let cache_home = explicit_xdg("XDG_CACHE_HOME")?.unwrap_or_else(|| local.clone());
            return Ok(Self {
                profile,
                config: roaming.join("Blue"),
                data: local.join("Blue").join("Data"),
                cache: cache_home.join("Blue").join("Cache"),
                shims: local.join("Blue").join("bin"),
            });
        }
        #[cfg(not(windows))]
        {
            let config_home =
                explicit_xdg("XDG_CONFIG_HOME")?.unwrap_or_else(|| profile.join(".config"));
            let cache_home =
                explicit_xdg("XDG_CACHE_HOME")?.unwrap_or_else(|| profile.join(".cache"));
            let config = config_home.join("blue");
            Ok(Self {
                profile: profile.clone(),
                data: config.clone(),
                config,
                cache: cache_home.join("blue"),
                shims: profile.join(".local").join("bin"),
            })
        }
    }

    pub fn blue_toml(&self) -> PathBuf {
        self.config.join("blue.toml")
    }
    pub fn blue_yaml(&self) -> PathBuf {
        self.config.join("blue.yaml")
    }
    pub fn session(&self) -> PathBuf {
        self.config.join("session.json")
    }
    pub fn identities(&self) -> PathBuf {
        self.config.join("identities")
    }
    pub fn governance_cache(&self) -> PathBuf {
        self.cache.join("governance-config.json")
    }
    pub fn runtime(&self) -> PathBuf {
        self.data.join("runtime")
    }
    pub fn legacy_windows_dir(&self) -> PathBuf {
        self.profile.join(".config").join("blue")
    }
}

fn explicit_xdg(name: &str) -> Result<Option<PathBuf>, GhError> {
    Ok(std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from))
}

/// The user's profile directory (`$HOME`, or `%USERPROFILE%` on Windows).
pub fn home_dir() -> Result<PathBuf, GhError> {
    #[cfg(windows)]
    {
        return known_folder(KnownFolder::Profile);
    }
    #[cfg(not(windows))]
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| GhError::config("HOME is not set"))
}

/// The platform config home (the parent of Blue's config directory).
pub fn config_home() -> Result<PathBuf, GhError> {
    let paths = ClientPaths::resolve()?;
    Ok(paths.config.parent().unwrap_or(Path::new("")).to_path_buf())
}

/// The platform cache home (the parent of Blue's cache directory).
pub fn cache_home() -> Result<PathBuf, GhError> {
    let paths = ClientPaths::resolve()?;
    Ok(paths.cache.parent().unwrap_or(Path::new("")).to_path_buf())
}

/// `$XDG_CONFIG_HOME/blue` — where `blue.toml`, the session, and the
/// shared `mcp.json` staging file live.
pub fn blue_config_dir() -> Result<PathBuf, GhError> {
    Ok(ClientPaths::resolve()?.config)
}

/// Machine-local mutable Blue state.
pub fn blue_data_dir() -> Result<PathBuf, GhError> {
    Ok(ClientPaths::resolve()?.data)
}

/// Default directory for command shims.
pub fn shim_dir() -> Result<PathBuf, GhError> {
    Ok(ClientPaths::resolve()?.shims)
}

/// The client config file, `$XDG_CONFIG_HOME/blue/blue.toml`.
pub fn blue_toml_path() -> Result<PathBuf, GhError> {
    Ok(ClientPaths::resolve()?.blue_toml())
}

/// The persisted login session, `$XDG_CONFIG_HOME/blue/session.json`.
pub fn session_path() -> Result<PathBuf, GhError> {
    Ok(ClientPaths::resolve()?.session())
}

/// The cached governance-config, `$XDG_CACHE_HOME/blue/governance-config.json`.
pub fn governance_cache_path() -> Result<PathBuf, GhError> {
    Ok(ClientPaths::resolve()?.governance_cache())
}

#[cfg(windows)]
enum KnownFolder {
    Profile,
    RoamingAppData,
    LocalAppData,
}

#[cfg(windows)]
fn known_folder(folder: KnownFolder) -> Result<PathBuf, GhError> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData, SHGetKnownFolderPath,
        KF_FLAG_DONT_VERIFY,
    };

    let id = match folder {
        KnownFolder::Profile => &FOLDERID_Profile,
        KnownFolder::RoamingAppData => &FOLDERID_RoamingAppData,
        KnownFolder::LocalAppData => &FOLDERID_LocalAppData,
    };
    let mut raw = std::ptr::null_mut();
    let status = unsafe {
        SHGetKnownFolderPath(
            id,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    if status < 0 || raw.is_null() {
        return Err(GhError::config(format!(
            "Windows Known Folder lookup failed (HRESULT 0x{:08x})",
            status as u32
        )));
    }
    let len = unsafe {
        let mut len = 0usize;
        while *raw.add(len) != 0 {
            len += 1;
        }
        len
    };
    let value = std::ffi::OsString::from_wide(unsafe { std::slice::from_raw_parts(raw, len) });
    unsafe { CoTaskMemFree(raw.cast()) };
    Ok(PathBuf::from(value))
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

    #[cfg(not(windows))]
    #[test]
    fn unix_layout_remains_compatible() {
        let profile = PathBuf::from("/tmp/blue profile");
        let paths = ClientPaths::resolve_for_profile(profile.clone()).unwrap();
        assert_eq!(paths.config, profile.join(".config/blue"));
        assert_eq!(paths.data, paths.config);
        assert_eq!(paths.cache, profile.join(".cache/blue"));
        assert_eq!(paths.shims, profile.join(".local/bin"));
    }
}
