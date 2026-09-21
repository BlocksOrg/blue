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

/// Longest server-supplied detail worth relaying. A load balancer in front of
/// the Control API can answer a 401 with a full HTML error page.
const MAX_DETAIL_CHARS: usize = 200;

/// Normalize terminal whitespace, remove control characters, reject blank,
/// and cap a server-supplied string before it reaches the user's terminal.
pub fn bounded_detail(raw: &str) -> Option<String> {
    let mut detail = String::new();
    for character in raw.trim().chars() {
        let character = match character {
            '\n' | '\r' | '\t' => ' ',
            character if character.is_control() => continue,
            character => character,
        };
        if detail.chars().count() == MAX_DETAIL_CHARS {
            break;
        }
        detail.push(character);
    }
    let detail = detail.trim();
    if detail.is_empty() {
        return None;
    }
    Some(detail.to_owned())
}

/// Pull the `error` string out of the Control API's `{"error": …}` body.
/// `None` for anything else, including the HTML an intermediary might return.
pub fn server_error_detail(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    bounded_detail(value.get("error")?.as_str()?)
}

/// The message shown when the control service rejects the current session.
/// `detail`, when the server sent one, is the only part that says *why*.
pub fn session_rejected_message(forbidden: bool, detail: Option<&str>) -> String {
    let headline = if forbidden {
        "the control service refused this session"
    } else {
        "your session is no longer valid"
    };
    match detail {
        Some(detail) if forbidden => format!("{headline}: {detail}"),
        Some(detail) => format!("{headline}: {detail} (run `blue login`)"),
        None if forbidden => headline.to_owned(),
        None => format!("{headline} (run `blue login`)"),
    }
}

fn session_rejection(status: reqwest::StatusCode, detail: Option<&str>) -> GhError {
    let forbidden = status == reqwest::StatusCode::FORBIDDEN;
    let message = session_rejected_message(forbidden, detail);
    if forbidden {
        GhError::forbidden(message)
    } else {
        GhError::unauthorized(message)
    }
}

/// Fetches from `GET {base_url}/governance-config` with a bearer token.
pub struct HttpConfigSource {
    base_url: String,
    client: reqwest::blocking::Client,
}

impl HttpConfigSource {
    pub fn new(base_url: impl Into<String>) -> Result<Self, GhError> {
        let client = reqwest::blocking::Client::builder()
            // `format!`, not `concat!`: `concat!` needs a literal, and the
            // version is only a literal for a release build. A candidate has
            // to identify itself as one here too.
            .user_agent(format!("blue/{}", gh_common::blue_version()))
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
            // 401 and 403 are not the same problem and neither is always
            // "log in again": a dead browser binding, a revoked token, a
            // suspended user and a missing OAuth scope all land here, and the
            // server already writes a specific message for each. Collapsing
            // them into one fixed string threw all of it away.
            let detail = server_error_detail(&resp.text().unwrap_or_default());
            return Err(session_rejection(status, detail.as_deref()));
        }
        if status == reqwest::StatusCode::UPGRADE_REQUIRED {
            let body = bounded_detail(&resp.text().unwrap_or_default())
                .unwrap_or_else(|| "no error detail".to_owned());
            return Err(GhError::config(format!(
                "control service rejected this client version: {body}"
            )));
        }
        if status == reqwest::StatusCode::CONFLICT {
            // The server's 409 message names the command to run and is already
            // written for a human, so pass it through verbatim.
            let detail = bounded_detail(&resp.text().unwrap_or_default())
                .unwrap_or_else(|| "the control service requires another action".to_owned());
            return Err(GhError::action_required(detail));
        }
        if !status.is_success() {
            let body = bounded_detail(&resp.text().unwrap_or_default())
                .unwrap_or_else(|| "no error detail".to_owned());
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
        self.endpoint()
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
    fn source_descriptions_distinguish_http_endpoints_from_local_files() {
        let http = HttpConfigSource::new("https://api.bluee.sh/").unwrap();
        assert_eq!(http.describe(), "https://api.bluee.sh/governance-config");

        let file = FileConfigSource::new("/tmp/blue-governance.yaml");
        assert_eq!(file.describe(), "file(/tmp/blue-governance.yaml)");
    }

    #[test]
    fn a_rejected_session_relays_the_reason_the_server_gave() {
        assert_eq!(
            session_rejected_message(false, Some("your browser sign-in has expired")),
            "your session is no longer valid: your browser sign-in has expired (run `blue login`)"
        );
        assert_eq!(
            session_rejected_message(false, None),
            "your session is no longer valid (run `blue login`)"
        );
        // A 403 is a different problem and must not claim the session expired.
        assert_eq!(
            session_rejected_message(true, Some("missing OAuth scope governance:read")),
            "the control service refused this session: missing OAuth scope governance:read"
        );
        assert!(matches!(
            session_rejection(reqwest::StatusCode::UNAUTHORIZED, None),
            GhError::Unauthorized(_)
        ));
        assert!(matches!(
            session_rejection(reqwest::StatusCode::FORBIDDEN, None),
            GhError::Forbidden(_)
        ));
    }

    #[test]
    fn only_a_well_formed_error_body_is_relayed() {
        assert_eq!(
            server_error_detail(r#"{"error":"invalid or expired session"}"#).as_deref(),
            Some("invalid or expired session")
        );
        assert_eq!(server_error_detail(""), None);
        assert_eq!(server_error_detail(r#"{"error":"   "}"#), None);
        assert_eq!(server_error_detail(r#"{"detail":"nope"}"#), None);
        // An ALB in front of control-api answers with HTML, not JSON.
        assert_eq!(
            server_error_detail("<html><head><title>401 Unauthorized</title></head></html>"),
            None
        );
    }

    #[test]
    fn an_oversized_detail_is_capped() {
        let body = serde_json::json!({ "error": "x".repeat(5_000) }).to_string();
        let detail = server_error_detail(&body).unwrap();
        assert_eq!(detail.chars().count(), MAX_DETAIL_CHARS);
    }

    #[test]
    fn terminal_controls_are_removed_from_server_details() {
        assert_eq!(
            bounded_detail(" first\nsecond\r\u{1b}]52;clipboard\u{7}\tlast ").as_deref(),
            Some("first second ]52;clipboard last")
        );
        assert_eq!(bounded_detail("\u{1b}\u{7}"), None);
    }

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
