//! `ServiceClient` — the one entry point the CLI uses to obtain governance
//! config: pick the source from `blue.toml`, fetch + cache, and fail-soft to
//! a fresh cache when the service is briefly unreachable.

use gh_common::GhError;
use gh_common::{paths, BlueToml};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Read};

use crate::cache::{self, CachedConfig};
use crate::identity::{self, Session};
use crate::schema::GovernanceConfig;
use crate::source::{ConfigSource, FileConfigSource, HttpConfigSource};

pub struct ServiceClient {
    source: Box<dyn ConfigSource>,
    service_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RevisionStreamEnd {
    /// The configured service does not implement the optional SSE endpoint.
    Unsupported,
    /// A previously-established stream ended and should be reconnected.
    Disconnected,
}

impl ServiceClient {
    /// Build the client from `blue.toml`: `http` if a service URL is set,
    /// otherwise a `file` source (explicit `config_file`, else a conventional
    /// local path). This is the "no vendor URL, zero-config file dev" path.
    pub fn from_config(cfg: &BlueToml) -> Result<Self, GhError> {
        let source: Box<dyn ConfigSource> = if cfg.has_http_service() {
            Box::new(HttpConfigSource::new(cfg.service.url.clone())?)
        } else if let Some(file) = &cfg.service.config_file {
            Box::new(FileConfigSource::new(file))
        } else {
            let default = paths::blue_config_dir()?.join("blue.yaml");
            Box::new(FileConfigSource::new(default))
        };
        Ok(ServiceClient {
            source,
            service_url: cfg.has_http_service().then(|| cfg.service.url.clone()),
        })
    }

    pub fn with_source(source: Box<dyn ConfigSource>) -> Self {
        ServiceClient {
            source,
            service_url: None,
        }
    }

    pub fn describe_source(&self) -> String {
        self.source.describe()
    }

    /// Block on the optional server-sent revision stream until it disconnects.
    /// The stream carries invalidation IDs only; callers fetch the authoritative
    /// configuration through the normal config endpoint on their next launch.
    pub fn stream_revisions(
        &self,
        session: &Session,
        on_revision: impl FnMut(String),
    ) -> Result<RevisionStreamEnd, GhError> {
        let Some(base) = self.service_url.as_deref() else {
            return Ok(RevisionStreamEnd::Unsupported);
        };
        let endpoint = format!("{}/governance-config/events", base.trim_end_matches('/'));
        let response = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|error| GhError::service(format!("building revision client: {error}")))?
            .get(&endpoint)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .bearer_auth(&session.token)
            .send()
            .map_err(|error| GhError::service(format!("connecting to {endpoint}: {error}")))?;

