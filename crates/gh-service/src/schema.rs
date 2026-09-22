//! The versioned governance-config wire schema.
//!
//! This is the client-side half of the service contract (see
//! `deploy/contract/`). Any backend — the reference `services/control-api` or a
//! BYO service — that returns JSON matching these types can drive the harness.
//!
//! The presence of the top-level [`GatewayConfig`] puts every allowed harness
//! in **gateway mode**; omit it for governance-only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Top-level response of `GET {service_url}/governance-config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernanceConfig {
    /// Monotonic-ish revision string; the daemon reconciles when it changes.
    pub revision: String,
    /// Governance wire contract understood by the publisher. Version 1 is the
    /// legacy requirement-based package schema; version 2 adds exact adapter
    /// intervals and capability negotiation.
    #[serde(default = "default_contract_version")]
    pub contract_version: u32,
    /// Features a client must implement before accepting this document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    /// Server-validated certification ceilings. These can only extend a
    /// compiled profile and never participate in implementation selection.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effective_verified_ceilings: BTreeMap<String, BTreeMap<String, String>>,
    /// Operator-visible rollout floor. Capability negotiation, not this field,
    /// enforces whether a client may consume the document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_client_version: Option<String>,
    /// Exact tenant-recommended Blue release exported by the control plane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_client_version: Option<String>,
    /// Client-side cache TTL. `None` ⇒ operator default (see [`Self::DEFAULT_TTL_SECONDS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    /// Harnesses this user/org may launch. The gate keys on this.
    #[serde(default)]
    pub allowed_harnesses: Vec<String>,
    /// Per-harness managed config, keyed by harness key (`"codex"`, ...).
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessPolicy>,
    /// Organization-selected extension packages. Packages are immutable,
    /// digest-pinned archives with explicit per-harness load adapters.
    #[serde(default)]
    pub packages: Vec<ManagedPackage>,
    /// Global inference routing policy. When present, every allowed supported
    /// harness is wired through the same deployment gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayConfig>,
    /// Global portable-session capture policy. When present, every compatible
    /// supported harness registers its native lifecycle hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_upload: Option<SessionUploadConfig>,
    /// Optional metadata-only telemetry sink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryConfig>,
    /// When `true`, the client must fail closed if it has no fresh config
    /// (no falling back to a stale cache).
    #[serde(default)]
    pub required: bool,
}

