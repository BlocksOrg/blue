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

use crate::source::bounded_detail;

/// A refresh attempt distinguishes a permanently spent credential from a
/// failure that may succeed unchanged on retry.
#[derive(Debug, thiserror::Error)]
pub enum RefreshFailure {
    #[error("the stored refresh credential is no longer valid")]
    InvalidGrant,
    #[error(transparent)]
    Temporary(#[from] GhError),
}

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
    /// The RFC 8707 resource indicator the grant was issued against, recorded
    /// **verbatim** as it was sent. The authorization server matches the
    /// identifier, not a normalized form of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// The scope the server actually *granted*, not the one we asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
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
            resource: None,
            scope: None,
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

    /// The refresh-grant form body. Pure, so the one thing that is easy to get
    /// wrong — which parameters actually go on the wire — is testable without
    /// a socket or a session file.
    fn refresh_form(&self) -> Result<Vec<(&str, &str)>, GhError> {
        let refresh_token = self
            .refresh_token
            .as_deref()
            .ok_or_else(|| GhError::unauthorized("login expired — run `blue login` again"))?;
        let client_id = self.client_id.as_deref().ok_or_else(|| {
            GhError::unauthorized("login cannot be refreshed — run `blue login` again")
        })?;
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ];
        // Without the resource indicator the server issues a token for its own
        // default audience, and `enforcePerClientResources` then rejects it at
        // the Control API. Every other grant in this repo sends it.
        if let Some(resource) = non_empty(self.resource.as_deref()) {
            form.push(("resource", resource));
        }
        // RFC 6749 §6: omitting `scope` means "as originally granted". Only a
        // scope the server itself handed back is safe to replay — replaying a
        // requested list the server narrowed yields `invalid_scope`.
        if let Some(scope) = non_empty(self.scope.as_deref()) {
            form.push(("scope", scope));
        }
        Ok(form)
    }

    /// Adopt the refreshed grant. Pure; the caller decides when to persist.
    fn apply_token_response(&mut self, tokens: TokenResponse, now: i64) {
        self.token = tokens.access_token;
        self.expires_at = Some(now + tokens.expires_in.unwrap_or(900));
        if tokens.refresh_token.is_some() {
            self.refresh_token = tokens.refresh_token;
        }
        if let Some(scope) = tokens.scope.filter(|scope| !scope.trim().is_empty()) {
            self.scope = Some(scope);
        }
    }

    /// Exchange the refresh token unconditionally and return the rotated
    /// session without touching disk.
    pub fn refreshed(&self, now: i64) -> Result<Session, RefreshFailure> {
        let token_endpoint = self.token_endpoint.clone().ok_or_else(|| {
            RefreshFailure::Temporary(GhError::unauthorized(
                "login cannot be refreshed — run `blue login` again",
            ))
        })?;
        let form = self.refresh_form().map_err(RefreshFailure::Temporary)?;
        // Every `blue` invocation reaches this through `session_for`. Without a
        // timeout a hung identity provider hangs the CLI forever.
        let response = oauth_client()?
            .post(&token_endpoint)
            .form(&form)
            .send()
            .map_err(|error| {
                RefreshFailure::Temporary(GhError::service(format!(
                    "refresh request failed: {error}"
                )))
            })?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().map_err(|error| {
                RefreshFailure::Temporary(GhError::service(format!(
                    "reading refresh rejection ({status}): {error}"
                )))
            })?;
            return Err(classify_refresh_rejection(status, &body));
        }
        let tokens: TokenResponse = response.json().map_err(|error| {
            RefreshFailure::Temporary(GhError::service(format!(
                "invalid refresh response: {error}"
            )))
        })?;
        let mut refreshed = self.clone();
        refreshed.apply_token_response(tokens, now);
        Ok(refreshed)
    }

    /// Exchange the refresh token unconditionally. Does not touch the disk.
    pub(crate) fn refresh_now(&mut self, now: i64) -> Result<(), GhError> {
        *self = self.refreshed(now).map_err(refresh_failure_error)?;
        Ok(())
    }

    pub fn refresh_if_needed(&mut self, now: i64) -> Result<(), GhError> {
        if self
            .expires_at
            .map(|expires| expires > now + 30)
            .unwrap_or(true)
        {
            return Ok(());
        }
        self.refresh_now(now)?;
        self.save()
    }

    /// Record the resource indicator a pre-0.1 `session.json` predates, so its
    /// next refresh carries it. Never invent a legacy session's scope: omitting
    /// it asks the authorization server to retain the originally granted set.
    ///
    /// The caller supplies them because only it knows the active deployment;
    /// reading `blue.toml` in here would make refreshing depend on ambient
    /// filesystem state and could send a resource this grant never named.
    pub fn adopt_refresh_context(&mut self, resource: &str) {
        if self.resource.is_none() {
            if let Some(resource) = non_empty(Some(resource)) {
                self.resource = Some(resource.to_owned());
            }
        }
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
        let response = oauth_client()?
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
    /// refresh grant is revoked. An unauthorized response is idempotent
    /// success: dashboard-wide revocation invalidates the bearer before a
    /// client can confirm the gateway cleanup it already performed.
    pub fn revoke_gateway_session(&self, service_url: &str) -> Result<(), GhError> {
        if service_url.trim().is_empty() {
            return Ok(());
        }
        let endpoint = reqwest::Url::parse(&format!("{}/", service_url.trim_end_matches('/')))
            .and_then(|url| url.join("gateway/session/revoke"))
            .map_err(|error| GhError::config(format!("invalid Control API URL: {error}")))?;
        let response = oauth_client()?
            .post(endpoint)
            .bearer_auth(&self.token)
            .send()
            .map_err(|error| {
                GhError::service(format!("gateway session revocation failed: {error}"))
            })?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            Ok(())
        } else {
            Err(GhError::service(format!(
                "gateway session revocation was rejected ({})",
                response.status()
            )))
        }
    }
}

