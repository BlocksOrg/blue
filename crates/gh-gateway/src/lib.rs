//! `gh-gateway` — compiled inference gateway behavior, **gateway mode only**.
//!
//! This crate contains only dependency-light, pure decisions shared by the
//! client, Control API, and inference proxy. It never writes files, sends
//! requests, or provisions credentials. The single writer (`gh-config`) lands
//! [`GatewayWiring`] in each harness's config.
//!
//! In governance-only mode this crate is never called: with no top-level
//! `gateway` block the harness keeps its own credentials and base-URL.

use gh_common::GhError;
use gh_service::GatewayConfig;
use serde::{Deserialize, Serialize};

mod litellm;

/// Where the inference JWT must be placed for a given harness. Codex references
/// an env var by name; Claude carries an in-file `ANTHROPIC_AUTH_TOKEN`;
/// Kimi/OpenCode bake it into their config/auth files. `gh-config` uses this to
/// route the token correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPlacement {
    /// Token is exported in an env var of this name; the harness config
    /// references it (Codex `env_key`). Also what `gh-agent` publishes to GUI
    /// app environments.
    EnvVar(String),
    /// Token is written directly into a harness config/auth file.
    InFile,
}

/// Gateway-owned client routing before harness placement is attached.
#[derive(Clone, PartialEq, Eq)]
pub struct GatewayRoute {
    /// The Blue inference proxy URL presented to the agent.
    pub base_url: String,
    /// The user's session-bound inference JWT.
    pub token: String,
}

impl std::fmt::Debug for GatewayRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayRoute")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// The concrete, harness-specific routing `gh-config` needs to write.
#[derive(Clone)]
pub struct GatewayWiring {
    /// The base URL the agent points at — the upstream inference proxy (or, once
    /// the attribution proxy exists, the local loopback that forwards to it).
    pub base_url: String,
    /// The session-bound inference JWT. Never a real provider key.
    pub token: String,
    /// Codex-style wire protocol hint (`"responses"`); `None` for others.
    pub wire_api: Option<String>,
    /// How/where the token is presented.
    pub auth: AuthPlacement,
}

impl std::fmt::Debug for GatewayWiring {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayWiring")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .field("wire_api", &self.wire_api)
            .field("auth", &self.auth)
            .finish()
    }
}

/// Where the inference proxy places the resolved upstream credential.
///
/// The enum describes placement only and never contains credential bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamCredentialPlacement {
    AuthorizationBearer,
    Header(&'static str),
}

/// Provider-neutral reason an upstream credential is definitively unusable.
///
/// Expiration is owned by the Control API and is intentionally not represented
/// here. Adapters return `None` for ambiguous authorization or availability
/// failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidCredentialReason {
    NotFound,
    Blocked,
    Revoked,
}

impl InvalidCredentialReason {
    /// Historical LiteLLM classification retained for one rolling-upgrade
    /// compatibility window.
    pub fn legacy_classification(self) -> Option<&'static str> {
        match self {
            Self::NotFound => Some("token_not_found_in_db"),
            Self::Blocked => Some("key_blocked"),
            Self::Revoked => None,
        }
    }

    /// Translate a historical LiteLLM classification during the compatibility
    /// window.
    pub fn from_legacy_classification(value: &str) -> Option<Self> {
        match value {
            "token_not_found_in_db" => Some(Self::NotFound),
            "key_blocked" => Some(Self::Blocked),
            _ => None,
        }
    }

    /// Provider-neutral reason persisted by the Control API.
    pub fn invalidation_message(self) -> &'static str {
        match self {
            Self::NotFound => "upstream gateway credential was not found",
            Self::Blocked => "upstream gateway credential was blocked",
            Self::Revoked => "upstream gateway credential was revoked",
        }
    }
}

/// Env var Codex references for its governed provider's key. GUI apps that
/// spawn Codex must have this in their environment (published by `gh-agent`).
pub const CODEX_ENV_KEY: &str = "HARNESS_CODEX_KEY";

/// One compiled gateway integration shared across Blue's client and services.
///
/// Every method is pure. Harness implementations retain their version-owned
/// authentication placement and wire protocol; the inference proxy applies the
/// upstream request decisions.
pub trait GatewayAdapter: Send + Sync {
    /// Stable key selected by `gateway.type`.
    fn kind(&self) -> &'static str;

    /// Validate runtime policy and compute gateway-owned client routing.
    fn client_route(&self, gateway: &GatewayConfig) -> Result<GatewayRoute, GhError>;

    /// Map the incoming path and query to the upstream request target.
    fn upstream_path(&self, path_and_query: &str) -> Result<String, GhError>;