impl GovernanceConfig {
    pub const DEFAULT_TTL_SECONDS: u64 = 300;
    pub const CONTRACT_VERSION: u32 = 3;
    pub const CAPABILITIES: &'static [&'static str] = &[
        "tenant_client_version_pin",
        "adapter_intervals",
        "compiled_harness_registry",
        "transactional_reconcile",
        "versioned_state",
        "unverified_harness_versions",
        "gateway_inference_jwt",
        "dynamic_verified_ceilings",
    ];

    pub fn ttl_seconds(&self) -> u64 {
        self.ttl_seconds.unwrap_or(Self::DEFAULT_TTL_SECONDS)
    }

    pub fn is_allowed(&self, harness: &str) -> bool {
        self.allowed_harnesses.iter().any(|h| h == harness)
    }

    pub fn policy(&self, harness: &str) -> Option<&HarnessPolicy> {
        self.harnesses.get(harness)
    }

    pub fn validate_client_version_pin(
        required: &str,
    ) -> Result<semver::Version, gh_common::GhError> {
        let version = semver::Version::parse(required).map_err(|_| {
            gh_common::GhError::config("required_client_version must be canonical exact SemVer")
        })?;
        if version.to_string() != required {
            return Err(gh_common::GhError::config(
                "required_client_version must be canonical exact SemVer",
            ));
        }
        Ok(version)
    }

    pub fn ensure_client_compatible(&self) -> Result<(), String> {
        if self.contract_version > Self::CONTRACT_VERSION {
            return Err(format!(
                "governance contract {} is newer than this client's contract {}",
                self.contract_version,
                Self::CONTRACT_VERSION
            ));
        }
        let unsupported = self
            .required_capabilities
            .iter()
            .filter(|capability| !Self::CAPABILITIES.contains(&capability.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !unsupported.is_empty() {
            return Err(format!(
                "governance requires unsupported client capabilities: {}",
                unsupported.join(", ")
            ));
        }
        Ok(())
    }
}

const fn default_contract_version() -> u32 {
    1
}

/// Everything the service governs for a single harness.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessPolicy {
    /// Semver requirement enforced against the locally installed harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_requirement: Option<String>,
    /// Explicitly permit versions newer than Blue's certified ceiling. This
    /// requires `version_requirement` so the accepted risk is explicitly
    /// scoped by an administrator-authored range.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_unverified_versions: bool,
    /// Model / approval / permission flags written into the harness's config.
    #[serde(default)]
    pub managed_config: ManagedConfig,
    #[serde(default)]
    pub mcp: Vec<McpServer>,
    /// Per-harness package enablement and settings. Package source, digest,
    /// version, and component inventory remain organization-controlled.
    #[serde(default)]
    pub package_overrides: BTreeMap<String, PackageOverride>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// An immutable organization-managed extension distribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedPackage {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub version: String,
    /// HTTPS URL or local/file URL to a `.tar.gz` package archive.
    pub source_ref: String,
    /// Organization-scoped mirrored artifact. When present, clients obtain a
    /// fresh authenticated download from the control plane instead of fetching
    /// `source_ref` directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    /// SHA-256 of the exact archive bytes.
    pub sha256: String,
    /// Optional platform-specific archive overrides. When present, clients
    /// require an exact `<os>-<arch>` entry instead of using `source_ref`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub platform_sources: BTreeMap<String, PackageSource>,
    /// Default package configuration, overridden per harness when requested.
    #[serde(default)]
    pub settings: BTreeMap<String, serde_json::Value>,
    /// Load instructions keyed by harness (`codex`, `claude`, `kimi`,
    /// `opencode`). Missing adapters are treated as unsupported.
    #[serde(default)]
    pub adapters: BTreeMap<String, PackageAdapter>,
}

/// One immutable archive used by a platform-specific package distribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageSource {
    pub source_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    pub sha256: String,
}

/// Declarative, non-scripted instructions for loading one package in a
/// harness-owned runtime overlay. Every path is archive-root-relative.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageAdapter {
    /// Inclusive harness-version bound where this package adapter is usable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduced: Option<String>,
    /// Exclusive harness-version bound where this package adapter stops being
    /// usable. These bounds describe availability; `variants` describe layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// Root of an agent-native plugin distribution. Claude loads this with
    /// `--plugin-dir`; Codex registers it in the governed profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_file: Option<String>,
    /// Agent-native plugin modules, primarily for OpenCode.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// Helper executables keyed by the command name exposed to package hooks.
    #[serde(default)]
    pub helpers: BTreeMap<String, PlatformAsset>,
    /// More-specific archive layouts selected by installed harness version.
    /// The top-level fields remain the backward-compatible default variant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<PackageAdapterVariant>,
}

impl PackageAdapter {
    pub fn availability(&self) -> Result<PackageAdapterInterval, String> {
        let introduced = self.introduced.as_deref().unwrap_or("0.0.0");
        let introduced = semver::Version::parse(introduced)
            .map_err(|error| format!("invalid adapter introduced version: {error}"))?;
        let before = self
            .before
            .as_deref()
            .map(semver::Version::parse)
            .transpose()
            .map_err(|error| format!("invalid adapter before version: {error}"))?;
        if before.as_ref().is_some_and(|before| before <= &introduced) {
            return Err("adapter before bound must be greater than introduced".into());
        }
        Ok(PackageAdapterInterval { introduced, before })
    }
}

