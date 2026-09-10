//! Streaming inference proxy for gateway mode.
//!
//! Session-bound credential resolutions are cached briefly and invalidated through the
//! Control API event stream. Request metadata is delivered in bounded batches.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{mpsc, OwnedSemaphorePermit, RwLock, Semaphore};
use uuid::Uuid;

#[derive(Clone)]
struct Mapping {
    upstream_credential: String,
    user: String,
    user_id: Option<String>,
    organization_id: Option<String>,
    profile_id: Option<String>,
    profile_name: Option<String>,
    credential_version: Option<String>,
    credential_expires_at: Option<OffsetDateTime>,
    gateway_session_expires_at: Option<OffsetDateTime>,
    /// Instant the backing session was last reactivated after a CLI logout.
    /// Tokens issued before it are rejected.
    session_not_before: Option<OffsetDateTime>,
}

struct CacheEntry {
    mapping: Mapping,
    expires_at: Instant,
}

struct CredentialCache {
    entries: HashMap<String, CacheEntry>,
    order: VecDeque<String>,
    capacity: usize,
    ttl: Duration,
}

impl CredentialCache {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
            ttl,
        }
    }

    fn get(&mut self, cache_key: &str) -> Option<Mapping> {
        let valid = self.entries.get(cache_key).is_some_and(|entry| {
            entry.expires_at > Instant::now()
                && entry
                    .mapping
                    .credential_expires_at
                    .is_none_or(|expires| expires > OffsetDateTime::now_utc())
                && entry
                    .mapping
                    .gateway_session_expires_at
                    .is_none_or(|expires| expires > OffsetDateTime::now_utc())
        });
        if !valid {
            self.entries.remove(cache_key);
            return None;
        }
        self.order.retain(|key| key != cache_key);
        self.order.push_back(cache_key.to_owned());
        self.entries
            .get(cache_key)
            .map(|entry| entry.mapping.clone())
    }

    fn insert(&mut self, cache_key: String, mapping: Mapping) {
        self.order.retain(|key| key != &cache_key);
        self.order.push_back(cache_key.clone());
        self.entries.insert(
            cache_key,
            CacheEntry {
                mapping,
                expires_at: Instant::now() + self.ttl,
            },
        );
        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn invalidate(&mut self, oauth_session_id: Option<&str>, user_id: &str) {
        if let Some(oauth_session_id) = oauth_session_id {
            let cache_key = format!("{oauth_session_id}:{user_id}");
            self.entries.remove(&cache_key);
            self.order.retain(|key| key != &cache_key);
        } else {
            self.entries
                .retain(|_, entry| entry.mapping.user_id.as_deref() != Some(user_id));
            self.order.retain(|key| self.entries.contains_key(key));
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

#[derive(Default)]
struct Metrics {
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    invalidation_events: AtomicU64,
    invalidation_disconnects: AtomicU64,
    cache_bypasses: AtomicU64,
    resolver_errors: AtomicU64,
    rate_limited_unverified: AtomicU64,
    rate_limited_session: AtomicU64,
    jwks_fetches: AtomicU64,
    jwks_unknown_kid: AtomicU64,
    log_dropped: AtomicU64,
    active_streams: AtomicU64,
    oauth_token_fetches: AtomicU64,
    oauth_token_fetch_errors: AtomicU64,
    tls_reload_successes: AtomicU64,
    tls_reload_errors: AtomicU64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InternalTransportMode {
    Mtls,
    InsecureHttp,
}

impl InternalTransportMode {
    fn from_env() -> Self {
        match std::env::var("HARNESS_INTERNAL_TRANSPORT_MODE")
            .unwrap_or_else(|_| "mtls".into())
            .as_str()
        {
            "mtls" => Self::Mtls,
            "insecure-http" => Self::InsecureHttp,
            value => panic!(
                "HARNESS_INTERNAL_TRANSPORT_MODE must be `mtls` or `insecure-http`, got `{value}`"
            ),
        }
    }
}

/// Static configuration for the OAuth2 client-credentials (M2M) flow the proxy
/// uses to authenticate to the Control API's internal gateway endpoints.
#[derive(Clone)]
struct OauthConfig {
    token_url: String,
    client_id: String,
    client_secret: String,
    scope: String,
    /// The Control API audience, sent as the RFC 8707 `resource` parameter so
    /// better-auth mints a token whose `aud` matches what the Control API checks.
    resource: String,
    /// Refresh this many seconds before expiry. When unset the proxy refreshes
    /// at ~80% of the token's lifetime.
    refresh_skew: Option<Duration>,
}

/// An in-process cached service token. `refresh_at` is when the background
/// worker / hot path proactively re-fetches; `exp` is the hard expiry after
/// which the token must not be used.
struct CachedServiceToken {
    token: String,
    refresh_at: Instant,
    exp: Instant,
}

struct RateWindow {
    updated: Instant,
    tokens: f64,
}

type RequestPermit = Arc<OwnedSemaphorePermit>;

struct AppState {
    gateway: &'static dyn gh_gateway::GatewayAdapter,
    upstream_credential_header: HeaderName,
    upstream_base: String,
    map: HashMap<String, Mapping>,
    static_key: Option<String>,
    resolver_url: Option<String>,
    gateway_jwks_url: Option<String>,
    gateway_jwt_issuer: Option<String>,
    gateway_jwt_audience: Option<String>,
    gateway_jwks: RwLock<Option<CachedGatewayJwks>>,
    event_url: Option<String>,
    oauth: Option<OauthConfig>,
    service_token: RwLock<Option<CachedServiceToken>>,
    token_refresh_lock: tokio::sync::Mutex<()>,
    upstream_client: reqwest::Client,
    oauth_client: reqwest::Client,
    control_client: RwLock<reqwest::Client>,
    resolver_timeout: Duration,
    cache: Mutex<CredentialCache>,
    resolution_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    rates: Mutex<HashMap<String, RateWindow>>,
    per_token_rps: u64,
    per_token_burst: u64,
    /// Budget for tokens that fail signature verification, charged per peer
    /// address. Legitimate traffic never touches it.
    unverified_rps: u64,
    jwks_refetch_cooldown: Duration,
    resolver_permits: Arc<Semaphore>,
    request_permits: Arc<Semaphore>,
    max_body_bytes: usize,
    log_tx: mpsc::Sender<RequestLogEvent>,
    invalidation_tx: mpsc::Sender<InvalidCredentialReport>,
    metrics: Metrics,
    invalidation_healthy: AtomicBool,
    ready: AtomicBool,
}

struct CachedGatewayJwks {
    set: JwkSet,
    fetched_at: Instant,
    /// When the JWKS endpoint was last called, successfully or not. Bounds how
    /// often an unknown kid can make us call it again.
    last_attempt: Instant,
}

impl CachedGatewayJwks {
    fn key(&self, kid: &str) -> Option<&Jwk> {
        self.set
            .keys
            .iter()
            .find(|key| key.common.key_id.as_deref() == Some(kid))
    }
}

const GATEWAY_JWKS_TTL: Duration = Duration::from_secs(300);

#[derive(Deserialize, Serialize)]
struct GatewayInferenceClaims {
    #[allow(dead_code)]
    iss: String,
    #[allow(dead_code)]
    aud: String,
    sub: String,
    iat: i64,
    exp: i64,
    jti: String,
    blue_oauth_session_id: String,
    scope: String,
}

struct GatewayIdentity {
    user_id: Uuid,
    oauth_session_id: String,
    /// Signature-verified `iat`, compared against the session's
    /// `session_not_before`. Deliberately **not** part of `cache_key`: every
    /// JWT for a session would otherwise get its own cache entry.
    issued_at: i64,
}

impl GatewayIdentity {
    fn cache_key(&self) -> String {
        format!("{}:{}", self.oauth_session_id, self.user_id)
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}
fn env_u64(name: &str, default: u64) -> u64 {
    env_usize(name, default as usize) as u64
}

fn select_gateway_adapter(
    kind: Option<&str>,
) -> Result<&'static dyn gh_gateway::GatewayAdapter, String> {
    let kind = kind
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "HARNESS_GATEWAY_TYPE is required".to_owned())?;
    gh_gateway::gateway_adapter(kind).ok_or_else(|| {
        format!(
            "unsupported HARNESS_GATEWAY_TYPE `{kind}` (compiled: {})",
            gh_gateway::supported_gateway_types().join(", ")
        )
    })
}

fn validate_gateway_adapter(
    adapter: &dyn gh_gateway::GatewayAdapter,
) -> Result<HeaderName, String> {
    let root_path = adapter
        .upstream_path("/")
        .map_err(|error| error.to_string())?;
    if !root_path.starts_with('/') {
        return Err(format!(
            "gateway adapter `{}` returned a non-absolute upstream path `{root_path}`",
            adapter.kind()
        ));
    }

    upstream_credential_header(adapter.upstream_credential_placement()).map_err(|error| {
        format!(
            "gateway adapter `{}` declares invalid upstream credential placement: {error}",
            adapter.kind()
        )
    })
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("INFERENCE_PROXY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    validate_required_gateway_env(|name| std::env::var(name).ok())
        .unwrap_or_else(|error| panic!("{error}"));

    let gateway_kind = std::env::var("HARNESS_GATEWAY_TYPE").ok();
    let gateway =
        select_gateway_adapter(gateway_kind.as_deref()).unwrap_or_else(|error| panic!("{error}"));
    let upstream_credential_header =
        validate_gateway_adapter(gateway).unwrap_or_else(|error| panic!("{error}"));
    let upstream_base = std::env::var("HARNESS_GATEWAY_URL")
        .or_else(|_| std::env::var("HARNESS_LITELLM_BASE_URL"))
        .expect("HARNESS_GATEWAY_URL is required")
        .trim_end_matches('/')
        .to_owned();
    let static_key = std::env::var("HARNESS_STATIC_VIRTUAL_KEY").ok();
    let resolver_url = std::env::var("HARNESS_GATEWAY_RESOLVER_URL").ok();
    let gateway_jwks_url = std::env::var("HARNESS_GATEWAY_JWKS_URL").ok();
    let gateway_jwt_issuer = std::env::var("HARNESS_GATEWAY_JWT_ISSUER").ok();
    let gateway_jwt_audience = std::env::var("HARNESS_GATEWAY_JWT_AUDIENCE").ok();
    let log_url = std::env::var("HARNESS_GATEWAY_LOG_URL").ok();
    let event_url = std::env::var("HARNESS_GATEWAY_EVENT_URL").ok().or_else(|| {
        resolver_url
            .as_deref()
            .and_then(|url| url.strip_suffix("/resolve"))
            .map(|base| format!("{base}/events"))
    });
    let credential_invalid_url = std::env::var("HARNESS_GATEWAY_CREDENTIAL_INVALID_URL")
        .ok()
        .or_else(|| {
            resolver_url
                .as_deref()
                .and_then(|url| url.strip_suffix("/resolve"))
                .map(|base| format!("{base}/credential-invalid"))
        });
    let oauth = oauth_config_from_env();
    let internal_transport = InternalTransportMode::from_env();
    if internal_transport == InternalTransportMode::InsecureHttp
        && [
            "HARNESS_INTERNAL_CA_PEM",
            "HARNESS_INTERNAL_CA_FILE",
            "HARNESS_PROXY_CLIENT_IDENTITY_PEM",
            "HARNESS_PROXY_CLIENT_IDENTITY_FILE",
        ]
        .iter()
        .any(|name| std::env::var(name).is_ok())
    {
        panic!("internal TLS settings must not be configured in insecure-http mode");
    }
    // Hard cutover: any Control API hop (resolve / events / request-logs /
    // credential-invalid) now requires the M2M OAuth flow. Fail fast if it is
    // not configured.
    let dynamic_mode = control_api_hop_configured(
        resolver_url.as_ref(),
        event_url.as_ref(),
        log_url.as_ref(),
        credential_invalid_url.as_ref(),
    );
    if dynamic_mode && oauth.is_none() {
        panic!(
            "HARNESS_PROXY_OAUTH_TOKEN_URL (and HARNESS_PROXY_OAUTH_CLIENT_ID/\
             HARNESS_PROXY_OAUTH_CLIENT_SECRET/HARNESS_PROXY_OAUTH_RESOURCE) are required \
             when a Control API resolver/event/log/credential-invalid URL is configured"
        );
    }
    let map = load_map();
    let explicit_local_development =
        std::env::var("BLUE_ALLOW_INSECURE_DEV").as_deref() == Ok("true");
    if dynamic_mode && (static_key.is_some() || !map.is_empty()) {
        panic!("static gateway routing cannot be combined with the production resolver path");
    }
    if !dynamic_mode && !explicit_local_development {
        panic!("HARNESS_GATEWAY_RESOLVER_URL is required outside explicit local development");
    }
    let listen = std::env::var("HARNESS_LISTEN").unwrap_or_else(|_| "0.0.0.0:8081".into());
    validate_urls(
        ProxyUrls {
            gateway: &upstream_base,
            gateway_jwks: gateway_jwks_url.as_deref(),
            resolver: resolver_url.as_deref(),
            logs: log_url.as_deref(),
            events: event_url.as_deref(),
            credential_invalid: credential_invalid_url.as_deref(),
            token: oauth.as_ref().map(|oauth| oauth.token_url.as_str()),
        },
        internal_transport,
    );

    let upstream_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(env_u64(
            "HARNESS_PROXY_UPSTREAM_CONNECT_TIMEOUT_MS",
            2_000,
        )))
        .build()
        .expect("building gateway client");
    let oauth_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .build()
        .expect("building OAuth client");
    let control_client = build_control_client(if dynamic_mode {
        internal_transport
    } else {
        InternalTransportMode::InsecureHttp
    })
    .unwrap_or_else(|error| panic!("building Control API client: {error}"));
    let (log_tx, log_rx) = mpsc::channel(env_usize("HARNESS_PROXY_LOG_QUEUE_CAPACITY", 10_000));
    let (invalidation_tx, invalidation_rx) = mpsc::channel(env_usize(
        "HARNESS_PROXY_INVALIDATION_QUEUE_CAPACITY",
        1_000,
    ));
    let state = Arc::new(AppState {
        gateway,
        upstream_credential_header,
        upstream_base,
        map,
        static_key,
        resolver_url,
        gateway_jwks_url,
        gateway_jwt_issuer,
        gateway_jwt_audience,
        gateway_jwks: RwLock::new(None),
        event_url,
        oauth,
        service_token: RwLock::new(None),
        token_refresh_lock: tokio::sync::Mutex::new(()),
        upstream_client,
        oauth_client,
        control_client: RwLock::new(control_client),
        resolver_timeout: Duration::from_secs(env_u64("HARNESS_PROXY_RESOLVER_TIMEOUT_SECONDS", 3)),
        cache: Mutex::new(CredentialCache::new(
            env_usize("HARNESS_PROXY_CACHE_CAPACITY", 100_000),
            Duration::from_secs(env_u64("HARNESS_PROXY_CACHE_TTL_SECONDS", 60)),
        )),
        resolution_locks: Mutex::new(HashMap::new()),
        rates: Mutex::new(HashMap::new()),
        per_token_rps: env_u64("HARNESS_PROXY_PER_TOKEN_RPS", 50),
        per_token_burst: env_u64("HARNESS_PROXY_PER_TOKEN_BURST", 100),
        unverified_rps: env_u64("HARNESS_PROXY_UNVERIFIED_RPS", 20),
        jwks_refetch_cooldown: Duration::from_secs(env_u64(
            "HARNESS_PROXY_JWKS_REFETCH_COOLDOWN",
            30,
        )),
        resolver_permits: Arc::new(Semaphore::new(env_usize(
            "HARNESS_PROXY_MAX_RESOLVER_CONCURRENCY",
            64,
        ))),
        request_permits: Arc::new(Semaphore::new(env_usize(
            "HARNESS_PROXY_MAX_IN_FLIGHT",
            7_500,
        ))),
        max_body_bytes: env_usize("HARNESS_PROXY_MAX_BODY_BYTES", 32 * 1024 * 1024),
        log_tx,
        invalidation_tx,
        metrics: Metrics::default(),
        invalidation_healthy: AtomicBool::new(!dynamic_mode),
        ready: AtomicBool::new(true),
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    if dynamic_mode
        && internal_transport == InternalTransportMode::Mtls
        && std::env::var("HARNESS_INTERNAL_CA_FILE").is_ok()
        && std::env::var("HARNESS_PROXY_CLIENT_IDENTITY_FILE").is_ok()
    {
        tokio::spawn(control_tls_reload_worker(
            state.clone(),
            shutdown_rx.clone(),
        ));
    }

    // Hard cutover: obtain the first service token before serving traffic so a
    // misconfigured M2M flow fails loudly rather than 503-ing later. Retry with
    // a generous deadline so a still-starting auth dependency (DB migrations,
    // dashboard cold start, client seed) doesn't trip the fail-fast.
    if state.oauth.is_some() {
        let startup_deadline_secs = env_u64("HARNESS_PROXY_OAUTH_STARTUP_DEADLINE_SECONDS", 120);
        let deadline = Instant::now() + Duration::from_secs(startup_deadline_secs);
        let mut obtained = false;
        let mut attempt: u32 = 0;
        while Instant::now() < deadline {
            if get_valid_token(&state).await.is_some() {
                obtained = true;
                break;
            }
            attempt += 1;
            tracing::warn!(
                attempt,
                "initial OAuth service token fetch failed; retrying in 2s"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if !obtained {
            panic!(
                "unable to obtain an initial OAuth service token from the Control API token endpoint within {startup_deadline_secs}s"
            );
        }
        tokio::spawn(token_refresh_worker(state.clone(), shutdown_rx.clone()));
    }

    let log_handle = tokio::spawn(log_worker(
        state.clone(),
        log_rx,
        log_url,
        shutdown_rx.clone(),
    ));
    tokio::spawn(invalid_credential_worker(
        state.clone(),
        invalidation_rx,
        credential_invalid_url,
        shutdown_rx.clone(),
    ));
    if state.event_url.is_some() && state.oauth.is_some() {
        tokio::spawn(invalidation_worker(state.clone(), shutdown_rx.clone()));
    }
    tokio::spawn({
        let state = state.clone();
        async move {
            shutdown_signal().await;
            state.ready.store(false, Ordering::Release);
            let _ = shutdown_tx.send(true);
        }
    });

    tracing::info!(gateway = state.gateway.kind(), upstream = %state.upstream_base, %listen, "inference-proxy starting");
    let protected = Router::new()
        .fallback(proxy)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            proxy_auth_guard,
        ));
    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .merge(protected)
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|error| panic!("binding {listen}: {error}"));
    let mut server_shutdown = shutdown_rx;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = server_shutdown.changed().await;
    })
    .await
    .expect("server error");
    let _ = tokio::time::timeout(Duration::from_secs(10), log_handle).await;
}

fn build_control_client(mode: InternalTransportMode) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if mode == InternalTransportMode::Mtls {
        let pem = pem_setting("HARNESS_INTERNAL_CA_PEM", "HARNESS_INTERNAL_CA_FILE").ok_or_else(
            || {
                anyhow::anyhow!(
                    "HARNESS_INTERNAL_CA_FILE or HARNESS_INTERNAL_CA_PEM is required in mtls mode"
                )
            },
        )?;
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes())?);
        let pem = pem_setting(
            "HARNESS_PROXY_CLIENT_IDENTITY_PEM",
            "HARNESS_PROXY_CLIENT_IDENTITY_FILE",
        ).ok_or_else(|| anyhow::anyhow!("HARNESS_PROXY_CLIENT_IDENTITY_FILE or HARNESS_PROXY_CLIENT_IDENTITY_PEM is required in mtls mode"))?;
        builder = builder.identity(reqwest::Identity::from_pem(pem.as_bytes())?);
    }
    Ok(builder
        .connect_timeout(Duration::from_millis(500))
        .build()?)
}