        if matches!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND
                | reqwest::StatusCode::METHOD_NOT_ALLOWED
                | reqwest::StatusCode::NOT_IMPLEMENTED
        ) {
            return Ok(RevisionStreamEnd::Unsupported);
        }
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(GhError::service(
                "service rejected the revision-event session token".to_string(),
            ));
        }
        if !response.status().is_success() {
            return Err(GhError::service(format!(
                "service returned {} for governance revision events",
                response.status()
            )));
        }

        parse_revision_stream(BufReader::new(response), on_revision)?;
        Ok(RevisionStreamEnd::Disconnected)
    }

    /// Download an organization-scoped immutable package artifact using the
    /// current harness session. The control plane returns a fresh short-lived
    /// object-store request, so neither provider credentials nor signed URLs
    /// are persisted in governance configuration.
    pub fn download_package_artifact(
        &self,
        session: &Session,
        artifact_id: &str,
    ) -> Result<Vec<u8>, GhError> {
        const MAX_PACKAGE_BYTES: u64 = 100 * 1024 * 1024;
        let base = self.service_url.as_deref().ok_or_else(|| {
            GhError::service("managed package artifacts require an HTTP control service")
        })?;
        let endpoint = reqwest::Url::parse(base)
            .and_then(|url| url.join(&format!("package-artifacts/{artifact_id}/download")))
            .map_err(|error| GhError::config(format!("invalid service URL: {error}")))?;
        #[derive(Deserialize)]
        struct DownloadRequest {
            url: String,
            method: String,
            #[serde(default)]
            headers: std::collections::BTreeMap<String, String>,
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|error| GhError::service(format!("building package client: {error}")))?;
        let response = client
            .post(endpoint)
            .bearer_auth(&session.token)
            .send()
            .map_err(|error| GhError::service(format!("requesting package artifact: {error}")))?;
        if !response.status().is_success() {
            return Err(GhError::service(format!(
                "requesting package artifact: HTTP {}",
                response.status()
            )));
        }
        let request: DownloadRequest = response.json().map_err(|error| {
            GhError::service(format!("invalid package download response: {error}"))
        })?;
        if request.method != "GET" {
            return Err(GhError::service(
                "control service returned an unsupported package download method",
            ));
        }
        let mut download = client.get(&request.url);
        for (name, value) in request.headers {
            download = download.header(name, value);
        }
        let response = download
            .send()
            .map_err(|error| GhError::service(format!("downloading package artifact: {error}")))?;
        if !response.status().is_success() {
            return Err(GhError::service(format!(
                "downloading package artifact: HTTP {}",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PACKAGE_BYTES)
        {
            return Err(GhError::service(
                "package artifact exceeds the 100 MiB limit",
            ));
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_PACKAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| GhError::service(format!("reading package artifact: {error}")))?;
        if bytes.len() as u64 > MAX_PACKAGE_BYTES {
            return Err(GhError::service(
                "package artifact exceeds the 100 MiB limit",
            ));
        }
        Ok(bytes)
    }

    /// Fetch fresh config and update the cache. `now` is unix seconds (injected
    /// for testability).
    pub fn fetch(&self, session: &Session, now: i64) -> Result<GovernanceConfig, GhError> {
        let config = self.source.fetch(session)?;
        cache::save(&config, now)?;
        Ok(config)
    }

    /// Obtain config for a `blue run`: try the live source; on failure fall
    /// back to a *fresh* cache. Fails closed only when the cache is absent, or
    /// stale-and-`required`.
    pub fn fetch_or_cached(
        &self,
        session: &Session,
        now: i64,
    ) -> Result<GovernanceConfig, GhError> {
        match self.fetch(session, now) {
            Ok(cfg) => Ok(cfg),
            // Authentication, authorization, and action-required failures
            // describe something only the user or administrator can fix.
            // Falling back to cache here would hide the rejection.
            Err(
                error @ (GhError::Config(_)
                | GhError::Serde(_)
                | GhError::Unauthorized(_)
                | GhError::Forbidden(_)
                | GhError::ActionRequired(_)),
            ) => Err(error),
            Err(fetch_err) => match cache::load() {
                Err(cache_err) => Err(GhError::service(format!(
                    "live governance fetch failed ({fetch_err}); cached governance-config could not be read ({cache_err})"
                ))),
                Ok(Some(CachedConfig { config, fetched_at })) => {
                    // A cache written by a newer client must not bypass the
                    // current binary's contract/capability gate after a
                    // downgrade.
                    config.ensure_client_compatible().map_err(|reason| {
                        GhError::config(format!(
                            "cached governance-config is unsupported by this client: {reason}"
                        ))
                    })?;
                    // The cache deliberately carries no inference JWT, so a
                    // gateway-mode config read from it can never be launched.
                    // Fail closed rather than dropping to direct mode, which
                    // would silently bill the user's own provider credentials.
                    if config
                        .gateway
                        .as_ref()
                        .is_some_and(|gateway| gateway.token.is_none())
                    {
                        return Err(GhError::Unauthorized(
                            "gateway mode requires a live control service; the cached \
                             configuration cannot carry an inference token (run `blue login`)"
                                .to_string(),
                        ));
                    }
                    let age = now.saturating_sub(fetched_at);
                    if config.required && (age as u64) >= config.ttl_seconds() {
                        Err(GhError::service(format!(
                            "governance is required but the service is unreachable and the \
                             cached config is stale ({age}s old): {fetch_err}"
                        )))
                    } else {
                        tracing::warn!(
                            error = %fetch_err,
                            age_secs = age,
                            "service unreachable; using cached governance-config (fail-soft)"
                        );
                        Ok(config)
                    }
                }
                Ok(None) => Err(GhError::service(format!(
                    "service unreachable and no cached governance-config: {fetch_err}"
                ))),
            },
        }
    }
}

fn parse_revision_stream(
    reader: impl BufRead,
    mut on_revision: impl FnMut(String),
) -> Result<(), GhError> {
    let mut event_name = String::new();
    let mut data = String::new();
    for line in reader.lines() {
        let line = line.map_err(|error| {
            GhError::service(format!("reading governance revision events: {error}"))
        })?;
        if line.is_empty() {
            if event_name == "revision" && !data.is_empty() {
                #[derive(Deserialize)]
                struct Payload {
                    revision: String,
                }
                let payload: Payload = serde_json::from_str(&data).map_err(|error| {
                    GhError::service(format!("invalid governance revision event: {error}"))
                })?;
                if !payload.revision.is_empty() {
                    on_revision(payload.revision);
                }
            }
            event_name.clear();
            data.clear();
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event_name = value.trim_start().to_owned();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    Ok(())
}

/// Complete authentication and resolve the service identity without writing
/// `session.json`.
pub fn authenticate(cfg: &BlueToml) -> Result<Session, GhError> {
    let mut session = match &cfg.identity {
        gh_common::client_config::IdentityConfig::Oidc {
            issuer,
            client_id,
            scopes,
        } => {
            if !cfg.has_http_service() {
                return Err(GhError::config("OIDC login requires [service].url"));
            }
            identity::device_login(issuer, client_id, scopes, &cfg.service.url)?
        }
        _ => identity::provider_from_config(&cfg.identity).login()?,
    };
    if cfg.has_http_service() {
        let endpoint = reqwest::Url::parse(&cfg.service.url)
            .and_then(|url| url.join("auth/me"))
            .map_err(|error| GhError::config(format!("invalid service URL: {error}")))?;
        #[derive(Deserialize)]
        struct Me {
            sub: String,
            email: String,
            org_id: String,
            role: String,
            expires_at: i64,
        }
        let response = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|error| GhError::service(format!("building identity client: {error}")))?
            .get(endpoint)
            .bearer_auth(&session.token)
            .send()
            .map_err(|error| GhError::service(format!("identity request failed: {error}")))?;
        if !response.status().is_success() {
            return Err(GhError::service(format!(
                "login rejected by service ({})",
                response.status()
            )));
        }
        let me: Me = response
            .json()
            .map_err(|error| GhError::service(format!("invalid identity response: {error}")))?;
        session.sub = Some(me.sub);
        session.email = Some(me.email);
        session.org_id = Some(me.org_id);
        session.groups = if me.role == "admin" {
            vec!["admin".into()]
        } else {
            vec![]
        };
        session.expires_at = Some(me.expires_at);
    }
    Ok(session)
}

/// Log in using the configured identity provider and persist the session.
pub fn login(cfg: &BlueToml) -> Result<Session, GhError> {
    let session = authenticate(cfg)?;
    session.save()?;
    Ok(session)
}

/// Load the persisted session, erroring with a helpful hint if absent.
pub fn require_session() -> Result<Session, GhError> {
    Session::load()?.ok_or_else(|| GhError::other("not logged in — run `blue login` first"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::test_support::with_cache_home;
    use crate::schema::GatewayConfig;

    struct FailingSource(fn() -> GhError);

    impl ConfigSource for FailingSource {
        fn fetch(&self, _session: &Session) -> Result<GovernanceConfig, GhError> {
            Err((self.0)())
        }
        fn describe(&self) -> String {
            "failing".to_string()
        }
    }

    fn governance_only() -> GovernanceConfig {
        serde_json::from_str(r#"{"revision":"r1","allowed_harnesses":["codex"]}"#).unwrap()
    }

    fn gateway_mode() -> GovernanceConfig {
        let mut config = governance_only();
        config.gateway = Some(GatewayConfig {
            kind: "litellm".into(),
            proxy_url: Some("https://proxy.example/inference/".into()),
            token: Some("header.payload.signature".into()),
            auth_style: "bearer".into(),
        });
        config
    }

    fn client(error: fn() -> GhError) -> ServiceClient {
        ServiceClient::with_source(Box::new(FailingSource(error)))
    }

    #[test]
    fn unauthorized_is_never_served_from_cache() {
        let _guard = with_cache_home("client-unauthorized");
        cache::save(&governance_only(), 100).unwrap();
        let error = client(|| GhError::unauthorized("expired"))
            .fetch_or_cached(&Session::bearer("t"), 101)
            .unwrap_err();
        assert!(matches!(error, GhError::Unauthorized(_)), "{error:?}");
    }

    #[test]
    fn action_required_is_never_served_from_cache() {
        let _guard = with_cache_home("client-action-required");
        cache::save(&governance_only(), 100).unwrap();
        let error = client(|| GhError::action_required("run `blue gateway`"))
            .fetch_or_cached(&Session::bearer("t"), 101)
            .unwrap_err();
        assert!(matches!(error, GhError::ActionRequired(_)), "{error:?}");
    }

    #[test]
    fn forbidden_is_never_served_from_cache() {
        let _guard = with_cache_home("client-forbidden");
        cache::save(&governance_only(), 100).unwrap();
        let error = client(|| GhError::forbidden("account is not provisioned"))
            .fetch_or_cached(&Session::bearer("t"), 101)
            .unwrap_err();
        assert!(matches!(error, GhError::Forbidden(_)), "{error:?}");
    }

    #[test]
    fn an_unreachable_service_still_fails_soft_to_a_fresh_cache() {
        let _guard = with_cache_home("client-fail-soft");
        cache::save(&governance_only(), 100).unwrap();
        let config = client(|| GhError::service("connection refused"))
            .fetch_or_cached(&Session::bearer("t"), 101)
            .unwrap();
        assert_eq!(config.revision, "r1");
    }

    #[test]
    fn a_cached_gateway_config_without_a_token_fails_closed() {
        let _guard = with_cache_home("client-gateway-closed");
        // save() strips the token, so this is what every gateway-mode cache
        // looks like on disk.
        cache::save(&gateway_mode(), 100).unwrap();
        let error = client(|| GhError::service("connection refused"))
            .fetch_or_cached(&Session::bearer("t"), 101)
            .unwrap_err();
        assert!(matches!(error, GhError::Unauthorized(_)), "{error:?}");
        assert!(error.to_string().contains("blue login"), "{error}");
    }

    #[test]
    fn parses_revision_events_and_ignores_heartbeats_and_other_events() {
        let body = concat!(
            ": keepalive\n\n",
            "event: revision\n",
            "id: r2\n",
            "data: {\"revision\":\"r2\"}\n\n",
            "event: unrelated\n",
            "data: {\"revision\":\"ignored\"}\n\n",
            "event: revision\n",
            "data: {\"revision\":\"r3\"}\n\n",
        );
        let mut revisions = Vec::new();
        parse_revision_stream(std::io::Cursor::new(body), |revision| {
            revisions.push(revision)
        })
        .unwrap();
        assert_eq!(revisions, ["r2", "r3"]);
    }

    #[test]
    fn rejects_invalid_revision_payloads() {
        let body = "event: revision\ndata: not-json\n\n";
        let error = parse_revision_stream(std::io::Cursor::new(body), |_| {}).unwrap_err();
        assert!(error
            .to_string()
            .contains("invalid governance revision event"));
    }
}
