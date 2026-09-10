//! Authentication to the upstream service.
//!
//! Identity is modeled as **claims, not a bare key** ([`Session`] carries
//! `org_id`/`email`/`groups`) so the service can scope governance-config and
//! inference-token issuance to org + group. Auth is a pluggable
//! [`IdentityProvider`]. The reference service uses OAuth device authorization;
//! static token mode remains available for local files and BYO services.

use serde::{Deserialize, Serialize};

use gh_common::client_config::IdentityConfig;
use gh_common::{paths, write_atomic, GhError};

/// A persisted login session. The `token` is a bearer credential presented to
/// the service; the claims drive authorization decisions server-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Bearer token presented on every service call.
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Unix seconds; `None` for non-expiring dev tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

impl Session {
    pub fn bearer(token: impl Into<String>) -> Self {
        Session {
            token: token.into(),
            sub: None,
            email: None,
            org_id: None,
            groups: Vec::new(),
            expires_at: None,
            refresh_token: None,
            token_endpoint: None,
            client_id: None,
        }
    }

    /// Persist to `session.json` atomically (0600).
    pub fn save(&self) -> Result<(), GhError> {
        let path = paths::session_path()?;
        let body = serde_json::to_vec_pretty(self).map_err(|e| GhError::Serde(e.to_string()))?;
        write_atomic(&path, body)
    }

    /// Load the persisted session, if any.
    pub fn load() -> Result<Option<Session>, GhError> {
        let path = paths::session_path()?;
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| GhError::Serde(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(GhError::Io { path, source: e }),
        }
    }

    pub fn remove() -> Result<(), GhError> {
        let path = paths::session_path()?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(GhError::Io { path, source }),
        }
    }

    pub fn refresh_if_needed(&mut self, now: i64) -> Result<(), GhError> {
        if self
            .expires_at
            .map(|expires| expires > now + 30)
            .unwrap_or(true)
        {
            return Ok(());
        }
        let refresh_token = self
            .refresh_token
            .as_deref()
            .ok_or_else(|| GhError::other("login expired — run `blue login` again"))?;
        let token_endpoint = self
            .token_endpoint
            .as_deref()
            .ok_or_else(|| GhError::other("login cannot be refreshed — run `blue login` again"))?;
        let client_id = self
            .client_id
            .as_deref()
            .ok_or_else(|| GhError::other("login cannot be refreshed — run `blue login` again"))?;
        let response = reqwest::blocking::Client::new()
            .post(token_endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", client_id),
            ])
            .send()
            .map_err(|error| GhError::service(format!("refresh request failed: {error}")))?;
        if !response.status().is_success() {
            return Err(GhError::other(
                "login expired and refresh was rejected — run `blue login` again",
            ));
        }
        let tokens: TokenResponse = response
            .json()
            .map_err(|error| GhError::service(format!("invalid refresh response: {error}")))?;
        self.token = tokens.access_token;
        self.expires_at = Some(now + tokens.expires_in.unwrap_or(900));
        if tokens.refresh_token.is_some() {
            self.refresh_token = tokens.refresh_token;
        }
        self.save()
    }

    pub fn revoke(&self) -> Result<(), GhError> {
        let (Some(refresh_token), Some(token_endpoint), Some(client_id)) = (
            self.refresh_token.as_deref(),
            self.token_endpoint.as_deref(),
            self.client_id.as_deref(),
        ) else {
            return Ok(());
        };
        let revoke_endpoint = reqwest::Url::parse(token_endpoint)
            .and_then(|url| url.join("revoke"))
            .map_err(|error| {
                GhError::config(format!("invalid OAuth revocation endpoint: {error}"))
            })?;
        let response = reqwest::blocking::Client::new()
            .post(revoke_endpoint)
            .form(&[
                ("token", refresh_token),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ])
            .send()
            .map_err(|error| GhError::service(format!("logout revocation failed: {error}")))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(GhError::service(format!(
                "logout revocation was rejected ({})",
                response.status()
            )))
        }
    }

    /// Revoke the gateway session bound to this OAuth access token before the
    /// refresh grant is revoked. Failure is surfaced so callers can warn while
    /// still removing local credentials.
    pub fn revoke_gateway_session(&self, service_url: &str) -> Result<(), GhError> {
        if service_url.trim().is_empty() {
            return Ok(());
        }
        let endpoint = reqwest::Url::parse(&format!("{}/", service_url.trim_end_matches('/')))
            .and_then(|url| url.join("gateway/session/revoke"))
            .map_err(|error| GhError::config(format!("invalid Control API URL: {error}")))?;
        let response = reqwest::blocking::Client::new()
            .post(endpoint)
            .bearer_auth(&self.token)
            .send()
            .map_err(|error| {
                GhError::service(format!("gateway session revocation failed: {error}"))
            })?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(GhError::service(format!(
                "gateway session revocation was rejected ({})",
                response.status()
            )))
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
    error_description: Option<String>,
}