    /// Declare where the resolved upstream credential is placed.
    fn upstream_credential_placement(&self) -> UpstreamCredentialPlacement;

    /// Whether a response status is eligible for bounded body inspection.
    fn inspect_response_status(&self, status: u16) -> bool;

    /// Classify a bounded upstream response as an invalid credential.
    fn classify_invalid_credential(
        &self,
        status: u16,
        body: &[u8],
    ) -> Option<InvalidCredentialReason>;
}

static LITELLM_ADAPTER: litellm::LiteLlmAdapter = litellm::LiteLlmAdapter;
static GATEWAY_ADAPTERS: [&'static dyn GatewayAdapter; 1] = [&LITELLM_ADAPTER];

/// Return every gateway adapter compiled into this Blue client.
pub fn gateway_registry() -> &'static [&'static dyn GatewayAdapter] {
    &GATEWAY_ADAPTERS
}

fn adapter_in<'a>(
    kind: &str,
    registry: &'a [&'a dyn GatewayAdapter],
) -> Option<&'a dyn GatewayAdapter> {
    registry
        .iter()
        .copied()
        .find(|adapter| adapter.kind() == kind)
}

/// Find a compiled adapter by its stable `gateway.type` key.
pub fn gateway_adapter(kind: &str) -> Option<&'static dyn GatewayAdapter> {
    adapter_in(kind, gateway_registry())
}

/// Return the stable keys supported by this build.
pub fn supported_gateway_types() -> Vec<&'static str> {
    gateway_registry()
        .iter()
        .map(|adapter| adapter.kind())
        .collect()
}

/// Compute common gateway values while the harness adapter supplies its own
/// placement and protocol decisions. This backwards-compatible entry point is
/// the path used by reconciliation and dispatches through [`gateway_registry`].
pub fn wire_with(
    g: &GatewayConfig,
    auth: AuthPlacement,
    wire_api: Option<&str>,
) -> Result<GatewayWiring, GhError> {
    wire_with_registry(g, auth, wire_api, gateway_registry())
}

