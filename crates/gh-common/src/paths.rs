//! Native client path resolution. Unix keeps the original XDG layout, while
//! Windows separates roaming configuration from machine-local state.

use std::path::PathBuf;

use crate::error::GhError;

/// Fully-resolved filesystem authority used by the Blue client.
///
/// `profile` and `shims` are optional because the profile directory is not
/// always needed: with both XDG roots supplied explicitly there is nothing left
/// for `$HOME` to answer, and a container that only ever logs in has no reason
/// to set it. They are reported through [`ClientPaths::profile`] and
/// [`ClientPaths::shims`], which re-raise the original resolution failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPaths {
    profile: Result<PathBuf, String>,
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    shims: Option<PathBuf>,
}

impl ClientPaths {
    pub fn resolve() -> Result<Self, GhError> {
        layout(
            home_dir,
            explicit_xdg("XDG_CONFIG_HOME")?,
            explicit_xdg("XDG_CACHE_HOME")?,
        )
    }

    /// The injectable seam: resolve against a caller-supplied profile instead
    /// of the ambient one.
    pub fn resolve_for_profile(profile: PathBuf) -> Result<Self, GhError> {
        layout(
            || Ok(profile.clone()),
            explicit_xdg("XDG_CONFIG_HOME")?,
            explicit_xdg("XDG_CACHE_HOME")?,
        )
    }

    /// The user's profile directory, or the failure that kept it from being
    /// resolved.
    pub fn profile(&self) -> Result<PathBuf, GhError> {
        self.profile.clone().map_err(GhError::Config)
    }