fn verification_url(device: &DeviceCodeResponse) -> &str {
    device
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&device.verification_uri)
}

fn open_device_authorization(device: &DeviceCodeResponse) {
    let url = verification_url(device);
    println!("Confirmation code: {}", device.user_code);
    match webbrowser::open(url) {
        Ok(()) => println!("Opened your browser to authorize the CLI."),
        Err(error) => println!(
            "Could not open the default browser ({error}).\nOpen this temporary URL to continue:\n{url}"
        ),
    }
}

pub fn device_login(
    issuer: &str,
    client_id: &str,
    configured_scopes: &[String],
    resource: &str,
) -> Result<Session, GhError> {
    let issuer = reqwest::Url::parse(&format!("{}/", issuer.trim_end_matches('/')))
        .map_err(|error| GhError::config(format!("invalid OAuth issuer: {error}")))?;
    let device_endpoint = issuer
        .join("device/code")
        .map_err(|error| GhError::config(format!("invalid OAuth device endpoint: {error}")))?;
    let token_endpoint = issuer
        .join("oauth2/token")
        .map_err(|error| GhError::config(format!("invalid OAuth token endpoint: {error}")))?;
    let scopes = if configured_scopes.is_empty() {
        vec![
            "openid",
            "profile",
            "email",
            "offline_access",
            "governance:read",
            "session:write",
            "client-status:write",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>()
    } else {
        configured_scopes.to_vec()
    };
    let http = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| GhError::service(format!("building OAuth client: {error}")))?;
    let response = http
        .post(device_endpoint)
        .form(&[
            ("client_id", client_id),
            ("scope", &scopes.join(" ")),
            ("resource", resource),
        ])
        .send()
        .map_err(|error| GhError::service(format!("device authorization failed: {error}")))?;
    if !response.status().is_success() {
        return Err(GhError::service(format!(
            "device authorization rejected: {}",
            response.text().unwrap_or_default()
        )));
    }
    let device: DeviceCodeResponse = response.json().map_err(|error| {
        GhError::service(format!("invalid device authorization response: {error}"))
    })?;
    open_device_authorization(&device);

    let started = std::time::Instant::now();
    let mut interval = device.interval.max(1);
    loop {
        if started.elapsed().as_secs() >= device.expires_in {
            return Err(GhError::other(
                "device authorization expired — run `blue login` again",
            ));
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
        let response = http
            .post(token_endpoint.clone())
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device.device_code.as_str()),
                ("client_id", client_id),
                ("resource", resource),
            ])
            .send()
            .map_err(|error| GhError::service(format!("token polling failed: {error}")))?;
        if response.status().is_success() {
            let tokens: TokenResponse = response
                .json()
                .map_err(|error| GhError::service(format!("invalid token response: {error}")))?;
            return Ok(Session {
                token: tokens.access_token,
                sub: None,
                email: None,
                org_id: None,
                groups: Vec::new(),
                expires_at: Some(crate::now_unix() + tokens.expires_in.unwrap_or(900)),
                refresh_token: tokens.refresh_token,
                token_endpoint: Some(token_endpoint.to_string()),
                client_id: Some(client_id.to_owned()),
            });
        }
        let status = response.status();
        let body = response.text().map_err(|error| {
            GhError::service(format!("reading OAuth error response ({status}): {error}"))
        })?;
        let error: OAuthError = serde_json::from_str(&body).map_err(|decode_error| {
            let summary = body.chars().take(500).collect::<String>();
            GhError::service(format!(
                "OAuth token endpoint returned {status} with an invalid error response: {summary} ({decode_error})"
            ))
        })?;
        match error.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += 5,
            _ => {
                return Err(GhError::service(
                    error.error_description.unwrap_or(error.error),
                ))
            }
        }
    }
}

