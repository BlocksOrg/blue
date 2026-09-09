//! `blue.toml` — the **client** config. This is deliberately tiny: it only
//! selects the service URL and how to authenticate to it. Everything else
//! (which harnesses are allowed, managed config, gateway routing) is governed
//! by the *service*, never the client.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::GhError;
use crate::paths;

/// Root of `blue.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlueToml {
    #[serde(default)]
    pub service: ServiceConfig,
    #[serde(default)]
    pub identity: IdentityConfig,
    /// Client-side operating-mode preferences. The client can only ever
    /// *downgrade* to governance-only for local dev — it can never enable
    /// gateway mode, which is declared by the service.
    #[serde(default)]
    pub mode: ModeConfig,
    /// Local presentation preferences. These never affect governance policy.
    #[serde(default)]
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiConfig {
    /// Agent selected by the user for bare `blue` launches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_harness: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// The single service URL the operator provisioned. Empty ⇒ use a `file`
    /// config source (local dev) instead of `http`.
    #[serde(default)]
    pub url: String,
    /// Local `blue.yaml` file for `mode = "file"` development/testing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_file: Option<String>,
}

/// How the client authenticates to the service. `token` is the simplest
/// self-host; `oidc` is the SSO seam (device-authorization grant).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum IdentityConfig {
    Token {
        /// Secret reference (`env://`, `file://`) or inline token for dev.
        #[serde(default)]
        token: String,
    },
    Oidc {
        issuer: String,
        client_id: String,
        #[serde(default)]
        scopes: Vec<String>,
    },
}

impl Default for IdentityConfig {
    fn default() -> Self {
        IdentityConfig::Token {
            token: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModeConfig {
    /// Force governance-only even if the service would offer gateway mode.
    /// Intended for local dev; cannot enable gateway mode (server decides).
    #[serde(default)]
    pub force_governance_only: bool,
    /// Enforced deployment: also write Claude's un-overridable
    /// `managed-settings.json` (the MDM/managed path). Best-effort without an
    /// OS-level managed profile (plan risk #11).
    #[serde(default)]
    pub enforced: bool,
    /// Permit daemon/wrapper reconciliation to merge into existing harness
    /// configuration without an interactive preflight.
    #[serde(default)]
    pub allow_noninteractive_merge: bool,
}

impl BlueToml {
    /// Load from the default XDG path. A missing file yields defaults — the
    /// client is usable with zero config against a `file` source.
    pub fn load() -> Result<Self, GhError> {
        let path = paths::blue_toml_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, GhError> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s)
                .map_err(|e| GhError::config(format!("parsing {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BlueToml::default()),
            Err(e) => Err(GhError::Io {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    /// Save client configuration atomically. OAuth tokens are stored separately.
    pub fn save(&self) -> Result<(), GhError> {
        self.save_to(&paths::blue_toml_path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), GhError> {
        let body =
            toml::to_string_pretty(self).map_err(|error| GhError::Serde(error.to_string()))?;
        crate::write_atomic(path, body)
    }

    /// Whether the client is configured to reach a real service over HTTP.
    pub fn has_http_service(&self) -> bool {
        !self.service.url.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_token_identity() {
        let cfg: BlueToml = toml::from_str(
            r#"
            [service]
            url = "https://harness.acme.com"

            [identity]
            mode = "token"
            token = "env://HARNESS_TOKEN"
            "#,
        )
        .unwrap();
        assert!(cfg.has_http_service());
        assert!(matches!(cfg.identity, IdentityConfig::Token { .. }));
    }

    #[test]
    fn parses_oidc_identity() {
        let cfg: BlueToml = toml::from_str(
            r#"
            [identity]
            mode = "oidc"
            issuer = "https://sso.acme.com"
            client_id = "harness"
            scopes = ["openid", "email"]
            "#,
        )
        .unwrap();
        match cfg.identity {
            IdentityConfig::Oidc { issuer, .. } => assert_eq!(issuer, "https://sso.acme.com"),
            _ => panic!("expected oidc"),
        }
    }

    #[test]
    fn missing_file_is_default() {
        let cfg = BlueToml::load_from(Path::new("/nonexistent/blue.toml")).unwrap();
        assert!(!cfg.has_http_service());
    }

    #[test]
    fn ui_preference_round_trips() {
        let mut cfg = BlueToml::default();
        cfg.ui.preferred_harness = Some("codex".into());
        let encoded = toml::to_string(&cfg).unwrap();
        let decoded: BlueToml = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.ui.preferred_harness.as_deref(), Some("codex"));
    }
}
