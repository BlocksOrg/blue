//! Atomic on-disk cache of the last governance-config, with a fetch timestamp
//! so `blue run` can fail-soft on a transient service outage (unless the
//! config is marked `required`).

use serde::{Deserialize, Serialize};

use gh_common::{paths, write_atomic, GhError};

use crate::schema::GovernanceConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedConfig {
    /// Unix seconds the config was fetched.
    pub fetched_at: i64,
    pub config: GovernanceConfig,
}

impl CachedConfig {
    /// Whether the cache is still within its TTL relative to `now` (unix secs).
    pub fn is_fresh(&self, now: i64) -> bool {
        let age = now.saturating_sub(self.fetched_at);
        age >= 0 && (age as u64) < self.config.ttl_seconds()
    }
}

/// Drop the runtime-only gateway credential from a config on its way to or
/// from disk. The inference JWT is session-bound and short-lived; persisting
/// it means a `blue run` after the session dies reinstalls a token the proxy
/// will reject, which is exactly the failure this cache is supposed to avoid.
/// Applied on load as well as save so caches already written by an earlier
/// build are neutralized rather than trusted.
fn strip_runtime_gateway_credentials(config: &mut GovernanceConfig) {
    if let Some(gateway) = config.gateway.as_mut() {
        gateway.proxy_url = None;
        gateway.token = None;
    }
}

/// Persist the freshly-fetched config to the XDG cache path.
pub fn save(config: &GovernanceConfig, fetched_at: i64) -> Result<(), GhError> {
    let mut config = config.clone();
    strip_runtime_gateway_credentials(&mut config);
    let cached = CachedConfig { fetched_at, config };
    let path = paths::governance_cache_path()?;
    let body = serde_json::to_vec_pretty(&cached).map_err(|e| GhError::Serde(e.to_string()))?;
    write_atomic(&path, body)?;
    match std::fs::remove_file(path.with_extension("invalidated")) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GhError::Io { path, source }),
    }
}

/// Load the cached config, if present.
pub fn load() -> Result<Option<CachedConfig>, GhError> {
    let path = paths::governance_cache_path()?;
    // A deletion failure (e.g. a read-only cache on Windows) must not make an
    // old policy usable again. Only a successful live save clears this marker.
    if path.with_extension("invalidated").try_exists()? {
        return Ok(None);
    }
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<CachedConfig>(&bytes)
            .map(|mut cached| {
                strip_runtime_gateway_credentials(&mut cached.config);
                Some(cached)
            })
            .map_err(|e| GhError::Serde(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(GhError::Io { path, source: e }),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    /// Thin wrapper over the shared XDG seam. `XDG_CACHE_HOME` is only one of
    /// the roots these paths resolve through, and redirecting it alone leaves
    /// `session.json` pointing at the developer's real home directory.
    pub(crate) use gh_common::paths::test_support::with_xdg_home as with_cache_home;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::GatewayConfig;

    fn gateway_config() -> GovernanceConfig {
        let mut config: GovernanceConfig =
            serde_json::from_str(r#"{"revision":"r1","allowed_harnesses":["codex"]}"#).unwrap();
        config.gateway = Some(GatewayConfig {
            kind: "litellm".into(),
            proxy_url: Some("https://proxy.example/inference/".into()),
            token: Some("header.payload.signature".into()),
            auth_style: "bearer".into(),
        });
        config
    }

    #[test]
    fn save_never_writes_the_inference_token_to_disk() {
        let _guard = test_support::with_cache_home("save-strips");
        save(&gateway_config(), 100).unwrap();

        // Assert on the bytes, not the round-tripped struct: a struct-level
        // assertion would still pass if the field were written and skipped
        // only on the way back in.
        let raw = std::fs::read_to_string(paths::governance_cache_path().unwrap()).unwrap();
        assert!(!raw.contains("header.payload.signature"), "{raw}");
        assert!(!raw.contains("proxy.example"), "{raw}");
        assert!(raw.contains("litellm"), "{raw}");

        let loaded = load().unwrap().unwrap();
        let gateway = loaded.config.gateway.unwrap();
        assert_eq!(gateway.token, None);
        assert_eq!(gateway.proxy_url, None);
    }

    #[test]
    fn load_neutralizes_a_cache_that_already_holds_a_token() {
        let _guard = test_support::with_cache_home("load-strips");
        let path = paths::governance_cache_path().unwrap();
        let poisoned = CachedConfig {
            fetched_at: 100,
            config: gateway_config(),
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&poisoned).unwrap()).unwrap();

        let gateway = load().unwrap().unwrap().config.gateway.unwrap();
        assert_eq!(gateway.token, None);
        assert_eq!(gateway.proxy_url, None);
    }
}

/// Remove a policy after a definitive live incompatibility.
pub fn invalidate() -> Result<(), GhError> {
    let path = paths::governance_cache_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => {
            tracing::warn!(%source, path = %path.display(), "could not delete rejected cache; marking it unusable");
            write_atomic(
                &path.with_extension("invalidated"),
                b"live governance rejected this client",
            )
        }
    }
}

#[cfg(test)]
mod invalidation_tests {
    use super::*;
    #[test]
    fn failed_cache_deletion_cannot_resurrect_policy() {
        let _guard = test_support::with_cache_home("invalidation-marker");
        let path = paths::governance_cache_path().unwrap();
        // A directory at the file path deterministically refuses remove_file
        // even for elevated test runners on Unix and Windows.
        std::fs::create_dir_all(&path).unwrap();
        invalidate().unwrap();
        assert!(load().unwrap().is_none());
        std::fs::remove_dir(&path).unwrap();
        let config: GovernanceConfig = serde_json::from_str(r#"{"revision":"fresh"}"#).unwrap();
        save(&config, 1).unwrap();
        assert_eq!(load().unwrap().unwrap().config.revision, "fresh");
        assert!(!path.with_extension("invalidated").exists());
    }
}