/// Pluggable authenticator. Implementations turn `blue.toml` `[identity]`
/// into a [`Session`].
pub trait IdentityProvider {
    fn login(&self) -> Result<Session, GhError>;
}

/// Simplest self-host: a static API token, optionally via a secret reference.
pub struct TokenProvider {
    token_ref: String,
}

impl TokenProvider {
    pub fn new(token_ref: impl Into<String>) -> Self {
        TokenProvider {
            token_ref: token_ref.into(),
        }
    }
}

impl IdentityProvider for TokenProvider {
    fn login(&self) -> Result<Session, GhError> {
        let token = resolve_secret(&self.token_ref)?;
        if token.is_empty() {
            return Err(GhError::config(
                "identity token is empty (set [identity].token in blue.toml)",
            ));
        }
        Ok(Session::bearer(token))
    }
}

/// OIDC device-authorization configuration. Service-aware login is performed
/// by `client::login`, which supplies the Control API resource indicator.
pub struct OidcProvider {
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

impl IdentityProvider for OidcProvider {
    fn login(&self) -> Result<Session, GhError> {
        Err(GhError::other(
            "OIDC login requires a configured service URL",
        ))
    }
}

/// Build the configured provider from `blue.toml` `[identity]`.
pub fn provider_from_config(cfg: &IdentityConfig) -> Box<dyn IdentityProvider> {
    match cfg {
        IdentityConfig::Token { token } => Box::new(TokenProvider::new(token.clone())),
        IdentityConfig::Oidc {
            issuer,
            client_id,
            scopes,
        } => Box::new(OidcProvider {
            issuer: issuer.clone(),
            client_id: client_id.clone(),
            scopes: scopes.clone(),
        }),
    }
}

/// Resolve a secret reference. Mirrors the server-side `blue.yaml`
/// convention: `env://VAR`, `file://path`, or an inline literal.
pub fn resolve_secret(reference: &str) -> Result<String, GhError> {
    if let Some(var) = reference.strip_prefix("env://") {
        std::env::var(var)
            .map_err(|_| GhError::config(format!("env var `{var}` referenced but not set")))
    } else if let Some(path) = reference.strip_prefix("file://") {
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .map_err(|e| GhError::Io {
                path: path.into(),
                source: e,
            })
    } else {
        Ok(reference.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_verification_url_is_preferred() {
        let response = DeviceCodeResponse {
            device_code: "device".into(),
            user_code: "ABCD-EFGH".into(),
            verification_uri: "https://example.com/device".into(),
            verification_uri_complete: Some("https://example.com/device/opaque".into()),
            expires_in: 600,
            interval: 5,
        };
        assert_eq!(
            verification_url(&response),
            "https://example.com/device/opaque"
        );
    }

    #[test]
    fn base_verification_url_is_the_fallback() {
        let response = DeviceCodeResponse {
            device_code: "device".into(),
            user_code: "ABCD-EFGH".into(),
            verification_uri: "https://example.com/device".into(),
            verification_uri_complete: None,
            expires_in: 600,
            interval: 5,
        };
        assert_eq!(verification_url(&response), "https://example.com/device");
    }

    #[test]
    fn resolves_inline_and_env() {
        assert_eq!(resolve_secret("literal").unwrap(), "literal");
        std::env::set_var("GH_TEST_SECRET", "sekret");
        assert_eq!(resolve_secret("env://GH_TEST_SECRET").unwrap(), "sekret");
    }

    #[test]
    fn token_provider_rejects_empty() {
        assert!(TokenProvider::new("").login().is_err());
    }

    #[test]
    fn oidc_provider_requires_service_context() {
        let p = OidcProvider {
            issuer: "https://sso".into(),
            client_id: "id".into(),
            scopes: vec![],
        };
        assert!(p.login().is_err());
    }
}
