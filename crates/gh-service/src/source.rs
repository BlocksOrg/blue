//! `ConfigSource` — where governance config comes from. Default is `http` (the
//! provisioned service); `file` backs local dev/testing and CI. No vendor URLs
//! are hardcoded anywhere.

use std::path::PathBuf;

use gh_common::GhError;

use crate::identity::Session;
use crate::schema::GovernanceConfig;

/// A source of governance config. Implementations are cheap to construct and
/// stateless beyond their endpoint/path.
pub trait ConfigSource: Send + Sync {
    fn fetch(&self, session: &Session) -> Result<GovernanceConfig, GhError>;
    /// Human-readable description for `blue doctor`.
    fn describe(&self) -> String;
}

/// Fetches from `GET {base_url}/governance-config` with a bearer token.
pub struct HttpConfigSource {
    base_url: String,
    client: reqwest::blocking::Client,
}

impl HttpConfigSource {
    pub fn new(base_url: impl Into<String>) -> Result<Self, GhError> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("blue/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| GhError::service(format!("building http client: {e}")))?;
        Ok(HttpConfigSource {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/governance-config", self.base_url)
    }
}

impl ConfigSource for HttpConfigSource {
    fn fetch(&self, session: &Session) -> Result<GovernanceConfig, GhError> {
        let resp = self
            .client
            .get(self.endpoint())
            .header(
                "x-blue-contract-version",
                GovernanceConfig::CONTRACT_VERSION,
            )
            .header(
                "x-blue-capabilities",
                GovernanceConfig::CAPABILITIES.join(","),
            )
            .bearer_auth(&session.token)
            .send()
            .map_err(|e| GhError::service(format!("GET {}: {e}", self.endpoint())))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(GhError::service(
                "service rejected the session token (run `blue login`)".to_string(),
            ));
        }
        if status == reqwest::StatusCode::UPGRADE_REQUIRED {
            let body = resp.text().unwrap_or_default();
            return Err(GhError::config(format!(
                "control service rejected this client version: {body}"
            )));
        }
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(GhError::service(format!(
                "service returned {status} for governance-config: {body}"
            )));
        }
        let config = resp
            .json::<GovernanceConfig>()
            .map_err(|e| GhError::service(format!("decoding governance-config: {e}")))?;
        config.ensure_client_compatible().map_err(GhError::config)?;
        Ok(config)
    }

    fn describe(&self) -> String {
        format!("http({})", self.endpoint())
    }
}

/// Reads governance config from a local JSON or YAML file. For local dev and
/// the minimal self-host (Control API over a static file uses the same shape).
pub struct FileConfigSource {
    path: PathBuf,
}

impl FileConfigSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileConfigSource { path: path.into() }
    }
}

impl ConfigSource for FileConfigSource {
    fn fetch(&self, _session: &Session) -> Result<GovernanceConfig, GhError> {
        let text = std::fs::read_to_string(&self.path).map_err(|e| GhError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        let config = parse_config(&self.path, &text)?;
        config.ensure_client_compatible().map_err(GhError::config)?;
        Ok(config)
    }

    fn describe(&self) -> String {
        format!("file({})", self.path.display())
    }
}

/// Parse governance config from text, choosing JSON vs YAML by extension
/// (defaulting to YAML, which is a JSON superset).
pub fn parse_config(path: &std::path::Path, text: &str) -> Result<GovernanceConfig, GhError> {
    let is_json = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if is_json {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| GhError::Serde(e.to_string()))?;
        serde_json::from_value(value.get("governance").cloned().unwrap_or(value))
            .map_err(|e| GhError::Serde(e.to_string()))
    } else {
        let value: serde_yaml::Value =
            serde_yaml::from_str(text).map_err(|e| GhError::Serde(e.to_string()))?;
        let governance = value.get("governance").cloned().unwrap_or(value);
        serde_yaml::from_value(governance).map_err(|e| GhError::Serde(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yaml_config() {
        let yaml = r#"
revision: r1
allowed_harnesses: [codex, claude]
harnesses:
  codex:
    managed_config:
      model: gpt-5
"#;
        let cfg = parse_config(std::path::Path::new("cfg.yaml"), yaml).unwrap();
        assert!(cfg.is_allowed("codex"));
        assert_eq!(
            cfg.policy("codex").unwrap().managed_config.model.as_deref(),
            Some("gpt-5")
        );
    }

    #[test]
    fn parses_governance_from_unified_blue_config() {
        let yaml = r#"
control_api:
  listen: 127.0.0.1:8080
governance:
  revision: r1
  allowed_harnesses: [codex]
"#;
        let cfg = parse_config(std::path::Path::new("blue.yaml"), yaml).unwrap();
        assert!(cfg.is_allowed("codex"));
    }
}