fn refresh_failure_error(error: RefreshFailure) -> GhError {
    match error {
        RefreshFailure::InvalidGrant => GhError::unauthorized(
            "login expired and the refresh credential is no longer valid — run `blue login` again",
        ),
        RefreshFailure::Temporary(error) => error,
    }
}

fn classify_refresh_rejection(status: reqwest::StatusCode, body: &str) -> RefreshFailure {
    let oauth_error = serde_json::from_str::<OAuthError>(body).ok();
    if oauth_error
        .as_ref()
        .is_some_and(|error| error.error == "invalid_grant")
    {
        return RefreshFailure::InvalidGrant;
    }
    let detail = oauth_error
        .and_then(|error| error.error_description)
        .and_then(|detail| bounded_detail(&detail))
        .or_else(|| bounded_detail(body))
        .unwrap_or_else(|| "no error detail".to_owned());
    RefreshFailure::Temporary(GhError::service(format!(
        "refresh was rejected ({status}): {detail}"
    )))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    /// What the server granted. Absent on servers that grant exactly what was
    /// asked for.
    #[serde(default)]
    scope: Option<String>,
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

/// The scopes the reference deployment's CLI client needs. `discovery.rs`
/// explicitly permits an empty `scopes` array, so the device flow and the
/// legacy-session adopter have to agree on the same list.
pub fn default_cli_scopes() -> Vec<String> {
    [
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
    .collect()
}

/// The blocking HTTP client every OAuth exchange uses. The timeout is the
/// point: an unbounded client here hangs `blue` itself.
fn oauth_client() -> Result<reqwest::blocking::Client, GhError> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| GhError::service(format!("building OAuth client: {error}")))
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
        default_cli_scopes()
    } else {
        configured_scopes.to_vec()
    };
    let http = oauth_client()?;
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
        let detail = bounded_detail(&response.text().unwrap_or_default())
            .unwrap_or_else(|| "no error detail".to_owned());
        return Err(GhError::service(format!(
            "device authorization rejected: {detail}"
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
            // The granted scope, not the requested one, and the resource
            // string exactly as it was sent — the server matches the
            // identifier it was given, not a normalized form of it.
            let granted_scope = tokens
                .scope
                .clone()
                .filter(|scope| !scope.trim().is_empty())
                .unwrap_or_else(|| scopes.join(" "));
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
                resource: non_empty(Some(resource)).map(str::to_owned),
                scope: Some(granted_scope),
            });
        }
        let status = response.status();
        let body = response.text().map_err(|error| {
            GhError::service(format!("reading OAuth error response ({status}): {error}"))
        })?;
        let error: OAuthError = serde_json::from_str(&body).map_err(|decode_error| {
            let summary = bounded_detail(&body).unwrap_or_else(|| "no error detail".to_owned());
            GhError::service(format!(
                "OAuth token endpoint returned {status} with an invalid error response: {summary} ({decode_error})"
            ))
        })?;
        match error.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += 5,
            _ => {
                let detail = error
                    .error_description
                    .and_then(|detail| bounded_detail(&detail))
                    .or_else(|| bounded_detail(&error.error))
                    .unwrap_or_else(|| "OAuth request rejected".to_owned());
                return Err(GhError::service(detail));
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

    fn refreshable() -> Session {
        Session {
            token: "old-access".into(),
            sub: None,
            email: None,
            org_id: None,
            groups: Vec::new(),
            expires_at: Some(0),
            refresh_token: Some("refresh".into()),
            token_endpoint: Some("http://127.0.0.1:1/oauth2/token".into()),
            client_id: Some("blue-cli".into()),
            resource: Some("https://control.example.com".into()),
            scope: Some("openid governance:read".into()),
        }
    }

    /// The single test that would have caught the omission: every other grant
    /// in the repo sends `resource`, the refresh grant silently did not, and
    /// no end-to-end test covered `refresh_if_needed` at all.
    #[test]
    fn refresh_form_carries_the_resource_indicator() {
        let session = refreshable();
        let form = session.refresh_form().unwrap();
        assert!(
            form.contains(&("resource", "https://control.example.com")),
            "{form:?}"
        );
        assert!(
            form.contains(&("scope", "openid governance:read")),
            "{form:?}"
        );
        assert!(form.contains(&("grant_type", "refresh_token")), "{form:?}");
    }

    #[test]
    fn refresh_form_omits_what_a_legacy_session_never_recorded() {
        let mut session = refreshable();
        session.resource = None;
        session.scope = Some("   ".into());
        let form = session.refresh_form().unwrap();
        assert!(!form.iter().any(|(key, _)| *key == "resource"), "{form:?}");
        // Sending `scope=` empty is not the same as omitting it.
        assert!(!form.iter().any(|(key, _)| *key == "scope"), "{form:?}");
    }

    #[test]
    fn adopt_refresh_context_never_overwrites_the_issued_grant() {
        let mut session = refreshable();
        session.adopt_refresh_context("https://other.example.com");
        assert_eq!(
            session.resource.as_deref(),
            Some("https://control.example.com")
        );
        assert_eq!(session.scope.as_deref(), Some("openid governance:read"));
    }

    #[test]
    fn adopt_refresh_context_only_backfills_a_legacy_resource() {
        let mut session = refreshable();
        session.resource = None;
        session.scope = None;
        session.adopt_refresh_context("https://control.example.com");
        assert_eq!(
            session.resource.as_deref(),
            Some("https://control.example.com")
        );
        assert_eq!(session.scope, None);
        assert!(
            !session
                .refresh_form()
                .unwrap()
                .iter()
                .any(|(key, _)| *key == "scope"),
            "a legacy refresh must let the authorization server retain its original scope"
        );

        // A file-source deployment has no service URL to adopt.
        let mut local = Session::bearer("t");
        local.adopt_refresh_context("");
        assert_eq!(local.resource, None);
    }

    #[test]
    fn the_granted_scope_wins_over_the_requested_one() {
        let mut session = refreshable();
        session.apply_token_response(
            TokenResponse {
                access_token: "new-access".into(),
                refresh_token: Some("rotated".into()),
                expires_in: Some(900),
                scope: Some("openid".into()),
            },
            1_000,
        );
        assert_eq!(session.token, "new-access");
        assert_eq!(session.refresh_token.as_deref(), Some("rotated"));
        assert_eq!(session.expires_at, Some(1_900));
        assert_eq!(session.scope.as_deref(), Some("openid"));
    }

    #[test]
    fn a_response_without_a_scope_keeps_the_one_already_granted() {
        let mut session = refreshable();
        session.apply_token_response(
            TokenResponse {
                access_token: "new-access".into(),
                refresh_token: None,
                expires_in: None,
                scope: None,
            },
            1_000,
        );
        assert_eq!(session.scope.as_deref(), Some("openid governance:read"));
        // A response that rotates nothing must not clear the refresh token.
        assert_eq!(session.refresh_token.as_deref(), Some("refresh"));
    }

    /// Read a full HTTP request off `stream`: headers, then exactly
    /// `Content-Length` more bytes. A single `read` returns only the headers
    /// for a POST, which would deadlock a body assertion.
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut raw = Vec::new();
        let mut byte = [0u8; 1];
        while !raw.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap() == 0 {
                break;
            }
            raw.push(byte[0]);
        }
        let headers = String::from_utf8_lossy(&raw).into_owned();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .map(|value| value.trim().parse().unwrap())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).unwrap();
        format!("{headers}{}", String::from_utf8_lossy(&body))
    }

    // `reqwest::blocking` panics inside a tokio runtime, so these are plain
    // `#[test]`s against a hand-rolled listener — the pattern `discovery.rs`
    // already uses, and the reason no http-mock dev-dependency exists.
    #[test]
    fn refresh_now_sends_the_resource_and_adopts_the_new_token() {
        use std::io::Write;
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            // Some hermetic test runners prohibit even loopback sockets.
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("binding mock token endpoint: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let body = r#"{"access_token":"new-access","expires_in":900,"scope":"openid"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            request
        });

        let mut session = refreshable();
        session.resource = Some(format!("http://{address}"));
        session.token_endpoint = Some(format!("http://{address}/oauth2/token"));
        session.refresh_now(1_000).unwrap();

        let request = server.join().unwrap();
        assert!(request.starts_with("POST /oauth2/token "), "{request}");
        assert!(
            request.contains(&format!(
                "resource={}",
                urlencoding_of(&format!("http://{address}"))
            )),
            "{request}"
        );
        assert_eq!(session.token, "new-access");
        assert_eq!(session.expires_at, Some(1_900));
        assert_eq!(session.scope.as_deref(), Some("openid"));
    }

    #[test]
    fn invalid_grant_is_distinct_from_temporary_refresh_failures() {
        let invalid = classify_refresh_rejection(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_grant","error_description":"spent"}"#,
        );
        assert!(
            matches!(invalid, RefreshFailure::InvalidGrant),
            "{invalid:?}"
        );
        assert!(matches!(
            classify_refresh_rejection(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                r#"{"error":"server_error","error_description":"later"}"#
            ),
            RefreshFailure::Temporary(GhError::Service(_))
        ));
    }

    #[test]
    fn gateway_revocation_accepts_an_already_unauthorized_session() {
        use std::io::{Read, Write};

        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("binding mock gateway endpoint: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let size = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            String::from_utf8_lossy(&request[..size]).into_owned()
        });

        let session = Session::bearer("already-revoked-access-token");
        session
            .revoke_gateway_session(&format!("http://{address}"))
            .unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /gateway/session/revoke "));
        assert!(
            request.contains("Authorization: Bearer already-revoked-access-token")
                || request.contains("authorization: Bearer already-revoked-access-token"),
            "{request}"
        );
    }

    #[test]
    fn refresh_if_needed_makes_no_request_while_the_token_is_fresh() {
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("binding mock token endpoint: {error}"),
        };
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();

        let mut session = refreshable();
        session.token_endpoint = Some(format!("http://{address}/oauth2/token"));
        session.expires_at = Some(10_000);
        // No `save()` either — a fresh token short-circuits before any IO.
        session.refresh_if_needed(1_000).unwrap();

        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "a fresh token must not reach the token endpoint"
        );
    }

    /// `application/x-www-form-urlencoded` escaping of the bytes reqwest emits
    /// for a URL value, so the assertion above compares like with like.
    fn urlencoding_of(value: &str) -> String {
        value
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                    (byte as char).to_string()
                }
                b' ' => "+".to_string(),
                _ => format!("%{byte:02X}"),
            })
            .collect()
    }

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