async fn control_tls_reload_worker(
    state: Arc<AppState>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut fingerprint = tls_file_fingerprint();
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.tick().await;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = interval.tick() => {
                let next = tls_file_fingerprint();
                if next.is_none() {
                    state.metrics.tls_reload_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("unable to read Control API mTLS certificate material; retaining last-known-good client");
                } else if next != fingerprint {
                    match build_control_client(InternalTransportMode::Mtls) {
                        Ok(client) => {
                            *state.control_client.write().await = client;
                            fingerprint = next;
                            state.metrics.tls_reload_successes.fetch_add(1, Ordering::Relaxed);
                            tracing::info!("reloaded Control API mTLS certificate material");
                        }
                        Err(error) => {
                            state.metrics.tls_reload_errors.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(%error, "failed to reload Control API mTLS certificate material; retaining last-known-good client");
                        }
                    }
                }
            }
        }
    }
}

fn tls_file_fingerprint() -> Option<[u8; 32]> {
    let ca = std::fs::read(std::env::var("HARNESS_INTERNAL_CA_FILE").ok()?).ok()?;
    let identity = std::fs::read(std::env::var("HARNESS_PROXY_CLIENT_IDENTITY_FILE").ok()?).ok()?;
    let mut hash = Sha256::new();
    hash.update(ca);
    hash.update(identity);
    Some(hash.finalize().into())
}

fn pem_setting(value_name: &str, file_name: &str) -> Option<String> {
    std::env::var(value_name).ok().or_else(|| {
        std::env::var(file_name).ok().map(|path| {
            std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("reading {file_name} {path}: {error}"))
        })
    })
}