fn wire_with_registry(
    gateway: &GatewayConfig,
    auth: AuthPlacement,
    wire_api: Option<&str>,
    registry: &[&dyn GatewayAdapter],
) -> Result<GatewayWiring, GhError> {
    if gateway.auth_style != "bearer" {
        return Err(GhError::config(format!(
            "unsupported gateway auth_style `{}` (supported: bearer)",
            gateway.auth_style
        )));
    }
    let Some(adapter) = adapter_in(&gateway.kind, registry) else {
        let supported = registry
            .iter()
            .map(|adapter| adapter.kind())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(GhError::config(format!(
            "unknown gateway type `{}` (supported: {supported})",
            gateway.kind
        )));
    };
    let route = adapter.client_route(gateway)?;

    Ok(GatewayWiring {
        base_url: route.base_url,
        token: route.token,
        wire_api: wire_api.map(str::to_owned),
        auth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gw() -> GatewayConfig {
        GatewayConfig {
            kind: "litellm".into(),
            proxy_url: Some("https://svc/inference/".into()),
            token: Some("jwt.abc.signature".into()),
            auth_style: "bearer".into(),
        }
    }

    #[test]
    fn implementation_can_place_codex_auth_and_responses() {
        let w = wire_with(
            &gw(),
            AuthPlacement::EnvVar(CODEX_ENV_KEY.into()),
            Some("responses"),
        )
        .unwrap();
        assert_eq!(w.base_url, "https://svc/inference"); // trailing slash trimmed
        assert_eq!(w.wire_api.as_deref(), Some("responses"));
        assert_eq!(w.auth, AuthPlacement::EnvVar(CODEX_ENV_KEY.into()));
        assert_eq!(w.token, "jwt.abc.signature");
    }

    #[test]
    fn registry_keys_are_nonempty_and_unique() {
        let mut kinds = std::collections::BTreeSet::new();
        for adapter in gateway_registry() {
            assert!(!adapter.kind().is_empty());
            assert!(adapter.kind().bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            }));
            assert!(
                kinds.insert(adapter.kind()),
                "duplicate gateway adapter `{}`",
                adapter.kind()
            );
        }
        assert_eq!(kinds, std::collections::BTreeSet::from(["litellm"]));
    }

    #[test]
    fn implementation_can_place_auth_in_file() {
        let w = wire_with(&gw(), AuthPlacement::InFile, None).unwrap();
        assert_eq!(w.auth, AuthPlacement::InFile);
        assert!(w.wire_api.is_none());
    }

    #[test]
    fn unsupported_client_auth_style_fails_closed() {
        let mut gateway = gw();
        gateway.auth_style = "x-api-key".into();
        let error = wire_with(&gateway, AuthPlacement::InFile, None).unwrap_err();
        assert!(error.to_string().contains("supported: bearer"));
    }

    #[test]
    fn unknown_gateway_errors() {
        let mut g = gw();
        g.kind = "bedrock".into();
        let error = wire_with(&g, AuthPlacement::InFile, None).unwrap_err();
        assert!(error.to_string().contains("supported: litellm"));
    }

    #[test]
    fn policy_template_without_runtime_values_cannot_be_wired() {
        let mut g = gw();
        g.proxy_url = None;
        g.token = None;
        assert!(wire_with(&g, AuthPlacement::InFile, None).is_err());
    }

    #[test]
    fn debug_output_redacts_inference_tokens() {
        let wiring = wire_with(&gw(), AuthPlacement::InFile, None).unwrap();
        let debug = format!("{wiring:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("jwt.abc.signature"));
        let route = gateway_adapter("litellm")
            .unwrap()
            .client_route(&gw())
            .unwrap();
        assert!(!format!("{route:?}").contains("jwt.abc.signature"));
    }

    #[test]
    fn litellm_declares_complete_upstream_behavior() {
        let adapter = gateway_adapter("litellm").unwrap();
        assert_eq!(
            adapter.upstream_path("/v1/responses?stream=true").unwrap(),
            "/v1/responses?stream=true"
        );
        assert_eq!(
            adapter.upstream_credential_placement(),
            UpstreamCredentialPlacement::AuthorizationBearer
        );
        assert!(adapter.inspect_response_status(401));
        assert!(!adapter.inspect_response_status(403));
        assert_eq!(
            adapter.classify_invalid_credential(
                401,
                br#"{"error":{"type":"token_not_found_in_db"}}"#,
            ),
            Some(InvalidCredentialReason::NotFound)
        );
        assert_eq!(
            adapter.classify_invalid_credential(
                401,
                br#"{"error":{"message":"model is forbidden"}}"#,
            ),
            None
        );
    }

    struct SyntheticAdapter;

    impl GatewayAdapter for SyntheticAdapter {
        fn kind(&self) -> &'static str {
            "synthetic"
        }
        fn client_route(&self, gateway: &GatewayConfig) -> Result<GatewayRoute, GhError> {
            Ok(GatewayRoute {
                base_url: "https://synthetic.test".into(),
                token: gateway.token.clone().unwrap_or_default(),
            })
        }
        fn upstream_path(&self, path_and_query: &str) -> Result<String, GhError> {
            Ok(format!("/gateway{path_and_query}"))
        }
        fn upstream_credential_placement(&self) -> UpstreamCredentialPlacement {
            UpstreamCredentialPlacement::Header("x-synthetic-key")
        }
        fn inspect_response_status(&self, status: u16) -> bool {
            status == 403
        }
        fn classify_invalid_credential(
            &self,
            status: u16,
            _body: &[u8],
        ) -> Option<InvalidCredentialReason> {
            (status == 403).then_some(InvalidCredentialReason::Revoked)
        }
    }

    #[test]
    fn synthetic_adapter_proves_registry_dispatch_and_harness_ownership() {
        static SYNTHETIC: SyntheticAdapter = SyntheticAdapter;
        let registry: [&dyn GatewayAdapter; 2] = [&LITELLM_ADAPTER, &SYNTHETIC];
        let mut gateway = gw();
        gateway.kind = "synthetic".into();
        let wiring = wire_with_registry(
            &gateway,
            AuthPlacement::EnvVar("SYNTHETIC_TOKEN".into()),
            Some("responses"),
            &registry,
        )
        .unwrap();
        assert_eq!(wiring.base_url, "https://synthetic.test");
        assert_eq!(wiring.auth, AuthPlacement::EnvVar("SYNTHETIC_TOKEN".into()));
        assert_eq!(wiring.wire_api.as_deref(), Some("responses"));
    }

    #[test]
    fn invalid_credential_reasons_are_provider_neutral_and_wire_stable() {
        assert_eq!(
            serde_json::to_string(&InvalidCredentialReason::NotFound).unwrap(),
            r#""not_found""#
        );
        assert_eq!(
            serde_json::from_str::<InvalidCredentialReason>(r#""revoked""#).unwrap(),
            InvalidCredentialReason::Revoked
        );
        assert_eq!(
            InvalidCredentialReason::from_legacy_classification("key_blocked"),
            Some(InvalidCredentialReason::Blocked)
        );
        assert_eq!(
            InvalidCredentialReason::Revoked.legacy_classification(),
            None
        );
    }
}
