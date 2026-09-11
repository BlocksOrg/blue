//! `gh-service` — the client for the provisioned service URL: authenticate the
//! user, fetch/cache the governance config, and expose the versioned wire
//! [`schema`]. The service is the identity anchor and the source of truth for
//! what the harness may do; this crate is how the client talks to it.

pub mod cache;
pub mod client;
pub mod discovery;
pub mod identity;
pub mod schema;
pub mod source;

pub use cache::CachedConfig;
pub use client::{authenticate, login, require_session, RevisionStreamEnd, ServiceClient};
pub use discovery::{discover, validate_deployment_url, DiscoveryDocument, DiscoveryOAuth};
pub use identity::{default_cli_scopes, IdentityProvider, RefreshFailure, Session};
pub use schema::{
    legacy_requirement_interval, GatewayConfig, GovernanceConfig, HarnessPolicy, ManagedConfig,
    ManagedPackage, McpServer, PackageAdapter, PackageAdapterInterval, PackageAdapterVariant,
    PackageOverride, PackageSource, PlatformAsset, PresignedUpload, SessionUploadConfig,
    TelemetryConfig,
};
pub use source::{
    bounded_detail, server_error_detail, session_rejected_message, ConfigSource, FileConfigSource,
    HttpConfigSource,
};

/// Current unix time in seconds. Kept in one place so callers don't scatter
/// clock reads (and tests can inject their own `now`).
pub fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
