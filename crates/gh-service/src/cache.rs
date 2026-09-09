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

/// Persist the freshly-fetched config to the XDG cache path.
pub fn save(config: &GovernanceConfig, fetched_at: i64) -> Result<(), GhError> {
    let cached = CachedConfig {
        fetched_at,
        config: config.clone(),
    };
    let path = paths::governance_cache_path()?;
    let body = serde_json::to_vec_pretty(&cached).map_err(|e| GhError::Serde(e.to_string()))?;
    write_atomic(&path, body)
}

/// Load the cached config, if present.
pub fn load() -> Result<Option<CachedConfig>, GhError> {
    let path = paths::governance_cache_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| GhError::Serde(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(GhError::Io { path, source: e }),
    }
}