/// A package layout valid only for a specific harness version range.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PackageAdapterVariant {
    /// Inclusive lower bound of the harness versions using this layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduced: Option<String>,
    /// Exclusive upper bound. Omit for the final open-ended interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// One-release migration input. It is accepted on read but never emitted;
    /// callers use [`PackageAdapterVariant::interval`] to normalize it.
    #[serde(default, skip_serializing)]
    pub version_requirement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_file: Option<String>,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub helpers: BTreeMap<String, PlatformAsset>,
}

impl<'de> Deserialize<'de> for PackageAdapterVariant {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        struct Wire {
            #[serde(default)]
            introduced: Option<String>,
            #[serde(default)]
            before: Option<String>,
            #[serde(default)]
            version_requirement: Option<String>,
            #[serde(default)]
            plugin_dir: Option<String>,
            #[serde(default)]
            skills_dir: Option<String>,
            #[serde(default)]
            agents_dir: Option<String>,
            #[serde(default)]
            hooks_file: Option<String>,
            #[serde(default)]
            plugins: Vec<String>,
            #[serde(default)]
            helpers: BTreeMap<String, PlatformAsset>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let (introduced, before) = if wire.introduced.is_none() && wire.before.is_none() {
            if let Some(requirement) = wire.version_requirement.as_deref() {
                let interval =
                    legacy_requirement_interval(requirement).map_err(serde::de::Error::custom)?;
                (
                    Some(interval.introduced.to_string()),
                    interval.before.map(|value| value.to_string()),
                )
            } else {
                (None, None)
            }
        } else {
            if wire.version_requirement.is_some() {
                return Err(serde::de::Error::custom(
                    "adapter variant cannot mix intervals with version_requirement",
                ));
            }
            (wire.introduced, wire.before)
        };
        Ok(Self {
            introduced,
            before,
            version_requirement: None,
            plugin_dir: wire.plugin_dir,
            skills_dir: wire.skills_dir,
            agents_dir: wire.agents_dir,
            hooks_file: wire.hooks_file,
            plugins: wire.plugins,
            helpers: wire.helpers,
        })
    }
}

impl PackageAdapterVariant {
    pub fn interval(&self) -> Result<PackageAdapterInterval, String> {
        if self.introduced.is_some() || self.before.is_some() {
            if self.version_requirement.is_some() {
                return Err("adapter variant cannot mix intervals with version_requirement".into());
            }
            let introduced = self
                .introduced
                .as_deref()
                .ok_or_else(|| "adapter interval requires an introduced lower bound".to_string())?;
            let introduced = semver::Version::parse(introduced)
                .map_err(|error| format!("invalid introduced version `{introduced}`: {error}"))?;
            let before = self
                .before
                .as_deref()
                .map(semver::Version::parse)
                .transpose()
                .map_err(|error| format!("invalid before version: {error}"))?;
            if before.as_ref().is_some_and(|before| before <= &introduced) {
                return Err("adapter interval before bound must be greater than introduced".into());
            }
            return Ok(PackageAdapterInterval { introduced, before });
        }
        let requirement = self.version_requirement.as_deref().ok_or_else(|| {
            "adapter variant requires introduced (and optional before)".to_string()
        })?;
        legacy_requirement_interval(requirement)
    }