fn control_api_hop_configured(
    resolver: Option<&String>,
    events: Option<&String>,
    logs: Option<&String>,
    credential_invalid: Option<&String>,
) -> bool {
    resolver.is_some() || events.is_some() || logs.is_some() || credential_invalid.is_some()
}

/// Validate the proxy's required startup envelope in one pass. The returned
/// error contains environment-variable names only, never configured values.
fn validate_required_gateway_env(get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    let present = |name: &str| get(name).is_some_and(|value| !value.trim().is_empty());
    let mut missing = Vec::new();
    for name in ["HARNESS_GATEWAY_TYPE", "HARNESS_GATEWAY_URL"] {
        if !present(name) {
            missing.push(name);
        }
    }

    let dynamic = [
        "HARNESS_GATEWAY_RESOLVER_URL",
        "HARNESS_GATEWAY_EVENT_URL",
        "HARNESS_GATEWAY_LOG_URL",
        "HARNESS_GATEWAY_CREDENTIAL_INVALID_URL",
    ]
    .iter()
    .any(|name| present(name));
    if dynamic {
        for name in [
            "HARNESS_PROXY_OAUTH_TOKEN_URL",
            "HARNESS_PROXY_OAUTH_CLIENT_ID",
            "HARNESS_PROXY_OAUTH_CLIENT_SECRET",
            "HARNESS_PROXY_OAUTH_RESOURCE",
            "HARNESS_GATEWAY_JWKS_URL",
            "HARNESS_GATEWAY_JWT_ISSUER",
            "HARNESS_GATEWAY_JWT_AUDIENCE",
        ] {
            if !present(name) {
                missing.push(name);
            }
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "invalid inference-proxy environment:\n{}",
            missing
                .into_iter()
                .map(|name| format!("- {name} is required"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

/// Builds the OAuth2 client-credentials configuration from the environment.
/// Returns `None` when no token URL is set (static-key / no-resolver mode);
/// panics if the token URL is set but a required companion value is missing.
fn oauth_config_from_env() -> Option<OauthConfig> {
    let token_url = std::env::var("HARNESS_PROXY_OAUTH_TOKEN_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    let client_id = std::env::var("HARNESS_PROXY_OAUTH_CLIENT_ID").expect(
        "HARNESS_PROXY_OAUTH_CLIENT_ID is required when HARNESS_PROXY_OAUTH_TOKEN_URL is set",
    );
    let client_secret = std::env::var("HARNESS_PROXY_OAUTH_CLIENT_SECRET").expect(
        "HARNESS_PROXY_OAUTH_CLIENT_SECRET is required when HARNESS_PROXY_OAUTH_TOKEN_URL is set",
    );
    let resource = std::env::var("HARNESS_PROXY_OAUTH_RESOURCE").expect(
        "HARNESS_PROXY_OAUTH_RESOURCE is required when HARNESS_PROXY_OAUTH_TOKEN_URL is set",
    );
    let scope =
        std::env::var("HARNESS_PROXY_OAUTH_SCOPE").unwrap_or_else(|_| "gateway:resolve".into());
    let refresh_skew = std::env::var("HARNESS_PROXY_OAUTH_REFRESH_SKEW_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);
    Some(OauthConfig {
        token_url,
        client_id,
        client_secret,
        scope,
        resource,
        refresh_skew,
    })
}

struct ProxyUrls<'a> {
    gateway: &'a str,
    gateway_jwks: Option<&'a str>,
    resolver: Option<&'a str>,
    logs: Option<&'a str>,
    events: Option<&'a str>,
    credential_invalid: Option<&'a str>,
    token: Option<&'a str>,
}

fn validate_urls(urls: ProxyUrls<'_>, internal_transport: InternalTransportMode) {
    let allow_insecure = std::env::var("BLUE_ALLOW_INSECURE_DEV").as_deref() == Ok("true");
    for (name, url) in [
        ("gateway", Some(urls.gateway)),
        ("gateway-jwks", urls.gateway_jwks),
        ("resolver", urls.resolver),
        ("request-log", urls.logs),
        ("events", urls.events),
        ("credential-invalidation", urls.credential_invalid),
        ("token", urls.token),
    ] {
        if let Some(url) = url {
            let internal = matches!(
                name,
                "gateway-jwks" | "resolver" | "request-log" | "events" | "credential-invalidation"
            );
            if internal {
                if !internal_url_matches_transport(internal_transport, url) {
                    let scheme = match internal_transport {
                        InternalTransportMode::Mtls => "HTTPS",
                        InternalTransportMode::InsecureHttp => "HTTP",
                    };
                    panic!("{name} URL must use {scheme} in the configured internal transport mode: {url}");
                }
            } else if !allow_insecure && !url.starts_with("https://") {
                panic!("{name} URL must use HTTPS: {url}");
            }
        }
    }
}

fn internal_url_matches_transport(mode: InternalTransportMode, url: &str) -> bool {
    match mode {
        InternalTransportMode::Mtls => url.starts_with("https://"),
        InternalTransportMode::InsecureHttp => url.starts_with("http://"),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok() };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}

async fn health() -> &'static str {
    "ok"
}
async fn ready(State(state): State<Arc<AppState>>) -> Response {
    if state.ready.load(Ordering::Acquire) {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response()
    }
}
async fn metrics(State(state): State<Arc<AppState>>) -> String {
    format!(
        concat!(
            "gateway_proxy_cache_hits_total {}\n",
            "gateway_proxy_cache_misses_total {}\n",
            "gateway_proxy_invalidation_events_total {}\n",
            "gateway_proxy_invalidation_disconnects_total {}\n",
            "gateway_proxy_cache_bypasses_total {}\n",
            "gateway_proxy_invalidation_stream_healthy {}\n",
            "gateway_proxy_resolver_errors_total {}\n",
            "gateway_proxy_rate_limited_unverified_total {}\n",
            "gateway_proxy_rate_limited_session_total {}\n",
            "gateway_proxy_jwks_fetches_total {}\n",
            "gateway_proxy_jwks_unknown_kid_total {}\n",
            "gateway_proxy_log_dropped_total {}\n",
            "gateway_proxy_active_streams {}\n",
            "gateway_proxy_oauth_token_fetches_total {}\n",
            "gateway_proxy_oauth_token_fetch_errors_total {}\n",
            "gateway_proxy_tls_reload_successes_total {}\n",
            "gateway_proxy_tls_reload_errors_total {}\n"
        ),
        state.metrics.cache_hits.load(Ordering::Relaxed),
        state.metrics.cache_misses.load(Ordering::Relaxed),
        state.metrics.invalidation_events.load(Ordering::Relaxed),
        state
            .metrics
            .invalidation_disconnects
            .load(Ordering::Relaxed),
        state.metrics.cache_bypasses.load(Ordering::Relaxed),
        u8::from(state.invalidation_healthy.load(Ordering::Acquire)),
        state.metrics.resolver_errors.load(Ordering::Relaxed),
        state
            .metrics
            .rate_limited_unverified
            .load(Ordering::Relaxed),
        state.metrics.rate_limited_session.load(Ordering::Relaxed),
        state.metrics.jwks_fetches.load(Ordering::Relaxed),
        state.metrics.jwks_unknown_kid.load(Ordering::Relaxed),
        state.metrics.log_dropped.load(Ordering::Relaxed),
        state.metrics.active_streams.load(Ordering::Relaxed),
        state.metrics.oauth_token_fetches.load(Ordering::Relaxed),
        state
            .metrics
            .oauth_token_fetch_errors
            .load(Ordering::Relaxed),
        state.metrics.tls_reload_successes.load(Ordering::Relaxed),
        state.metrics.tls_reload_errors.load(Ordering::Relaxed)
    )
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// Fetches a fresh service token from the Control API's OAuth token endpoint
/// using the client-credentials grant with `client_secret_basic` auth. Returns
/// `None` on any transport/status/decode failure (the caller records the error
/// metric and decides whether to fall back to a still-valid cached token).
async fn fetch_service_token(state: &AppState) -> Option<CachedServiceToken> {
    let oauth = state.oauth.as_ref()?;
    let params = [
        ("grant_type", "client_credentials"),
        ("scope", oauth.scope.as_str()),
        ("resource", oauth.resource.as_str()),
    ];
    let response = state
        .oauth_client
        .post(&oauth.token_url)
        .basic_auth(&oauth.client_id, Some(&oauth.client_secret))
        .form(&params)
        .timeout(state.resolver_timeout)
        .send()
        .await
        .map_err(|error| tracing::warn!(%error, "fetching service token"))
        .ok()?;
    if !response.status().is_success() {
        tracing::warn!(status = %response.status(), "service token endpoint rejected the request");
        return None;
    }
    let body: TokenResponse = response
        .json()
        .await
        .map_err(|error| tracing::warn!(%error, "decoding service token response"))
        .ok()?;
    let now = Instant::now();
    let ttl = Duration::from_secs(body.expires_in.max(1));
    let refresh_after = match oauth.refresh_skew {
        Some(skew) if skew < ttl => ttl - skew,
        Some(_) => ttl / 5,
        None => ttl.mul_f64(0.8),
    }
    .max(Duration::from_millis(100));
    Some(CachedServiceToken {
        token: body.access_token,
        refresh_at: now + refresh_after,
        exp: now + ttl,
    })
}

/// Performs one token fetch, updating metrics and the cache. Returns the fresh
/// token string, or `None` if the fetch failed.
async fn refresh_service_token(state: &AppState) -> Option<String> {
    state
        .metrics
        .oauth_token_fetches
        .fetch_add(1, Ordering::Relaxed);
    match fetch_service_token(state).await {
        Some(fresh) => {
            let token = fresh.token.clone();
            *state.service_token.write().await = Some(fresh);
            Some(token)
        }
        None => {
            state
                .metrics
                .oauth_token_fetch_errors
                .fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// Hot-path accessor for the current service token. Returns the cached token
/// while it is still fresh (`now < refresh_at`); otherwise it single-flights a
/// refresh behind `token_refresh_lock` so a burst of callers straddling a
/// rollover triggers exactly one fetch. Falls back to a cached-but-unexpired
/// token if the refresh fails. Returns `None` only when M2M is unconfigured or
/// no usable token exists.
async fn get_valid_token(state: &AppState) -> Option<String> {
    state.oauth.as_ref()?;
    {
        let cached = state.service_token.read().await;
        if let Some(entry) = cached.as_ref() {
            if Instant::now() < entry.refresh_at {
                return Some(entry.token.clone());
            }
        }
    }
    let _guard = state.token_refresh_lock.lock().await;
    // Another caller may have refreshed while we waited for the lock.
    {
        let cached = state.service_token.read().await;
        if let Some(entry) = cached.as_ref() {
            if Instant::now() < entry.refresh_at {
                return Some(entry.token.clone());
            }
        }
    }
    if let Some(token) = refresh_service_token(state).await {
        return Some(token);
    }
    // Refresh failed: reuse a still-valid token if one remains.
    let cached = state.service_token.read().await;
    cached
        .as_ref()
        .filter(|entry| Instant::now() < entry.exp)
        .map(|entry| entry.token.clone())
}

/// Background worker that proactively refreshes the service token at its
/// `refresh_at` instant so the hot path virtually always just reads the cache.
async fn token_refresh_worker(
    state: Arc<AppState>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let sleep_for = {
            let cached = state.service_token.read().await;
            match cached.as_ref() {
                Some(entry) => entry.refresh_at.saturating_duration_since(Instant::now()),
                None => Duration::from_millis(200),
            }
        }
        .max(Duration::from_millis(200));
        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => {}
            _ = shutdown.changed() => return,
        }
        if *shutdown.borrow() {
            return;
        }
        // Drives the single-flight refresh once we are at/after refresh_at.
        let _ = get_valid_token(&state).await;
    }
}

#[derive(Deserialize)]
struct ResolveResponse {
    upstream_credential: String,
    user: String,
    user_id: Option<String>,
    organization_id: Option<String>,
    profile_id: Option<String>,
    profile_name: Option<String>,
    credential_version: Option<String>,
    credential_expires_at: Option<String>,
    gateway_session_expires_at: Option<String>,
    session_not_before: Option<String>,
}
#[derive(Debug)]
enum ResolveError {
    Invalid,
    Unavailable,
}

async fn gateway_jwk_for(state: &AppState, kid: &str) -> Result<Jwk, ResolveError> {
    {
        let cached = state.gateway_jwks.read().await;
        if let Some(cache) = cached.as_ref() {
            if let Some(key) = cache.key(kid) {
                if cache.fetched_at.elapsed() < GATEWAY_JWKS_TTL {
                    return Ok(key.clone());
                }
            } else if cache.last_attempt.elapsed() < state.jwks_refetch_cooldown {
                // Unknown kid against a set we just fetched. Refetching per
                // request turns a trickle of bogus kids into a JWKS flood
                // against the Control API; rotation still converges within one
                // cooldown.
                state
                    .metrics
                    .jwks_unknown_kid
                    .fetch_add(1, Ordering::Relaxed);
                return Err(ResolveError::Invalid);
            }
        }
    }
    let url = state
        .gateway_jwks_url
        .as_ref()
        .ok_or(ResolveError::Unavailable)?;
    state.metrics.jwks_fetches.fetch_add(1, Ordering::Relaxed);
    let response = if state.oauth.is_some() {
        let token = get_valid_token(state)
            .await
            .ok_or(ResolveError::Unavailable)?;
        state
            .control_client
            .read()
            .await
            .clone()
            .get(url)
            .bearer_auth(token)
            .timeout(state.resolver_timeout)
            .send()
            .await
    } else {
        // Explicit local-development mode has no Control API service identity.
        state
            .oauth_client
            .get(url)
            .timeout(state.resolver_timeout)
            .send()
            .await
    };
    let fetched = match response {
        Ok(response) => match response.error_for_status() {
            Ok(response) => response.json::<JwkSet>().await.map_err(|_| ()),
            Err(_) => Err(()),
        },
        Err(_) => Err(()),
    };
    let now = Instant::now();
    let mut cached = state.gateway_jwks.write().await;
    let Ok(set) = fetched else {
        // Record the attempt even on failure, so a broken JWKS endpoint is not
        // retried once per request.
        if let Some(cache) = cached.as_mut() {
            cache.last_attempt = now;
        }
        return Err(ResolveError::Unavailable);
    };
    // Cache the set *before* looking the kid up. Doing it the other way round
    // meant an unknown kid never populated the cache at all, so every
    // legitimate miss that followed paid for another fetch.
    let cache = cached.insert(CachedGatewayJwks {
        set,
        fetched_at: now,
        last_attempt: now,
    });
    cache.key(kid).cloned().ok_or_else(|| {
        state
            .metrics
            .jwks_unknown_kid
            .fetch_add(1, Ordering::Relaxed);
        ResolveError::Invalid
    })
}

async fn validate_gateway_token(
    state: &AppState,
    token: &str,
) -> Result<GatewayIdentity, ResolveError> {
    let header = decode_header(token).map_err(|_| ResolveError::Invalid)?;
    if header.alg != Algorithm::RS256 {
        return Err(ResolveError::Invalid);
    }
    let kid = header.kid.as_deref().ok_or(ResolveError::Invalid)?;
    let jwk = gateway_jwk_for(state, kid).await?;
    let key = DecodingKey::from_jwk(&jwk).map_err(|_| ResolveError::Invalid)?;
    let issuer = state
        .gateway_jwt_issuer
        .as_deref()
        .ok_or(ResolveError::Unavailable)?;
    let audience = state
        .gateway_jwt_audience
        .as_deref()
        .ok_or(ResolveError::Unavailable)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    let claims = decode::<GatewayInferenceClaims>(token, &key, &validation)
        .map_err(|_| ResolveError::Invalid)?
        .claims;
    if !claims
        .scope
        .split_whitespace()
        .any(|scope| scope == "gateway:infer")
        || claims.blue_oauth_session_id.trim().is_empty()
        || claims.jti.trim().is_empty()
        || claims.iat > OffsetDateTime::now_utc().unix_timestamp() + 60
        || claims.exp <= claims.iat
    {
        return Err(ResolveError::Invalid);
    }
    Ok(GatewayIdentity {
        user_id: claims.sub.parse().map_err(|_| ResolveError::Invalid)?,
        oauth_session_id: claims.blue_oauth_session_id,
        issued_at: claims.iat,
    })
}

async fn resolve_dynamic(
    state: &AppState,
    identity: &GatewayIdentity,
) -> Result<Mapping, ResolveError> {
    let url = state
        .resolver_url
        .as_ref()
        .ok_or(ResolveError::Unavailable)?;
    let token = get_valid_token(state)
        .await
        .ok_or(ResolveError::Unavailable)?;
    let _permit = state
        .resolver_permits
        .acquire()
        .await
        .map_err(|_| ResolveError::Unavailable)?;
    let client = state.control_client.read().await.clone();
    let response = client
        .post(url)
        .bearer_auth(&token)
        .timeout(state.resolver_timeout)
        .json(&serde_json::json!({
            "user_id": identity.user_id,
            "blue_oauth_session_id": identity.oauth_session_id,
        }))
        .send()
        .await
        .map_err(|_| ResolveError::Unavailable)?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(ResolveError::Invalid);
    }
    if !response.status().is_success() {
        return Err(ResolveError::Unavailable);
    }
    let body: ResolveResponse = response
        .json()
        .await
        .map_err(|_| ResolveError::Unavailable)?;
    if body.upstream_credential.trim().is_empty() {
        return Err(ResolveError::Unavailable);
    }
    Ok(Mapping {
        upstream_credential: body.upstream_credential,
        user: body.user,
        user_id: body.user_id,
        organization_id: body.organization_id,
        profile_id: body.profile_id,
        profile_name: body.profile_name,
        credential_version: body.credential_version,
        credential_expires_at: body
            .credential_expires_at
            .as_deref()
            .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok()),
        gateway_session_expires_at: body
            .gateway_session_expires_at
            .as_deref()
            .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok()),
        session_not_before: body
            .session_not_before
            .as_deref()
            .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok()),
    })
}

fn load_map() -> HashMap<String, Mapping> {
    let Some(path) = std::env::var("HARNESS_STATIC_TOKEN_MAP_FILE").ok() else {
        return HashMap::new();
    };
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {path}: {error}"));
    let raw: HashMap<String, serde_json::Value> =
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("parsing {path}: {error}"));
    raw.into_iter()
        .map(|(token, value)| {
            (
                token,
                Mapping {
                    upstream_credential: value
                        .get("virtual_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                    user: value
                        .get("user")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_owned(),
                    user_id: None,
                    organization_id: None,
                    profile_id: None,
                    profile_name: None,
                    credential_version: None,
                    credential_expires_at: None,
                    gateway_session_expires_at: None,
                    session_not_before: None,
                },
            )
        })
        .collect()
}

fn extract_inference_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(text) = value.to_str() {
            if let Some(token) = text.strip_prefix("Bearer ") {
                return Some(token.trim().to_owned());
            }
        }
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .map(str::to_owned)
}

fn token_digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Token bucket over `state.rates`. Callers prefix the key with the keyspace
/// they own (`t:` unverified token digest, `s:` verified session, `p:` peer
/// address) so the three limiters cannot collide in the shared map.
fn admitted_at(state: &AppState, key: &str, rps: u64, burst: u64) -> bool {
    let mut rates = state.rates.lock().expect("rate limiter poisoned");
    if rates.len() > 200_000 {
        rates.retain(|_, window| window.updated.elapsed() < Duration::from_secs(120));
    }
    let burst = burst.max(1) as f64;
    let window = rates.entry(key.to_owned()).or_insert(RateWindow {
        updated: Instant::now(),
        tokens: burst,
    });
    let now = Instant::now();
    window.tokens =
        (window.tokens + now.duration_since(window.updated).as_secs_f64() * rps as f64).min(burst);
    window.updated = now;
    if window.tokens < 1.0 {
        return false;
    }
    window.tokens -= 1.0;
    true
}

fn admitted(state: &AppState, cache_key: &str) -> bool {
    admitted_at(state, cache_key, state.per_token_rps, state.per_token_burst)
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}
fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[derive(Clone, Serialize)]
struct RequestLogEvent {
    id: Uuid,
    organization_id: String,
    user_id: String,
    profile_id: Option<String>,
    profile_name: Option<String>,
    occurred_at: String,
    method: String,
    path: String,
    model: Option<String>,
    http_status: Option<u16>,
    upstream_latency_ms: u64,
    harness: Option<String>,
    repository: Option<String>,
    branch: Option<String>,
    commit_sha: Option<String>,
    dirty: Option<bool>,
    run_id: Option<String>,
}
struct RequestLogMetadata {
    occurred_at: String,
    method: String,
    path: String,
    model: Option<String>,
    harness: Option<String>,
    repository: Option<String>,
    branch: Option<String>,
    commit_sha: Option<String>,
    dirty: Option<bool>,
    run_id: Option<String>,
}

#[derive(Clone, Serialize)]
struct InvalidCredentialReport {
    user_id: String,
    credential_version: String,
    reason: gh_gateway::InvalidCredentialReason,
    /// Historical LiteLLM-specific field retained for one compatibility
    /// release. It is absent for reasons old Control APIs cannot understand.
    #[serde(skip_serializing_if = "Option::is_none")]
    classification: Option<String>,
}

async fn invalid_credential_worker(
    state: Arc<AppState>,
    mut receiver: mpsc::Receiver<InvalidCredentialReport>,
    url: Option<String>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut reported = HashMap::<String, Instant>::new();
    loop {
        let report = tokio::select! {
            report = receiver.recv() => report,
            _ = shutdown.changed() => return,
        };
        let Some(report) = report else { return };
        let now = Instant::now();
        reported.retain(|_, at| now.duration_since(*at) < Duration::from_secs(300));
        if reported.contains_key(&report.credential_version) {
            continue;
        }
        reported.insert(report.credential_version.clone(), now);
        let Some(url) = url.as_ref() else {
            continue;
        };
        let Some(token) = get_valid_token(&state).await else {
            tracing::warn!("service token unavailable; skipping invalid-credential report");
            continue;
        };
        let client = state.control_client.read().await.clone();
        if let Err(error) = client
            .post(url)
            .bearer_auth(&token)
            .timeout(Duration::from_secs(45))
            .json(&report)
            .send()
            .await
        {
            tracing::warn!(%error, "failed to report invalid gateway credential");
        }
    }
}
fn request_log_metadata(method: &Method, uri: &Uri, headers: &HeaderMap) -> RequestLogMetadata {
    RequestLogMetadata {
        occurred_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into()),
        method: method.as_str().to_owned(),
        path: uri.path().to_owned(),
        model: header_text(headers, "x-harness-model"),
        harness: header_text(headers, "x-harness-agent"),
        repository: header_text(headers, "x-harness-repo"),
        branch: header_text(headers, "x-harness-git-branch"),
        commit_sha: header_text(headers, "x-harness-commit"),
        dirty: header_text(headers, "x-harness-dirty").and_then(|value| value.parse().ok()),
        run_id: header_text(headers, "x-harness-run-id"),
    }
}
fn enqueue_request_log(
    state: &AppState,
    mapping: &Mapping,
    metadata: RequestLogMetadata,
    http_status: Option<u16>,
    upstream_latency_ms: u64,
) {
    let (Some(user_id), Some(organization_id)) =
        (mapping.user_id.clone(), mapping.organization_id.clone())
    else {
        return;
    };
    let event = RequestLogEvent {
        id: Uuid::new_v4(),
        organization_id,
        user_id,
        profile_id: mapping.profile_id.clone(),
        profile_name: mapping.profile_name.clone(),
        occurred_at: metadata.occurred_at,
        method: metadata.method,
        path: metadata.path,
        model: metadata.model,
        http_status,
        upstream_latency_ms,
        harness: metadata.harness,
        repository: metadata.repository,
        branch: metadata.branch,
        commit_sha: metadata.commit_sha,
        dirty: metadata.dirty,
        run_id: metadata.run_id,
    };
    if state.log_tx.try_send(event).is_err() {
        state.metrics.log_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

async fn log_worker(
    state: Arc<AppState>,
    mut receiver: mpsc::Receiver<RequestLogEvent>,
    log_url: Option<String>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let Some(url) = log_url.map(|url| {
        if url.ends_with("/batch") {
            url
        } else {
            format!("{}/batch", url.trim_end_matches('/'))
        }
    }) else {
        while receiver.recv().await.is_some() {}
        return;
    };
    let mut deliveries = tokio::task::JoinSet::new();
    loop {
        let first = tokio::select! { event = receiver.recv() => event, _ = shutdown.changed() => { receiver.close(); receiver.recv().await } };
        let Some(first) = first else {
            break;
        };
        let mut events = vec![first];
        let deadline = tokio::time::sleep(Duration::from_millis(100));
        tokio::pin!(deadline);
        while events.len() < 500 {
            tokio::select! { event = receiver.recv() => match event { Some(event) => events.push(event), None => break }, _ = &mut deadline => break }
        }
        let count = events.len() as u64;
        let payload = serde_json::json!({ "events": events });
        if deliveries.len() >= 4 {
            let _ = deliveries.join_next().await;
        }
        deliveries.spawn(deliver_log_batch(
            state.clone(),
            url.clone(),
            payload,
            count,
        ));
    }
    while deliveries.join_next().await.is_some() {}
}

async fn deliver_log_batch(
    state: Arc<AppState>,
    url: String,
    payload: serde_json::Value,
    count: u64,
) {
    for attempt in 0..3 {
        if let Some(token) = get_valid_token(&state).await {
            let client = state.control_client.read().await.clone();
            let result = client
                .post(&url)
                .bearer_auth(&token)
                .timeout(Duration::from_secs(2))
                .json(&payload)
                .send()
                .await;
            if result
                .as_ref()
                .is_ok_and(|response| response.status().is_success())
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50 * (1 << attempt))).await;
    }
    state
        .metrics
        .log_dropped
        .fetch_add(count, Ordering::Relaxed);
    tracing::warn!(count, "dropping request-log batch");
}

#[derive(Deserialize)]
struct InvalidationEvent {
    user_id: String,
    oauth_session_id: Option<String>,
    #[allow(dead_code)]
    credential_version: Option<String>,
}
async fn invalidation_worker(
    state: Arc<AppState>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let url = state.event_url.clone().expect("event URL checked");
    let mut last_id: Option<String> = None;
    loop {
        mark_invalidation_unhealthy(&state);
        if *shutdown.borrow() {
            return;
        }
        let Some(token) = get_valid_token(&state).await else {
            tracing::warn!("service token unavailable for invalidation stream; retrying");
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        let client = state.control_client.read().await.clone();
        let mut request = client.get(&url).bearer_auth(&token);
        if let Some(id) = &last_id {
            request = request.header("last-event-id", id);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                state.invalidation_healthy.store(true, Ordering::Release);
                let mut bytes = response.bytes_stream();
                let mut buffer = String::new();
                let mut event_id = None;
                loop {
                    let chunk = tokio::select! {
                        chunk = tokio::time::timeout(Duration::from_secs(30), bytes.next()) => {
                            match chunk {
                                Ok(chunk) => chunk,
                                Err(_) => {
                                    tracing::warn!("invalidation stream heartbeat timed out");
                                    break;
                                }
                            }
                        },
                        _ = shutdown.changed() => return
                    };
                    let Some(Ok(chunk)) = chunk else {
                        break;
                    };
                    buffer.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(end) = buffer.find('\n') {
                        let line = buffer[..end].trim_end_matches('\r').to_owned();
                        buffer.drain(..=end);
                        if let Some(id) = line.strip_prefix("id:") {
                            event_id = Some(id.trim().to_owned());
                        } else if let Some(data) = line.strip_prefix("data:") {
                            if data.trim() == "{\"resync\":true}" {
                                state.cache.lock().expect("cache poisoned").clear();
                                state
                                    .metrics
                                    .invalidation_events
                                    .fetch_add(1, Ordering::Relaxed);
                                last_id = None;
                            } else if let Ok(event) =
                                serde_json::from_str::<InvalidationEvent>(data.trim())
                            {
                                state
                                    .cache
                                    .lock()
                                    .expect("cache poisoned")
                                    .invalidate(event.oauth_session_id.as_deref(), &event.user_id);
                                state
                                    .metrics
                                    .invalidation_events
                                    .fetch_add(1, Ordering::Relaxed);
                                last_id = event_id.take().or(last_id);
                            }
                        }
                    }
                }
            }
            Ok(response) => {
                tracing::warn!(status = %response.status(), "invalidation stream rejected")
            }
            Err(error) => tracing::warn!(%error, "invalidation stream unavailable"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn mark_invalidation_unhealthy(state: &AppState) {
    if state.invalidation_healthy.swap(false, Ordering::AcqRel) {
        state
            .metrics
            .invalidation_disconnects
            .fetch_add(1, Ordering::Relaxed);
    }
    state.cache.lock().expect("cache poisoned").clear();
}

fn cached_mapping(state: &AppState, cache_key: &str) -> Option<Mapping> {
    if state.invalidation_healthy.load(Ordering::Acquire) {
        state.cache.lock().expect("cache poisoned").get(cache_key)
    } else {
        state.metrics.cache_bypasses.fetch_add(1, Ordering::Relaxed);
        None
    }
}

fn too_many_requests(message: &'static str) -> Response {
    let mut response = (StatusCode::TOO_MANY_REQUESTS, message).into_response();
    response
        .headers_mut()
        .insert("retry-after", "1".parse().unwrap());
    response
}

async fn proxy_auth_guard(
    State(state): State<Arc<AppState>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    if !state.ready.load(Ordering::Acquire) {
        return (StatusCode::SERVICE_UNAVAILABLE, "proxy is draining").into_response();
    }
    let permit = match state.request_permits.clone().try_acquire_owned() {
        Ok(permit) => Arc::new(permit),
        Err(_) => {
            let mut response =
                (StatusCode::SERVICE_UNAVAILABLE, "proxy capacity exhausted").into_response();
            response
                .headers_mut()
                .insert("retry-after", "1".parse().unwrap());
            return response;
        }
    };
    let Some(inference_token) = extract_inference_token(request.headers()) else {
        return (StatusCode::UNAUTHORIZED, "missing inference token").into_response();
    };
    // Meter *before* verification. Signature checks are the expensive part, so
    // a limiter that only runs after them leaves unverifiable tokens entirely
    // unmetered.
    if !admitted(&state, &format!("t:{}", token_digest(&inference_token))) {
        state
            .metrics
            .rate_limited_unverified
            .fetch_add(1, Ordering::Relaxed);
        return too_many_requests("token rate limit exceeded");
    }
    let peer_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip());
    let identity = if state.resolver_url.is_some() {
        match validate_gateway_token(&state, &inference_token).await {
            Ok(identity) => Some(identity),
            Err(ResolveError::Invalid) => {
                // Digest keying alone cannot see a flood of freshly random
                // tokens — each one gets its own bucket. Charge the peer for
                // failures only, so legitimate traffic never touches this.
                if let Some(peer_ip) = peer_ip {
                    if !admitted_at(
                        &state,
                        &format!("p:{peer_ip}"),
                        state.unverified_rps,
                        state.unverified_rps,
                    ) {
                        state
                            .metrics
                            .rate_limited_unverified
                            .fetch_add(1, Ordering::Relaxed);
                        return too_many_requests("unverified token rate limit exceeded");
                    }
                }
                return (StatusCode::UNAUTHORIZED, "invalid inference token").into_response();
            }
            Err(ResolveError::Unavailable) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "JWT verification unavailable",
                )
                    .into_response()
            }
        }
    } else {
        None
    };
    let cache_key = identity
        .as_ref()
        .map(GatewayIdentity::cache_key)
        .unwrap_or_else(|| "local-development".into());
    if !admitted(&state, &format!("s:{cache_key}")) {
        state
            .metrics
            .rate_limited_session
            .fetch_add(1, Ordering::Relaxed);
        return too_many_requests("session rate limit exceeded");
    }

    let cached = cached_mapping(&state, &cache_key);
    let mapping = if identity.is_none() {
        if let Some(mapping) = state.map.get(&inference_token) {
            mapping.clone()
        } else if let Some(upstream_credential) = state.static_key.clone() {
            Mapping {
                upstream_credential,
                user: "dev".into(),
                user_id: None,
                organization_id: None,
                profile_id: None,
                profile_name: None,
                credential_version: None,
                credential_expires_at: None,
                gateway_session_expires_at: None,
                session_not_before: None,
            }
        } else {
            return (StatusCode::UNAUTHORIZED, "invalid local development token").into_response();
        }
    } else if let Some(mapping) = cached {
        state.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        mapping
    } else {
        state.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);
        let resolution_lock = {
            let mut locks = state
                .resolution_locks
                .lock()
                .expect("resolution locks poisoned");
            locks
                .entry(cache_key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _resolution_guard = resolution_lock.lock().await;
        let rechecked = cached_mapping(&state, &cache_key);
        if let Some(mapping) = rechecked {
            state.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
            state
                .resolution_locks
                .lock()
                .expect("resolution locks poisoned")
                .remove(&cache_key);
            mapping
        } else {
            let Some(identity) = identity.as_ref() else {
                unreachable!("local-development requests return before dynamic resolution")
            };
            let resolved = resolve_dynamic(&state, identity).await;
            state
                .resolution_locks
                .lock()
                .expect("resolution locks poisoned")
                .remove(&cache_key);
            match resolved {
                Ok(mapping) => {
                    let mut cache = state.cache.lock().expect("cache poisoned");
                    if state.invalidation_healthy.load(Ordering::Acquire) {
                        cache.insert(cache_key, mapping.clone());
                    }
                    mapping
                }
                Err(ResolveError::Invalid) => {
                    return (StatusCode::UNAUTHORIZED, "inactive gateway session").into_response()
                }
                Err(ResolveError::Unavailable) => {
                    state
                        .metrics
                        .resolver_errors
                        .fetch_add(1, Ordering::Relaxed);
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "credential resolver unavailable",
                    )
                        .into_response();
                }
            }
        }
    };

    // Reject JWTs minted before the session was reactivated. Pure integer
    // compare against a value already in the mapping — no resolver traffic.
    if let (Some(identity), Some(not_before)) = (identity.as_ref(), mapping.session_not_before) {
        if identity.issued_at < not_before.unix_timestamp() {
            return (
                StatusCode::UNAUTHORIZED,
                "inference token predates this session",
            )
                .into_response();
        }
    }

    request.extensions_mut().insert(mapping);
    request.extensions_mut().insert(permit);
    next.run(request).await
}

fn upstream_url(
    adapter: &dyn gh_gateway::GatewayAdapter,
    upstream_base: &str,
    path_and_query: &str,
) -> Result<String, String> {
    let path = adapter
        .upstream_path(path_and_query)
        .map_err(|error| error.to_string())?;
    if !path.starts_with('/') {
        return Err(format!(
            "gateway adapter `{}` returned a non-absolute upstream path `{path}`",
            adapter.kind()
        ));
    }
    Ok(format!("{upstream_base}{path}"))
}

fn upstream_credential_header(
    placement: gh_gateway::UpstreamCredentialPlacement,
) -> Result<HeaderName, String> {
    let header = match placement {
        gh_gateway::UpstreamCredentialPlacement::AuthorizationBearer => {
            Ok(HeaderName::from_static("authorization"))
        }
        gh_gateway::UpstreamCredentialPlacement::Header(name) => {
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("invalid header name `{name}`"))
        }
    }?;
    if matches!(header.as_str(), "host" | "content-length") || is_hop_by_hop(header.as_str()) {
        return Err(format!("forbidden credential header `{header}`"));
    }
    Ok(header)
}

fn apply_upstream_credential(
    placement: gh_gateway::UpstreamCredentialPlacement,
    request: reqwest::RequestBuilder,
    credential: &str,
    credential_header: &HeaderName,
) -> reqwest::RequestBuilder {
    match placement {
        gh_gateway::UpstreamCredentialPlacement::AuthorizationBearer => {
            request.bearer_auth(credential)
        }
        gh_gateway::UpstreamCredentialPlacement::Header(_) => {
            request.header(credential_header, credential)
        }
    }
}

fn copy_forwarded_headers(
    mut upstream: reqwest::RequestBuilder,
    headers: &HeaderMap,
    credential_header: &HeaderName,
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization" | "x-api-key" | "host" | "content-length"
        ) || name == credential_header
            || is_hop_by_hop(&lower)
        {
            continue;
        }
        upstream = upstream.header(name, value);
    }
    upstream
}

