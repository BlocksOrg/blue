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
    client_version: String,
    client: reqwest::blocking::Client,
}

impl HttpConfigSource {
    pub fn new(base_url: impl Into<String>, client_version: &str) -> Result<Self, GhError> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(format!("blue/{client_version}"))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| GhError::service(format!("building http client: {e}")))?;
        Ok(HttpConfigSource {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            client_version: client_version.into(),
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
        let pin = if status.is_success() || status == reqwest::StatusCode::UPGRADE_REQUIRED {
            if resp
                .headers()
                .get_all("x-blue-required-client-version")
                .iter()
                .count()
                > 1
            {
                return Err(GhError::config("multiple required client version headers"));
            }
            resp.headers()
                .get("x-blue-required-client-version")
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_owned)
                        .map_err(|_| GhError::config("invalid required client version header"))
                })
                .transpose()?
        } else {
            None
        };
        if let Some(required) = &pin {
            GovernanceConfig::check_client_version(required, &self.client_version)?;
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
            .map_err(|e| GhError::Serde(format!("decoding governance-config: {e}")))?;
        if let Some(required) = pin {
            if config.required_client_version.as_deref() != Some(required.as_str()) {
                return Err(GhError::config(
                    "required client version header and document disagree",
                ));
            }
        }
        config.ensure_client_compatible(&self.client_version)?;
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
    client_version: String,
}

impl FileConfigSource {
    pub fn new(path: impl Into<PathBuf>, client_version: &str) -> Self {
        FileConfigSource {
            path: path.into(),
            client_version: client_version.into(),
        }
    }
}

impl ConfigSource for FileConfigSource {
    fn fetch(&self, _session: &Session) -> Result<GovernanceConfig, GhError> {
        let text = std::fs::read_to_string(&self.path).map_err(|e| GhError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        let config = parse_config(&self.path, &text)?;
        config.ensure_client_compatible(&self.client_version)?;
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

#[cfg(test)]
mod client_version_tests {
    use super::*;
    use std::io::{Read, Write};
    fn fetch_response(
        status: &str,
        headers: &str,
        body: &str,
    ) -> Result<GovernanceConfig, GhError> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut request = [0; 8192];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
        });
        let result = HttpConfigSource::new(format!("http://{address}"), "7.8.9")
            .unwrap()
            .fetch(&Session::bearer("fixture"));
        thread.join().unwrap();
        result
    }
    #[test]
    fn http_header_enforces_pin_on_success_and_426_but_auth_has_priority() {
        for status in ["200 OK", "426 Upgrade Required"] {
            assert!(matches!(
                fetch_response(
                    status,
                    "X-Blue-Required-Client-Version: 8.0.0\r\n",
                    "not a document"
                ),
                Err(GhError::ClientVersionMismatch { .. })
            ));
        }
        assert!(matches!(
            fetch_response(
                "401 Unauthorized",
                "X-Blue-Required-Client-Version: 8.0.0\r\n",
                ""
            ),
            Err(GhError::Unauthorized(_))
        ));
        assert!(matches!(
            fetch_response(
                "426 Upgrade Required",
                "X-Blue-Required-Client-Version: 7.9.0\r\n",
                "unsupported capability"
            ),
            Err(GhError::Config(_))
        ));
    }
    #[test]
    fn http_rejects_malformed_and_conflicting_pins_and_supports_old_servers() {
        let body = r#"{"revision":"r1", "required_client_version":"7.8.9"}"#;
        assert!(
            fetch_response("200 OK", "X-Blue-Required-Client-Version: 7.8.9\r\n", body).is_ok()
        );
        assert!(fetch_response(
            "200 OK",
            "X-Blue-Required-Client-Version: 7.9.0\r\n",
            r#"{"revision":"r1", "required_client_version":"7.9.0"}"#
        )
        .is_ok());
        assert!(fetch_response("200 OK", "", body).is_ok());
        assert!(matches!(
            fetch_response("200 OK", "X-Blue-Required-Client-Version: ^7.8.9\r\n", body),
            Err(GhError::Config(_))
        ));
        assert!(matches!(
            fetch_response(
                "200 OK",
                "X-Blue-Required-Client-Version: 7.8.9\r\n",
                r#"{"revision":"r1","required_client_version":"7.8.10"}"#
            ),
            Err(GhError::Config(_))
        ));
    }
    #[test]
    fn file_source_uses_executing_cli_version() {
        let _guard = crate::cache::test_support::with_cache_home("version-file");
        let path = gh_common::paths::blue_config_dir()
            .unwrap()
            .join("pin.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "revision: r1\nrequired_client_version: 7.8.9\n").unwrap();
        assert!(FileConfigSource::new(&path, "7.8.9")
            .fetch(&Session::bearer("fixture"))
            .is_ok());
        assert!(matches!(
            FileConfigSource::new(&path, "8.0.0").fetch(&Session::bearer("fixture")),
            Err(GhError::ClientVersionMismatch { .. })
        ));
        assert!(FileConfigSource::new(&path, "7.8.10")
            .fetch(&Session::bearer("fixture"))
            .is_ok());
    }
}