    pub fn as_adapter(&self) -> PackageAdapter {
        PackageAdapter {
            introduced: None,
            before: None,
            plugin_dir: self.plugin_dir.clone(),
            skills_dir: self.skills_dir.clone(),
            agents_dir: self.agents_dir.clone(),
            hooks_file: self.hooks_file.clone(),
            plugins: self.plugins.clone(),
            helpers: self.helpers.clone(),
            variants: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageAdapterInterval {
    pub introduced: semver::Version,
    pub before: Option<semver::Version>,
}

impl PackageAdapterInterval {
    pub fn contains(&self, version: &semver::Version) -> bool {
        version >= &self.introduced && self.before.as_ref().is_none_or(|before| version < before)
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.before
            .as_ref()
            .is_none_or(|before| before > &other.introduced)
            && other
                .before
                .as_ref()
                .is_none_or(|before| before > &self.introduced)
    }
}

/// Normalize the intentionally supported legacy subset: `>=A` with optional
/// `,<B`. Other semver syntax can describe holes or implicit prerelease rules
/// and is rejected instead of being approximated.
pub fn legacy_requirement_interval(requirement: &str) -> Result<PackageAdapterInterval, String> {
    let mut introduced = None;
    let mut before = None;
    for raw in requirement.split(',') {
        let comparator = raw.trim();
        if let Some(value) = comparator.strip_prefix(">=") {
            if introduced.is_some() {
                return Err("legacy requirement has multiple lower bounds".into());
            }
            introduced = Some(
                semver::Version::parse(value.trim())
                    .map_err(|error| format!("invalid lower bound: {error}"))?,
            );
        } else if let Some(value) = comparator.strip_prefix('<') {
            if comparator.starts_with("<=") || before.is_some() {
                return Err("legacy requirement must use one exclusive upper bound".into());
            }
            before = Some(
                semver::Version::parse(value.trim())
                    .map_err(|error| format!("invalid upper bound: {error}"))?,
            );
        } else {
            return Err(format!(
                "legacy requirement `{requirement}` is not one contiguous half-open interval"
            ));
        }
    }
    let introduced = introduced
        .ok_or_else(|| "legacy requirement requires an inclusive >= lower bound".to_string())?;
    if before.as_ref().is_some_and(|before| before <= &introduced) {
        return Err("legacy requirement upper bound must be greater than lower bound".into());
    }
    Ok(PackageAdapterInterval { introduced, before })
}

/// Platform-specific executable paths included in the verified archive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlatformAsset {
    /// Keys are `<os>-<arch>` (for example `macos-aarch64`) with optional
    /// `default` fallback. Values are archive-root-relative paths.
    #[serde(default)]
    pub paths: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageOverride {
    /// Defaults to enabled when the package declares an adapter for the
    /// harness. Set false to opt this harness out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub settings: BTreeMap<String, serde_json::Value>,
}

/// Provider-neutral session artifact upload configuration.
///
/// The client POSTs session metadata to `presign_url` using its service bearer
/// token. The endpoint responds with a [`PresignedUpload`], commonly an AWS S3
/// presigned PUT today, but no storage-provider details are exposed here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUploadConfig {
    pub presign_url: String,
}

/// Response returned by `SessionUploadConfig::presign_url`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresignedUpload {
    pub upload_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_url: Option<String>,
    /// HTTP method used for the raw blob. Defaults to `PUT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Provider-required headers (for example an S3 checksum header).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complete_url: Option<String>,
}

/// Managed model/approval config. Known keys are typed; anything else is
/// preserved in `extra` so the schema can grow without a client release.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManagedConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// e.g. `"never"`, `"on-request"` — semantics are per-harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    /// e.g. codex `sandbox_mode`; opaque string passed through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    /// Whether the harness may auto-approve edits/commands (yolo-style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approve: Option<bool>,
    /// Model thinking/reasoning effort (for example `medium`). Harnesses that
    /// do not expose an equivalent setting ignore this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Whether the harness may expose or select its accelerated service tier.
    /// Codex maps this to `features.fast_mode` and resets the preferred tier
    /// to `default` when the policy disables Fast mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Gateway routing block. `type` selects the compiled wiring behavior in
/// `gh-gateway`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// Adapter key. Only `"litellm"` is supported today.
    #[serde(rename = "type")]
    pub kind: String,
    /// The upstream inference proxy the agent points at (not LiteLLM directly).
    /// Runtime-only: the control API injects this when delivering a config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Session-bound inference JWT. Runtime-only: the control API injects this
    /// for the authenticated user and never persists the encoded token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// How the inference JWT is presented to the proxy. `"bearer"` is the
    /// only supported value.
    #[serde(default = "default_auth_style")]
    pub auth_style: String,
}