async fn proxy(
    State(state): State<Arc<AppState>>,
    Extension(mapping): Extension<Mapping>,
    Extension(permit): Extension<RequestPermit>,
    request: Request<Body>,
) -> Response {
    let (parts, body) = request.into_parts();
    let method = parts.method;
    let uri = parts.uri;
    let headers = parts.headers;
    let mut metadata = request_log_metadata(&method, &uri, &headers);
    if headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|size| size > state.max_body_bytes)
    {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeds proxy limit",
        )
            .into_response();
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let max = state.max_body_bytes;
    let model_prefix = Arc::new(Mutex::new(Vec::with_capacity(4096)));
    let capture = model_prefix.clone();
    let request_stream = body.into_data_stream().map(move |item| match item {
        Ok(chunk) if seen.fetch_add(chunk.len(), Ordering::Relaxed) + chunk.len() <= max => {
            let mut prefix = capture.lock().expect("model capture poisoned");
            if prefix.len() < 64 * 1024 {
                let remaining = 64 * 1024 - prefix.len();
                prefix.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Ok(chunk)
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request body exceeds proxy limit",
        )),
        Err(error) => Err(std::io::Error::other(error)),
    });
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let url = match upstream_url(state.gateway, &state.upstream_base, path_and_query) {
        Ok(url) => url,
        Err(error) => {
            tracing::error!(gateway = state.gateway.kind(), %error, "gateway adapter rejected upstream path");
            return (StatusCode::BAD_GATEWAY, "invalid upstream gateway path").into_response();
        }
    };
    let mut upstream = state
        .upstream_client
        .request(method, &url)
        .body(reqwest::Body::wrap_stream(request_stream));
    upstream = copy_forwarded_headers(upstream, &headers, &state.upstream_credential_header);
    upstream = apply_upstream_credential(
        state.gateway.upstream_credential_placement(),
        upstream,
        &mapping.upstream_credential,
        &state.upstream_credential_header,
    );
    let started = Instant::now();
    match upstream.send().await {
        Ok(response) => {
            let status = response.status();
            if metadata.model.is_none() {
                metadata.model = model_from_prefix(&model_prefix);
            }
            enqueue_request_log(
                &state,
                &mapping,
                metadata,
                Some(status.as_u16()),
                started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            );
            // Inspect only adapter-selected responses with an explicit bounded
            // length so an untrusted upstream cannot force arbitrary buffering.
            if state.gateway.inspect_response_status(status.as_u16())
                && response
                    .content_length()
                    .is_some_and(|size| size <= 64 * 1024)
            {
                let response_headers = response.headers().clone();
                match response.bytes().await {
                    Ok(bytes) => {
                        if let (Some(reason), Some(user_id), Some(version)) = (
                            state
                                .gateway
                                .classify_invalid_credential(status.as_u16(), &bytes),
                            mapping.user_id.as_ref(),
                            mapping.credential_version.as_ref(),
                        ) {
                            let _ = state.invalidation_tx.try_send(InvalidCredentialReport {
                                user_id: user_id.clone(),
                                credential_version: version.clone(),
                                reason,
                                classification: reason.legacy_classification().map(str::to_owned),
                            });
                        }
                        let mut builder = Response::builder().status(status);
                        for (name, value) in &response_headers {
                            let lower = name.as_str().to_ascii_lowercase();
                            if lower != "content-length" && !is_hop_by_hop(&lower) {
                                builder = builder.header(name, value);
                            }
                        }
                        return builder.body(Body::from(bytes)).unwrap_or_else(|_| {
                            (StatusCode::BAD_GATEWAY, "bad upstream response").into_response()
                        });
                    }
                    Err(_) => {
                        return (StatusCode::BAD_GATEWAY, "bad upstream response").into_response()
                    }
                }
            }
            let mut builder = Response::builder().status(status);
            for (name, value) in response.headers() {
                let lower = name.as_str().to_ascii_lowercase();
                if lower != "content-length" && !is_hop_by_hop(&lower) {
                    builder = builder.header(name, value);
                }
            }
            state.metrics.active_streams.fetch_add(1, Ordering::Relaxed);
            let stream = hold_permit(response.bytes_stream(), permit, state.clone());
            builder.body(Body::from_stream(stream)).unwrap_or_else(|_| {
                (StatusCode::BAD_GATEWAY, "bad upstream response").into_response()
            })
        }
        Err(error) => {
            tracing::error!(%error, %url, user = %mapping.user, version = ?mapping.credential_version, "upstream request failed");
            if metadata.model.is_none() {
                metadata.model = model_from_prefix(&model_prefix);
            }
            enqueue_request_log(
                &state,
                &mapping,
                metadata,
                None,
                started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            );
            (StatusCode::BAD_GATEWAY, "upstream request failed").into_response()
        }
    }
}