    /// The default directory for command shims.
    pub fn shims(&self) -> Result<PathBuf, GhError> {
        match &self.shims {
            Some(shims) => Ok(shims.clone()),
            None => Ok(self.profile()?.join(".local").join("bin")),
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
    pub fn legacy_windows_dir(&self) -> Result<PathBuf, GhError> {
        Ok(self.profile()?.join(".config").join("blue"))
    }
}

/// The per-platform layout, with the XDG overrides injected rather than read
/// from the ambient environment — so tests can pin them.
///
/// `profile` is a resolver rather than a value: every root it can answer for
/// may be overridden, so resolving it eagerly turns an unset `$HOME` into a
/// hard failure even when nothing asks for it.
fn layout(
    profile: impl Fn() -> Result<PathBuf, GhError>,
    config_override: Option<PathBuf>,
    cache_override: Option<PathBuf>,
) -> Result<ClientPaths, GhError> {
    #[cfg(windows)]
    {
        let roaming = match config_override {
            Some(path) => path,
            None => known_folder(KnownFolder::RoamingAppData)?,
        };
        let local = known_folder(KnownFolder::LocalAppData)?;
        let cache_home = cache_override.unwrap_or_else(|| local.clone());
        Ok(ClientPaths {
            // Only `legacy_windows_dir` reads the profile here; the Known
            // Folders carry everything else.
            profile: profile().map_err(unresolved_profile),
            config: roaming.join("Blue"),
            data: local.join("Blue").join("Data"),
            cache: cache_home.join("Blue").join("Cache"),
            shims: Some(local.join("Blue").join("bin")),
        })
    }
    #[cfg(not(windows))]
    {
        let config_home = match config_override {
            Some(path) => path,
            None => profile()?.join(".config"),
        };
        let cache_home = match cache_override {
            Some(path) => path,
            None => profile()?.join(".cache"),
        };
        let profile = profile().map_err(unresolved_profile);
        let config = config_home.join("blue");
        Ok(ClientPaths {
            shims: profile
                .as_ref()
                .ok()
                .map(|profile| profile.join(".local").join("bin")),
            profile,
            data: config.clone(),
            config,
            cache: cache_home.join("blue"),
        })
    }
}

/// Carry the reason the profile could not be resolved, so the paths that do
/// need it fail with the original diagnosis rather than a generic one.
fn unresolved_profile(error: GhError) -> String {
    match error {
        GhError::Config(message) => message,
        other => other.to_string(),
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
        known_folder(KnownFolder::Profile)
    }
    #[cfg(not(windows))]
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| GhError::config("HOME is not set"))
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
    ClientPaths::resolve()?.shims()
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

/// The wide, NUL-terminated form of `path` for a raw Win32 `W` call.
///
/// Calling those APIs directly skips the `\\?\` promotion `std::fs` performs
/// on our behalf, which silently caps them at `MAX_PATH` — 248 for
/// `CreateDirectoryW`, since the directory has to leave room for an 8.3 child.
/// A managed tree crosses that on an ordinary profile once a marketplace
/// plugin nests a few levels down, so promote here instead.
///
/// The prefix suppresses normalisation, so the path has to be absolute and
/// separator-clean before it is applied: `absolute` resolves `.`, `..` and `/`
/// through `GetFullPathNameW`, and returns an already-verbatim path untouched.
/// Device paths (`\\.\`) are passed through — they are not ours to rewrite.
#[cfg(windows)]
pub(crate) fn wide_path(path: &std::path::Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    let absolute = std::path::absolute(path)?;
    let promotion = match absolute.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            // `C:\dir` -> `\\?\C:\dir`
            Prefix::Disk(_) => Some((r"\\?\", 0)),
            // `\\server\share` -> `\\?\UNC\server\share`
            Prefix::UNC(..) => Some((r"\\?\UNC", 1)),
            // Already verbatim, or a device namespace we must not rewrite.
            _ => None,
        },
        _ => None,
    };
    let wide = absolute.as_os_str().encode_wide().collect::<Vec<u16>>();
    let promoted = match promotion {
        Some((prefix, skip)) => prefix
            .encode_utf16()
            .chain(wide.into_iter().skip(skip))
            .collect(),
        None => wide,
    };
    Ok(promoted.into_iter().chain(Some(0)).collect())
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

    /// Every variable [`home_dir`](super::home_dir) and
    /// [`ClientPaths::resolve`](super::ClientPaths::resolve) read.
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
    use std::path::Path;

    use super::*;

    /// Resolved against an injected profile rather than the ambient one: the
    /// runner may have no `$HOME` at all.
    #[test]
    fn blue_owned_paths_are_namespaced_under_the_resolved_layout() {
        let paths = ClientPaths::resolve_for_profile(PathBuf::from("/tmp/blue profile")).unwrap();
        assert_eq!(paths.blue_toml(), paths.config.join("blue.toml"));
        assert_eq!(paths.session(), paths.config.join("session.json"));
        assert_eq!(
            paths.governance_cache(),
            paths.cache.join("governance-config.json")
        );

        // The namespace itself is platform-specific: `<xdg>/blue` on Unix,
        // `%APPDATA%\Blue` + `%LOCALAPPDATA%\Blue\Cache` on Windows.
        #[cfg(windows)]
        {
            assert_eq!(paths.config.file_name().unwrap(), "Blue");
            assert_eq!(paths.cache.file_name().unwrap(), "Cache");
            assert_eq!(paths.cache.parent().unwrap().file_name().unwrap(), "Blue");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(paths.config.file_name().unwrap(), "blue");
            assert_eq!(paths.cache.file_name().unwrap(), "blue");
        }
    }

    /// `blue login` in a container that sets both XDG roots and no `$HOME`:
    /// the profile answers for nothing the layout needs, so demanding it up
    /// front only broke the roots that were fully specified.
    #[cfg(not(windows))]
    #[test]
    fn unix_layout_resolves_without_a_profile_when_both_roots_are_explicit() {
        let paths = layout(
            || Err(GhError::config("HOME is not set")),
            Some(PathBuf::from("/xdg/config")),
            Some(PathBuf::from("/xdg/cache")),
        )
        .unwrap();
        assert_eq!(paths.config, Path::new("/xdg/config/blue"));
        assert_eq!(paths.data, paths.config);
        assert_eq!(paths.cache, Path::new("/xdg/cache/blue"));
        // Only the paths that genuinely need a profile still fail.
        assert!(paths.profile().is_err());
        assert!(paths.shims().is_err());
    }

    /// End to end through the ambient environment: `blue login` and friends
    /// resolve with no `$HOME` at all. Both roots have to be pinned for the
    /// whole process, so the check runs in a child.
    #[cfg(not(windows))]
    #[test]
    fn client_paths_resolve_in_a_homeless_container() {
        let root = std::env::temp_dir().join(format!("blue-homeless-{}", std::process::id()));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "paths::tests::homeless_xdg_child_helper"])
            .env_remove("HOME")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("BLUE_HOMELESS_TEST_ROOT", &root)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Runs inside the child spawned by
    /// `client_paths_resolve_in_a_homeless_container`.
    #[cfg(not(windows))]
    #[test]
    fn homeless_xdg_child_helper() {
        let Some(root) = std::env::var_os("BLUE_HOMELESS_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        assert!(home_dir().is_err(), "the child should have no profile");
        assert_eq!(blue_config_dir().unwrap(), root.join("config/blue"));
        assert_eq!(blue_data_dir().unwrap(), root.join("config/blue"));
        assert_eq!(
            blue_toml_path().unwrap(),
            root.join("config/blue/blue.toml")
        );
        assert_eq!(
            session_path().unwrap(),
            root.join("config/blue/session.json")
        );
        assert_eq!(
            governance_cache_path().unwrap(),
            root.join("cache/blue/governance-config.json")
        );
        // Shims are the one root with no override, so they still need `$HOME`.
        assert!(shim_dir().is_err());
    }

    /// The layout still fails when a root it cannot override is missing.
    #[cfg(not(windows))]
    #[test]
    fn unix_layout_still_requires_a_profile_for_an_unset_root() {
        assert!(layout(
            || Err(GhError::config("HOME is not set")),
            Some(PathBuf::from("/xdg/config")),
            None,
        )
        .is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_layout_remains_compatible() {
        let profile = PathBuf::from("/tmp/blue profile");
        let paths = layout(|| Ok(profile.clone()), None, None).unwrap();
        assert_eq!(paths.config, profile.join(".config/blue"));
        assert_eq!(paths.data, paths.config);
        assert_eq!(paths.cache, profile.join(".cache/blue"));
        assert_eq!(paths.shims().unwrap(), profile.join(".local/bin"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_layout_honours_explicit_xdg_overrides() {
        let profile = PathBuf::from("/tmp/blue profile");
        let paths = layout(
            || Ok(profile.clone()),
            Some(PathBuf::from("/xdg/config")),
            Some(PathBuf::from("/xdg/cache")),
        )
        .unwrap();
        assert_eq!(paths.config, Path::new("/xdg/config/blue"));
        assert_eq!(paths.data, paths.config);
        assert_eq!(paths.cache, Path::new("/xdg/cache/blue"));
        assert_eq!(paths.shims().unwrap(), profile.join(".local/bin"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_layout_separates_roaming_config_from_local_state() {
        let profile = PathBuf::from(r"C:\Users\blue");
        let paths = layout(
            || Ok(profile.clone()),
            Some(PathBuf::from(r"R:\Roaming")),
            Some(PathBuf::from(r"C:\Cache")),
        )
        .unwrap();
        assert_eq!(paths.profile().unwrap(), profile);
        assert_eq!(paths.config, Path::new(r"R:\Roaming\Blue"));
        assert_eq!(paths.cache, Path::new(r"C:\Cache\Blue\Cache"));
        assert_ne!(paths.data, paths.config);
    }
}