fn default_auth_style() -> String {
    "bearer".to_string()
}

/// One MCP server entry. Either a stdio `command` or a remote `url`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub sink_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governance_only_config_has_no_gateway() {
        let json = r#"{
            "revision": "r1",
            "allowed_harnesses": ["codex", "claude"],
            "harnesses": {
                "codex": { "managed_config": { "model": "gpt-5", "approval_policy": "never" } }
            }
        }"#;
        let cfg: GovernanceConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.is_allowed("codex"));
        assert!(!cfg.is_allowed("kimi"));
        assert!(cfg.gateway.is_none());
        let codex = cfg.policy("codex").unwrap();
        assert_eq!(codex.managed_config.model.as_deref(), Some("gpt-5"));
        assert_eq!(cfg.ttl_seconds(), GovernanceConfig::DEFAULT_TTL_SECONDS);
    }

    #[test]
    fn minimum_client_version_is_visibility_metadata() {
        let config: GovernanceConfig = serde_json::from_value(serde_json::json!({
            "revision": "r1",
            "minimum_client_version": "999.0.0"
        }))
        .unwrap();
        assert!(config.ensure_client_compatible().is_ok());
    }

    #[test]
    fn gateway_config_round_trips() {
        let json = r#"{
            "revision": "r2",
            "allowed_harnesses": ["claude"],
            "gateway": {
                "type": "litellm",
                "proxy_url": "https://svc/inference",
                "token": "jwt.abc.signature"
            },
            "harnesses": {
                "claude": {
                    "managed_config": { "model": "claude-opus-4-8" }
                }
            }
        }"#;
        let cfg: GovernanceConfig = serde_json::from_str(json).unwrap();
        let g = cfg.gateway.as_ref().unwrap();
        assert_eq!(g.kind, "litellm");
        assert_eq!(g.auth_style, "bearer");
        assert_eq!(g.token.as_deref(), Some("jwt.abc.signature"));
    }

    #[test]
    fn gateway_policy_template_does_not_require_runtime_credentials() {
        let yaml = r#"
revision: r3
allowed_harnesses: [codex]
gateway:
  type: litellm
harnesses:
  codex:
    managed_config:
      model: gpt-5.6-sol
"#;
        let cfg: GovernanceConfig = serde_yaml::from_str(yaml).unwrap();
        let gateway = cfg.gateway.as_ref().unwrap();
        assert!(gateway.proxy_url.is_none());
        assert!(gateway.token.is_none());

        let serialized = serde_yaml::to_string(&cfg).unwrap();
        assert!(!serialized.contains("proxy_url"));
        assert!(!serialized.contains("token"));
    }

    #[test]
    fn managed_config_preserves_unknown_keys() {
        let mc: ManagedConfig =
            serde_json::from_str(r#"{ "model": "x", "future_flag": true }"#).unwrap();
        assert_eq!(mc.model.as_deref(), Some("x"));
        assert_eq!(
            mc.extra.get("future_flag").unwrap(),
            &serde_json::json!(true)
        );
    }

    #[test]
    fn session_upload_is_opt_in_and_provider_neutral() {
        let config: GovernanceConfig = serde_json::from_str(
            r#"{
                "revision": "r1",
                "allowed_harnesses": ["codex", "claude", "kimi", "opencode"],
                "session_upload": { "presign_url": "https://control.example/uploads" }
            }"#,
        )
        .unwrap();
        assert_eq!(
            config.session_upload.unwrap().presign_url,
            "https://control.example/uploads"
        );

        let response: PresignedUpload = serde_json::from_str(
            r#"{ "upload_id": "abc", "status": "pending", "upload_url": "https://blob.example/object" }"#,
        )
        .unwrap();
        assert_eq!(response.method, None);
        assert!(response.headers.is_empty());
    }

    #[test]
    fn managed_packages_and_overrides_round_trip() {
        let config: GovernanceConfig = serde_yaml::from_str(
            r#"
revision: package-r1
allowed_harnesses: [claude]
packages:
  - id: review-kit
    version: "1.2.3"
    source_ref: https://packages.example/review-kit.tar.gz
    artifact_id: 853ea39a-563b-4d1e-a428-22b46068e125
    sha256: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    adapters:
      claude:
        plugin_dir: plugin
harnesses:
  claude:
    package_overrides:
      review-kit:
        enabled: true
        settings:
          mode: strict
"#,
        )
        .unwrap();
        assert_eq!(config.packages[0].id, "review-kit");
        assert_eq!(
            config.packages[0].artifact_id.as_deref(),
            Some("853ea39a-563b-4d1e-a428-22b46068e125")
        );
        assert_eq!(
            config.harnesses["claude"].package_overrides["review-kit"].settings["mode"],
            serde_json::json!("strict")
        );
        let json = serde_json::to_value(config).unwrap();
        assert_eq!(
            json["packages"][0]["adapters"]["claude"]["plugin_dir"],
            "plugin"
        );
    }

    #[test]
    fn legacy_variant_is_normalized_and_only_interval_is_emitted() {
        let variant: PackageAdapterVariant = serde_json::from_value(serde_json::json!({
            "version_requirement": ">=1.2.0-beta.1, <2.0.0",
            "skills_dir": "skills"
        }))
        .unwrap();
        let interval = variant.interval().unwrap();
        assert!(interval.contains(&semver::Version::parse("1.2.0-beta.1").unwrap()));
        assert!(!interval.contains(&semver::Version::new(2, 0, 0)));
        let encoded = serde_json::to_value(variant).unwrap();
        assert_eq!(encoded["introduced"], "1.2.0-beta.1");
        assert_eq!(encoded["before"], "2.0.0");
        assert!(encoded.get("version_requirement").is_none());
        assert!(
            serde_json::from_value::<PackageAdapterVariant>(serde_json::json!({
                "version_requirement": "^1.2.0"
            }))
            .is_err()
        );
    }

    #[test]
    fn package_adapter_availability_round_trips() {
        let adapter: PackageAdapter = serde_json::from_value(serde_json::json!({
            "introduced": "2.0.12",
            "before": "3.0.0-0",
            "plugin_dir": "plugin"
        }))
        .unwrap();
        let interval = adapter.availability().unwrap();
        assert!(interval.contains(&semver::Version::new(2, 0, 12)));
        assert!(!interval.contains(&semver::Version::parse("3.0.0-0").unwrap()));
        let encoded = serde_json::to_value(adapter).unwrap();
        assert_eq!(encoded["introduced"], "2.0.12");
        assert_eq!(encoded["before"], "3.0.0-0");
    }

    #[test]
    fn capability_negotiation_rejects_unknown_contracts_and_features() {
        let mut config: GovernanceConfig = serde_json::from_value(serde_json::json!({
            "revision": "r1", "contract_version": 999
        }))
        .unwrap();
        assert!(config
            .ensure_client_compatible()
            .unwrap_err()
            .contains("newer"));
        config.contract_version = GovernanceConfig::CONTRACT_VERSION;
        config.required_capabilities = vec!["future_feature".into()];
        assert!(config
            .ensure_client_compatible()
            .unwrap_err()
            .contains("future_feature"));
    }

    #[test]
    fn unverified_version_opt_in_is_explicit_on_the_wire() {
        let default = serde_json::to_value(HarnessPolicy::default()).unwrap();
        assert!(default.get("allow_unverified_versions").is_none());

        let policy: HarnessPolicy = serde_yaml::from_str(
            "version_requirement: '=0.151.1'\nallow_unverified_versions: true\n",
        )
        .unwrap();
        assert!(policy.allow_unverified_versions);
        assert_eq!(policy.version_requirement.as_deref(), Some("=0.151.1"));
        assert_eq!(
            serde_json::to_value(policy).unwrap()["allow_unverified_versions"],
            true
        );
    }
}