fn model_from_prefix(prefix: &Mutex<Vec<u8>>) -> Option<String> {
    let prefix = prefix.lock().expect("model capture poisoned");
    serde_json::from_slice::<serde_json::Value>(&prefix)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(|model| model.as_str())
                .map(str::to_owned)
        })
        .or_else(|| {
            let text = std::str::from_utf8(&prefix).ok()?;
            let after_key = text.split_once("\"model\"")?.1;
            let after_colon = after_key.split_once(':')?.1.trim_start();
            let quoted = after_colon.strip_prefix('"')?;
            Some(quoted.split_once('"')?.0.to_owned())
        })
}

fn hold_permit(
    stream: impl futures_util::Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Send + 'static,
    permit: RequestPermit,
    state: Arc<AppState>,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Send + 'static {
    struct Guard {
        _permit: RequestPermit,
        state: Arc<AppState>,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            self.state
                .metrics
                .active_streams
                .fetch_sub(1, Ordering::Relaxed);
        }
    }
    let guard = Guard {
        _permit: permit,
        state,
    };
    stream.map(move |item| {
        let _keep = &guard;
        item
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mapping(user_id: &str) -> Mapping {
        Mapping {
            upstream_credential: "secret".into(),
            user: "user@example.com".into(),
            user_id: Some(user_id.into()),
            organization_id: Some("org".into()),
            profile_id: None,
            profile_name: None,
            credential_version: Some("v1".into()),
            credential_expires_at: None,
            gateway_session_expires_at: None,
            session_not_before: None,
        }
    }
    #[test]
    fn cache_is_bounded_and_invalidates_by_user() {
        let mut cache = CredentialCache::new(2, Duration::from_secs(60));
        cache.insert("one".into(), mapping("u1"));
        cache.insert("two".into(), mapping("u2"));
        cache.insert("three".into(), mapping("u3"));
        assert!(cache.get("one").is_none());
        assert!(cache.get("two").is_some());
        cache.invalidate(None, "u2");
        assert!(cache.get("two").is_none());
    }
    #[test]
    fn cache_invalidation_can_target_one_oauth_session() {
        let mut cache = CredentialCache::new(2, Duration::from_secs(60));
        cache.insert("session-a:u1".into(), mapping("u1"));
        cache.insert("session-b:u1".into(), mapping("u1"));
        cache.invalidate(Some("session-a"), "u1");
        assert!(cache.get("session-a:u1").is_none());
        assert!(cache.get("session-b:u1").is_some());
    }

    #[test]
    fn invalidation_disconnect_clears_and_bypasses_the_cache() {
        let state = test_state(None);
        state
            .cache
            .lock()
            .unwrap()
            .insert("session-a:u1".into(), mapping("u1"));

        mark_invalidation_unhealthy(&state);

        assert!(!state.invalidation_healthy.load(Ordering::Acquire));
        assert!(cached_mapping(&state, "session-a:u1").is_none());
        assert_eq!(
            state
                .metrics
                .invalidation_disconnects
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(state.metrics.cache_bypasses.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn metadata_uses_allowlisted_headers_and_omits_query() {
        let mut headers = HeaderMap::new();
        headers.insert("x-harness-agent", "codex".parse().unwrap());
        headers.insert("x-harness-model", "gpt-5".parse().unwrap());
        let metadata = request_log_metadata(
            &Method::POST,
            &"/v1/responses?api_key=secret".parse().unwrap(),
            &headers,
        );
        assert_eq!(metadata.path, "/v1/responses");
        assert_eq!(metadata.model.as_deref(), Some("gpt-5"));
        assert_eq!(metadata.harness.as_deref(), Some("codex"));
    }

    #[test]
    fn model_can_be_captured_from_an_incomplete_streaming_prefix() {
        let prefix = Mutex::new(br#"{"model":"gpt-5","input":"unfinished"#.to_vec());
        assert_eq!(model_from_prefix(&prefix).as_deref(), Some("gpt-5"));
    }

    use std::sync::atomic::AtomicUsize;

    fn test_oauth(token_url: String) -> OauthConfig {
        OauthConfig {
            token_url,
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            scope: "gateway:resolve".into(),
            resource: "http://control-api.test".into(),
            refresh_skew: None,
        }
    }

    #[test]
    fn internal_transport_modes_require_an_exact_url_scheme() {
        assert!(internal_url_matches_transport(
            InternalTransportMode::Mtls,
            "https://control-api-internal:8082/internal/gateway/resolve"
        ));
        assert!(!internal_url_matches_transport(
            InternalTransportMode::Mtls,
            "http://control-api-internal:8082/internal/gateway/resolve"
        ));
        assert!(internal_url_matches_transport(
            InternalTransportMode::InsecureHttp,
            "http://control-api-internal:8082/internal/gateway/resolve"
        ));
        assert!(!internal_url_matches_transport(
            InternalTransportMode::InsecureHttp,
            "https://control-api-internal:8082/internal/gateway/resolve"
        ));
    }

    fn test_state(oauth: Option<OauthConfig>) -> Arc<AppState> {
        let (log_tx, _log_rx) = mpsc::channel(16);
        let (invalidation_tx, _invalidation_rx) = mpsc::channel(16);
        // Intentionally leak the receivers so the channels stay open for the
        // lifetime of the test without us having to thread them through.
        Box::leak(Box::new(_log_rx));
        Box::leak(Box::new(_invalidation_rx));
        Arc::new(AppState {
            gateway: gh_gateway::gateway_adapter("litellm").unwrap(),
            upstream_credential_header: HeaderName::from_static("authorization"),
            upstream_base: "http://upstream.test".into(),
            map: HashMap::new(),
            static_key: None,
            resolver_url: None,
            gateway_jwks_url: None,
            gateway_jwt_issuer: None,
            gateway_jwt_audience: None,
            gateway_jwks: RwLock::new(None),
            event_url: None,
            oauth,
            service_token: RwLock::new(None),
            token_refresh_lock: tokio::sync::Mutex::new(()),
            upstream_client: reqwest::Client::new(),
            oauth_client: reqwest::Client::new(),
            control_client: RwLock::new(reqwest::Client::new()),
            resolver_timeout: Duration::from_secs(3),
            cache: Mutex::new(CredentialCache::new(10, Duration::from_secs(60))),
            resolution_locks: Mutex::new(HashMap::new()),
            rates: Mutex::new(HashMap::new()),
            per_token_rps: 100,
            per_token_burst: 100,
            unverified_rps: 20,
            jwks_refetch_cooldown: Duration::from_secs(30),
            resolver_permits: Arc::new(Semaphore::new(8)),
            request_permits: Arc::new(Semaphore::new(8)),
            max_body_bytes: 1024,
            log_tx,
            invalidation_tx,
            metrics: Metrics::default(),
            invalidation_healthy: AtomicBool::new(true),
            ready: AtomicBool::new(true),
        })
    }

    async fn token_handler(
        State(counter): State<Arc<AtomicUsize>>,
    ) -> axum::Json<serde_json::Value> {
        counter.fetch_add(1, Ordering::SeqCst);
        axum::Json(serde_json::json!({ "access_token": "service-token", "expires_in": 100 }))
    }

    /// Spawns a fake OAuth token endpoint that counts how many times it is hit
    /// and returns its `.../token` URL.
    async fn spawn_token_server(counter: Arc<AtomicUsize>) -> String {
        let app = Router::new()
            .route("/token", axum::routing::post(token_handler))
            .with_state(counter);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/token")
    }

    async fn spawn_gateway_jwks_server() -> String {
        async fn handler() -> axum::Json<serde_json::Value> {
            let mut set: serde_json::Value = serde_json::from_str(include_str!(
                "../../../tests/e2e-slim/fixtures/jwks/jwks.json"
            ))
            .unwrap();
            let mut retained = set["keys"][0].clone();
            retained["kid"] = "retained-key".into();
            set["keys"].as_array_mut().unwrap().push(retained);
            axum::Json(set)
        }
        let app = Router::new().route("/jwks", get(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/jwks")
    }

    /// Same fixture as `spawn_gateway_jwks_server`, but counts every fetch.
    async fn spawn_counting_jwks_server(counter: Arc<AtomicUsize>) -> String {
        async fn handler(State(counter): State<Arc<AtomicUsize>>) -> axum::Json<serde_json::Value> {
            counter.fetch_add(1, Ordering::SeqCst);
            axum::Json(
                serde_json::from_str(include_str!(
                    "../../../tests/e2e-slim/fixtures/jwks/jwks.json"
                ))
                .unwrap(),
            )
        }
        let app = Router::new()
            .route("/jwks", get(handler))
            .with_state(counter);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/jwks")
    }

    #[tokio::test]
    async fn a_repeated_unknown_kid_costs_exactly_one_jwks_fetch() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut state = test_state(None);
        let jwks_url = spawn_counting_jwks_server(counter.clone()).await;
        let mutable = Arc::get_mut(&mut state).unwrap();
        mutable.gateway_jwks_url = Some(jwks_url);

        for _ in 0..5 {
            assert!(matches!(
                gateway_jwk_for(&state, "no-such-kid").await,
                Err(ResolveError::Invalid)
            ));
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(state.metrics.jwks_unknown_kid.load(Ordering::Relaxed), 5);

        // The unknown kid still populated the cache, so a legitimate kid that
        // arrives afterwards is served without another fetch.
        assert!(gateway_jwk_for(&state, "e2e-slim-rsa-1").await.is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(state.metrics.jwks_fetches.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn an_expired_cooldown_lets_key_rotation_converge() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut state = test_state(None);
        let jwks_url = spawn_counting_jwks_server(counter.clone()).await;
        let mutable = Arc::get_mut(&mut state).unwrap();
        mutable.gateway_jwks_url = Some(jwks_url);
        mutable.jwks_refetch_cooldown = Duration::ZERO;

        for _ in 0..3 {
            assert!(gateway_jwk_for(&state, "rotated-in-kid").await.is_err());
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn unverified_and_session_limiters_do_not_share_a_keyspace() {
        let state = test_state(None);
        // Same string, different keyspaces: exhausting one must not affect the
        // other.
        for _ in 0..state.per_token_burst {
            assert!(admitted(&state, "t:collide"));
        }
        assert!(!admitted(&state, "t:collide"));
        assert!(admitted(&state, "s:collide"));
    }

    #[test]
    fn a_repeated_malformed_token_is_rejected_before_verification() {
        let mut state = test_state(None);
        let mutable = Arc::get_mut(&mut state).unwrap();
        mutable.per_token_rps = 0;
        mutable.per_token_burst = 2;
        let key = format!("t:{}", token_digest("not-a-jwt"));
        assert!(admitted(&state, &key));
        assert!(admitted(&state, &key));
        assert!(!admitted(&state, &key));
    }

    fn sign_gateway_claims(claims: &GatewayInferenceClaims, kid: &str) -> String {
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
            "../../../tests/e2e-slim/fixtures/jwks/jwt-signing-key.pem"
        ))
        .unwrap();
        let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        jsonwebtoken::encode(&header, claims, &key).unwrap()
    }

    #[tokio::test]
    async fn inference_jwt_validation_rejects_invalid_claims_and_signatures() {
        let mut state = test_state(None);
        let jwks_url = spawn_gateway_jwks_server().await;
        let mutable = Arc::get_mut(&mut state).unwrap();
        mutable.gateway_jwks_url = Some(jwks_url);
        mutable.gateway_jwt_issuer = Some("https://control.example".into());
        mutable.gateway_jwt_audience = Some("blue-inference-proxy".into());
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut claims = GatewayInferenceClaims {
            iss: "https://control.example".into(),
            aud: "blue-inference-proxy".into(),
            sub: Uuid::new_v4().to_string(),
            iat: now,
            exp: now + 300,
            jti: Uuid::new_v4().to_string(),
            blue_oauth_session_id: "oauth-session".into(),
            scope: "gateway:infer".into(),
        };
        let valid = sign_gateway_claims(&claims, "e2e-slim-rsa-1");
        let identity = validate_gateway_token(&state, &valid).await.unwrap();
        assert_eq!(identity.oauth_session_id, "oauth-session");
        assert!(
            validate_gateway_token(&state, &sign_gateway_claims(&claims, "retained-key"))
                .await
                .is_ok()
        );

        claims.aud = "wrong-audience".into();
        assert!(matches!(
            validate_gateway_token(&state, &sign_gateway_claims(&claims, "e2e-slim-rsa-1")).await,
            Err(ResolveError::Invalid)
        ));
        claims.aud = "blue-inference-proxy".into();
        claims.iss = "https://wrong.example".into();
        assert!(matches!(
            validate_gateway_token(&state, &sign_gateway_claims(&claims, "e2e-slim-rsa-1")).await,
            Err(ResolveError::Invalid)
        ));
        claims.iss = "https://control.example".into();
        claims.scope = "governance:read".into();
        assert!(matches!(
            validate_gateway_token(&state, &sign_gateway_claims(&claims, "e2e-slim-rsa-1")).await,
            Err(ResolveError::Invalid)
        ));
        claims.scope = "gateway:infer".into();
        claims.blue_oauth_session_id.clear();
        assert!(matches!(
            validate_gateway_token(&state, &sign_gateway_claims(&claims, "e2e-slim-rsa-1")).await,
            Err(ResolveError::Invalid)
        ));
        claims.blue_oauth_session_id = "oauth-session".into();
        claims.sub = "not-a-uuid".into();
        assert!(matches!(
            validate_gateway_token(&state, &sign_gateway_claims(&claims, "e2e-slim-rsa-1")).await,
            Err(ResolveError::Invalid)
        ));
        claims.sub = Uuid::new_v4().to_string();
        claims.exp = now - 120;
        assert!(matches!(
            validate_gateway_token(&state, &sign_gateway_claims(&claims, "e2e-slim-rsa-1")).await,
            Err(ResolveError::Invalid)
        ));
        claims.exp = now + 300;
        let mut tampered = sign_gateway_claims(&claims, "e2e-slim-rsa-1");
        tampered.push('x');
        assert!(matches!(
            validate_gateway_token(&state, &tampered).await,
            Err(ResolveError::Invalid)
        ));
        assert!(matches!(
            validate_gateway_token(&state, &sign_gateway_claims(&claims, "unknown-key")).await,
            Err(ResolveError::Invalid)
        ));
    }

    async fn capture_headers(
        State(sender): State<Arc<Mutex<Option<tokio::sync::oneshot::Sender<HeaderMap>>>>>,
        headers: HeaderMap,
    ) -> StatusCode {
        if let Some(sender) = sender.lock().expect("capture lock poisoned").take() {
            let _ = sender.send(headers);
        }
        StatusCode::NO_CONTENT
    }

    #[tokio::test]
    async fn forwarding_replaces_adapter_credential_header_exactly_once() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let app = Router::new()
            .fallback(capture_headers)
            .with_state(Arc::new(Mutex::new(Some(sender))));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut incoming = HeaderMap::new();
        incoming.append("x-provider-key", "attacker-one".parse().unwrap());
        incoming.append("x-provider-key", "attacker-two".parse().unwrap());
        incoming.insert("authorization", "Bearer attacker".parse().unwrap());
        incoming.insert("x-api-key", "attacker".parse().unwrap());
        incoming.insert("x-request-id", "request-123".parse().unwrap());

        let placement = gh_gateway::UpstreamCredentialPlacement::Header("x-provider-key");
        let credential_header = upstream_credential_header(placement).unwrap();
        let request = reqwest::Client::new().post(format!("http://{address}/capture"));
        let request = copy_forwarded_headers(request, &incoming, &credential_header);
        apply_upstream_credential(placement, request, "resolved-secret", &credential_header)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        let captured = receiver.await.unwrap();
        let values = captured
            .get_all("x-provider-key")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, ["resolved-secret"]);
        assert!(captured.get("authorization").is_none());
        assert!(captured.get("x-api-key").is_none());
        assert_eq!(captured.get("x-request-id").unwrap(), "request-123");
    }

    #[tokio::test]
    async fn get_valid_token_serves_cache_until_refresh_at() {
        let counter = Arc::new(AtomicUsize::new(0));
        let url = spawn_token_server(counter.clone()).await;
        let state = test_state(Some(test_oauth(url)));

        let first = get_valid_token(&state).await;
        assert_eq!(first.as_deref(), Some("service-token"));
        // A second call well inside the refresh window must not hit the server.
        let second = get_valid_token(&state).await;
        assert_eq!(second, first);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(state.metrics.oauth_token_fetches.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn get_valid_token_single_flights_concurrent_refresh() {
        let counter = Arc::new(AtomicUsize::new(0));
        let url = spawn_token_server(counter.clone()).await;
        let state = test_state(Some(test_oauth(url)));

        let mut handles = Vec::new();
        for _ in 0..32 {
            let state = state.clone();
            handles.push(tokio::spawn(async move { get_valid_token(&state).await }));
        }
        for handle in handles {
            assert_eq!(handle.await.unwrap().as_deref(), Some("service-token"));
        }
        // A burst against an empty cache must collapse into exactly one fetch.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn get_valid_token_returns_none_without_oauth() {
        let state = test_state(None);
        assert!(get_valid_token(&state).await.is_none());
    }

    #[test]
    fn every_control_api_hop_requires_m2m_configuration() {
        let url = "http://control-api.test/internal".to_owned();
        assert!(control_api_hop_configured(Some(&url), None, None, None));
        assert!(control_api_hop_configured(None, Some(&url), None, None));
        assert!(control_api_hop_configured(None, None, Some(&url), None));
        assert!(control_api_hop_configured(None, None, None, Some(&url)));
        assert!(!control_api_hop_configured(None, None, None, None));
    }

    #[test]
    fn required_gateway_environment_errors_are_aggregated_and_redacted() {
        let values = HashMap::from([
            (
                "HARNESS_GATEWAY_RESOLVER_URL".to_owned(),
                "https://control.example/internal/gateway/resolve".to_owned(),
            ),
            (
                "UNRELATED_SECRET".to_owned(),
                "must-not-appear-in-errors".to_owned(),
            ),
        ]);
        let error = validate_required_gateway_env(|name| values.get(name).cloned()).unwrap_err();
        for name in [
            "HARNESS_GATEWAY_TYPE",
            "HARNESS_GATEWAY_URL",
            "HARNESS_PROXY_OAUTH_TOKEN_URL",
            "HARNESS_PROXY_OAUTH_CLIENT_ID",
            "HARNESS_PROXY_OAUTH_CLIENT_SECRET",
            "HARNESS_PROXY_OAUTH_RESOURCE",
        ] {
            assert!(error.contains(name), "missing {name} in {error}");
        }
        assert!(!error.contains("must-not-appear-in-errors"));
    }

    #[test]
    fn selects_only_compiled_gateway_types() {
        assert_eq!(
            select_gateway_adapter(Some("litellm")).unwrap().kind(),
            "litellm"
        );
        assert!(select_gateway_adapter(None)
            .err()
            .unwrap()
            .contains("HARNESS_GATEWAY_TYPE is required"));
        assert!(select_gateway_adapter(Some("unknown"))
            .err()
            .unwrap()
            .contains("compiled: litellm"));
        validate_gateway_adapter(gh_gateway::gateway_adapter("litellm").unwrap()).unwrap();
    }

    #[test]
    fn adapter_controls_upstream_target_and_credential_placement() {
        let gateway = gh_gateway::gateway_adapter("litellm").unwrap();
        assert_eq!(
            upstream_url(gateway, "https://gateway.test", "/v1/responses?stream=true").unwrap(),
            "https://gateway.test/v1/responses?stream=true"
        );

        let bearer = apply_upstream_credential(
            gh_gateway::UpstreamCredentialPlacement::AuthorizationBearer,
            reqwest::Client::new().get("https://gateway.test"),
            "upstream-secret",
            &HeaderName::from_static("authorization"),
        )
        .build()
        .unwrap();
        assert_eq!(
            bearer
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer upstream-secret"
        );

        let header = apply_upstream_credential(
            gh_gateway::UpstreamCredentialPlacement::Header("x-provider-key"),
            reqwest::Client::new().get("https://gateway.test"),
            "upstream-secret",
            &HeaderName::from_static("x-provider-key"),
        )
        .build()
        .unwrap();
        assert_eq!(
            header
                .headers()
                .get("x-provider-key")
                .unwrap()
                .to_str()
                .unwrap(),
            "upstream-secret"
        );
        assert!(
            upstream_credential_header(gh_gateway::UpstreamCredentialPlacement::Header(
                "Invalid Header"
            ))
            .is_err()
        );
        assert_eq!(
            upstream_credential_header(gh_gateway::UpstreamCredentialPlacement::Header(
                "x-provider-key"
            ))
            .unwrap(),
            HeaderName::from_static("x-provider-key")
        );
        assert!(
            upstream_credential_header(gh_gateway::UpstreamCredentialPlacement::Header(
                "content-length"
            ))
            .is_err()
        );
        assert!(
            upstream_credential_header(gh_gateway::UpstreamCredentialPlacement::Header(
                "connection"
            ))
            .is_err()
        );
    }

    #[test]
    fn invalidation_report_dual_wires_litellm_reasons() {
        let report = InvalidCredentialReport {
            user_id: "user".into(),
            credential_version: "version".into(),
            reason: gh_gateway::InvalidCredentialReason::NotFound,
            classification: gh_gateway::InvalidCredentialReason::NotFound
                .legacy_classification()
                .map(str::to_owned),
        };
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["reason"], "not_found");
        assert_eq!(value["classification"], "token_not_found_in_db");

        let revoked = InvalidCredentialReport {
            user_id: "user".into(),
            credential_version: "version".into(),
            reason: gh_gateway::InvalidCredentialReason::Revoked,
            classification: None,
        };
        assert!(serde_json::to_value(revoked)
            .unwrap()
            .get("classification")
            .is_none());
    }

    #[test]
    fn classifies_only_litellm_invalid_key_errors() {
        let gateway = gh_gateway::gateway_adapter("litellm").unwrap();
        assert_eq!(
            gateway.classify_invalid_credential(
                StatusCode::UNAUTHORIZED.as_u16(),
                br#"{"error":{"type":"token_not_found_in_db","code":"401"}}"#,
            ),
            Some(gh_gateway::InvalidCredentialReason::NotFound)
        );
        assert_eq!(
            gateway.classify_invalid_credential(
                StatusCode::UNAUTHORIZED.as_u16(),
                br#"{"error":{"message":"Authentication Error, Key is blocked. Update via /key/unblock"}}"#,
            ),
            Some(gh_gateway::InvalidCredentialReason::Blocked)
        );
        assert_eq!(
            gateway.classify_invalid_credential(
                StatusCode::UNAUTHORIZED.as_u16(),
                br#"{"error":{"type":"auth_error","message":"model is forbidden"}}"#,
            ),
            None
        );
        assert_eq!(
            gateway.classify_invalid_credential(
                StatusCode::FORBIDDEN.as_u16(),
                br#"{"error":{"type":"token_not_found_in_db"}}"#,
            ),
            None
        );
    }
}
