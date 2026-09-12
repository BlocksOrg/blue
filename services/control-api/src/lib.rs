mod blob;
mod db;
mod executable_provisioner;
mod gateway_auth;
mod scim;
mod secrets;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use gh_gateway_provisioner::{GatewayProvisioner, ProvisionerError};
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{
    decode, decode_header, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgListener;
use sqlx::{FromRow, PgConnection, PgPool, Postgres, QueryBuilder};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, Month, OffsetDateTime};
use uuid::Uuid;

use blob::BlobStore;
use secrets::{Envelope, SecretProtector};

static INTERNAL_TLS_RELOAD_SUCCESSES: AtomicU64 = AtomicU64::new(0);
static INTERNAL_TLS_RELOAD_ERRORS: AtomicU64 = AtomicU64::new(0);
static WORKER_LAST_SUCCESS_UNIX: AtomicU64 = AtomicU64::new(0);

fn record_worker_success() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    WORKER_LAST_SUCCESS_UNIX.store(now, Ordering::Relaxed);
}

/// Finish the pre-envelope upgrade for installations that jump directly from a
/// release which stored the managed upstream credential in `proxy_virtual_key`.
/// Rows are claimed independently so concurrent serving replicas cannot encrypt
/// or replace the same credential twice.
async fn migrate_legacy_gateway_credentials(
    pool: &PgPool,
    protector: &SecretProtector,
) -> Result<(), ApiError> {
    loop {
        let mut transaction = pool.begin().await?;
        let row = sqlx::query!(
            "select user_id,proxy_virtual_key from public.gateway_key_selections where proxy_virtual_key is not null and credential_ciphertext is null for update skip locked limit 1"
        )
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(());
        };
        let credential = row
            .proxy_virtual_key
            .ok_or_else(|| ApiError::internal("legacy gateway credential disappeared"))?;
        let envelope = protector.encrypt(credential.as_bytes()).await?;
        let updated = sqlx::query!(
            "update public.gateway_key_selections set credential_ciphertext=$1,credential_nonce=$2,credential_wrapped_key=$3,encryption_key_id=$4,credential_version=gen_random_uuid(),proxy_virtual_key=null,last_reconciled_at=coalesce(last_reconciled_at,updated_at),updated_at=now() where user_id=$5 and proxy_virtual_key is not null and credential_ciphertext is null",
            envelope.ciphertext,
            envelope.nonce,
            envelope.wrapped_key,
            envelope.key_id,
            row.user_id,
        )
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(ApiError::internal(
                "legacy gateway credential changed during encryption",
            ));
        }
        transaction.commit().await?;
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayProvisionerConfig {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default = "default_reconcile_ttl")]
    pub reconcile_ttl_seconds: i64,
    #[serde(default)]
    pub executable_path: Option<PathBuf>,
    #[serde(default)]
    pub executable_sha256: Option<String>,
    #[serde(default)]
    pub policy_revision: Option<String>,
    #[serde(default = "default_provisioner_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_provisioner_max_concurrency")]
    pub max_concurrency: usize,
}

fn default_reconcile_ttl() -> i64 {
    86_400
}
fn default_provisioner_timeout() -> u64 {
    15
}
fn default_provisioner_max_concurrency() -> usize {
    8
}
#[derive(Clone, Debug, Deserialize)]
pub struct GatewayEncryptionConfig {
    pub provider: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub key_id: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct PackageSourceConnection {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub api_base_url: Option<String>,
    #[serde(default)]
    pub web_base_url: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub private_key: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub ca_bundle: Option<String>,
    #[serde(default)]
    pub download_hosts: Vec<String>,
    #[serde(default)]
    pub organizations: BTreeMap<String, Vec<String>>,
}

#[derive(Clone)]
pub struct AppConfig {
    pub database_url: String,
    pub listen: String,
    pub internal_listen: String,
    pub internal_transport_mode: String,
    pub internal_tls_cert_file: Option<PathBuf>,
    pub internal_tls_key_file: Option<PathBuf>,
    pub internal_tls_client_ca_file: Option<PathBuf>,
    pub database_max_connections: u32,
    pub gateway_log_database_max_connections: u32,
    pub gateway_kms_max_concurrency: usize,
    pub gateway_kms_timeout_seconds: u64,
    pub run_background_jobs: bool,
    pub blue_config_file: PathBuf,
    pub blue_config_overlay_files: Vec<PathBuf>,
    pub bootstrap_org: String,
    pub bootstrap_org_name: String,
    pub bootstrap_admin_sub: String,
    pub bootstrap_admin_email: String,
    pub session_retention_days: i64,
    pub gateway_request_log_retention_days: i64,
    pub s3_bucket: String,
    pub s3_package_bucket: String,
    pub s3_region: String,
    pub s3_endpoint: Option<String>,
    pub s3_public_endpoint: Option<String>,
    pub s3_force_path_style: bool,
    pub blob_presign_ttl_seconds: u64,
    pub auth_session_url: String,
    pub auth_jwks_url: String,
    pub auth_issuer: String,
    pub auth_audience: String,
    pub auth_client_id: String,
    pub auth_public_url: String,
    pub auth_mode: String,
    pub scim_bearer_token: Option<String>,
    pub scim_group_role_mappings: BTreeMap<String, String>,
    pub gateway_kind: Option<String>,
    pub gateway_upstream_url: Option<String>,
    pub gateway_inference_proxy_url: Option<String>,
    pub gateway_inference_proxy_health_url: Option<String>,
    pub gateway_jwt_issuer: String,
    pub gateway_jwt_audience: String,
    /// Lifetime of a minted inference JWT, clamped to the remaining browser
    /// session. A shorter TTL is the better security answer, but the token is
    /// baked into the agent's process environment at spawn and there is no
    /// in-flight rotation, so shortening it shortens the usable agent run.
    pub gateway_inference_token_ttl_seconds: u64,
    pub gateway_jwt_active_kid: Option<String>,
    pub gateway_jwt_private_key_file: Option<PathBuf>,
    pub gateway_jwt_jwks_file: Option<PathBuf>,
    pub gateway_jwt_private_key_pem: Option<String>,
    pub gateway_jwt_jwks_json: Option<String>,
    pub internal_allowed_client_id: String,
    pub gateway_provisioner: Option<GatewayProvisionerConfig>,
    pub gateway_encryption: Option<GatewayEncryptionConfig>,
    pub package_source_connections: Vec<PackageSourceConnection>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ApiError> {
        let (blue_config_file, blue_config_overlay_files, settings) = load_blue_settings()?;
        let settings = Some(settings);
        Ok(Self {
            database_url: required_setting(
                "HARNESS_DATABASE_URL",
                &settings,
                &["control_api", "database_url"],
            )?,
            listen: setting_or(
                "HARNESS_LISTEN",
                &settings,
                &["control_api", "listen"],
                "0.0.0.0:8080",
            )?,
            internal_listen: setting_or(
                "HARNESS_INTERNAL_LISTEN",
                &settings,
                &["control_api", "internal_listen"],
                "127.0.0.1:8082",
            )?,
            internal_transport_mode: std::env::var("HARNESS_INTERNAL_TRANSPORT_MODE")
                .unwrap_or_else(|_| "mtls".into()),
            internal_tls_cert_file: std::env::var("HARNESS_INTERNAL_TLS_CERT_FILE")
                .ok()
                .map(PathBuf::from),
            internal_tls_key_file: std::env::var("HARNESS_INTERNAL_TLS_KEY_FILE")
                .ok()
                .map(PathBuf::from),
            internal_tls_client_ca_file: std::env::var("HARNESS_INTERNAL_TLS_CLIENT_CA_FILE")
                .ok()
                .map(PathBuf::from),
            database_max_connections: positive_setting(
                "HARNESS_DATABASE_MAX_CONNECTIONS",
                &settings,
                &["control_api", "database", "max_connections"],
                30,
            )? as u32,
            gateway_log_database_max_connections: positive_setting(
                "HARNESS_GATEWAY_LOG_DATABASE_MAX_CONNECTIONS",
                &settings,
                &["control_api", "gateway_request_logs", "max_connections"],
                5,
            )? as u32,
            gateway_kms_max_concurrency: positive_setting(
                "HARNESS_GATEWAY_KMS_MAX_CONCURRENCY",
                &settings,
                &["control_api", "gateway", "kms_max_concurrency"],
                32,
            )? as usize,
            gateway_kms_timeout_seconds: positive_setting(
                "HARNESS_GATEWAY_KMS_TIMEOUT_SECONDS",
                &settings,
                &["control_api", "gateway", "kms_timeout_seconds"],
                3,
            )? as u64,
            run_background_jobs: parse_bool_setting(&setting_or(
                "HARNESS_RUN_BACKGROUND_JOBS",
                &settings,
                &["control_api", "run_background_jobs"],
                "true",
            )?)?,
            blue_config_file,
            blue_config_overlay_files,
            bootstrap_org: setting_or(
                "HARNESS_BOOTSTRAP_ORG_ID",
                &settings,
                &["control_api", "bootstrap", "organization_slug"],
                "dev",
            )?,
            bootstrap_org_name: setting_or(
                "HARNESS_BOOTSTRAP_ORG_NAME",
                &settings,
                &["control_api", "bootstrap", "organization_name"],
                "Development",
            )?,
            bootstrap_admin_sub: setting_or(
                "HARNESS_BOOTSTRAP_ADMIN_SUB",
                &settings,
                &["control_api", "bootstrap", "admin_subject"],
                "admin",
            )?,
            bootstrap_admin_email: setting_or(
                "HARNESS_BOOTSTRAP_ADMIN_EMAIL",
                &settings,
                &["control_api", "bootstrap", "admin_email"],
                "admin@example.com",
            )?,
            session_retention_days: positive_setting(
                "HARNESS_SESSION_RETENTION_DAYS",
                &settings,
                &["control_api", "blob_storage", "retention_days"],
                30,
            )?,
            gateway_request_log_retention_days: positive_setting(
                "HARNESS_GATEWAY_REQUEST_LOG_RETENTION_DAYS",
                &settings,
                &["control_api", "gateway_request_logs", "retention_days"],
                30,
            )?,
            s3_bucket: required_setting(
                "HARNESS_BLOB_BUCKET",
                &settings,
                &["control_api", "blob_storage", "bucket"],
            )?,
            s3_package_bucket: setting_or(
                "HARNESS_PACKAGE_BUCKET",
                &settings,
                &["control_api", "package_artifacts", "bucket"],
                "package-artifacts",
            )?,
            s3_region: setting_or(
                "HARNESS_BLOB_REGION",
                &settings,
                &["control_api", "blob_storage", "region"],
                "us-east-1",
            )?,
            s3_endpoint: env_or_setting(
                "HARNESS_BLOB_ENDPOINT",
                &settings,
                &["control_api", "blob_storage", "endpoint"],
            )?,
            s3_public_endpoint: env_or_setting(
                "HARNESS_BLOB_PUBLIC_ENDPOINT",
                &settings,
                &["control_api", "blob_storage", "public_endpoint"],
            )?,
            s3_force_path_style: parse_bool_setting(&setting_or(
                "HARNESS_BLOB_FORCE_PATH_STYLE",
                &settings,
                &["control_api", "blob_storage", "force_path_style"],
                "false",
            )?)?,
            blob_presign_ttl_seconds: positive_setting(
                "HARNESS_BLOB_PRESIGN_TTL_SECONDS",
                &settings,
                &["control_api", "blob_storage", "presign_ttl_seconds"],
                300,
            )? as u64,
            auth_session_url: setting_or(
                "HARNESS_AUTH_SESSION_URL",
                &settings,
                &["control_api", "auth", "session_url"],
                "http://127.0.0.1:3000/api/auth/get-session",
            )?,
            auth_jwks_url: setting_or(
                "HARNESS_AUTH_JWKS_URL",
                &settings,
                &["control_api", "auth", "jwks_url"],
                "http://127.0.0.1:3000/api/auth/jwks",
            )?,
            auth_issuer: setting_or(
                "HARNESS_AUTH_ISSUER",
                &settings,
                &["control_api", "auth", "issuer"],
                "http://127.0.0.1:3000/api/auth",
            )?,
            auth_audience: setting_or(
                "HARNESS_AUTH_AUDIENCE",
                &settings,
                &["control_api", "auth", "audience"],
                "http://127.0.0.1:8080",
            )?,
            auth_client_id: setting_or(
                "HARNESS_OAUTH_CLIENT_ID",
                &settings,
                &["control_api", "auth", "client_id"],
                "blue-cli",
            )?,
            auth_public_url: setting_or(
                "HARNESS_AUTH_PUBLIC_URL",
                &settings,
                &["control_api", "auth", "public_url"],
                "http://127.0.0.1:3000",
            )?,
            auth_mode: identity_mode(&setting_or(
                "HARNESS_AUTH_MODE",
                &settings,
                &["control_api", "identity", "mode"],
                "password",
            )?)?,
            scim_bearer_token: env_or_setting(
                "HARNESS_SCIM_BEARER_TOKEN",
                &settings,
                &["control_api", "identity", "scim_bearer_token"],
            )?
            .filter(|value| !value.trim().is_empty()),
            scim_group_role_mappings: role_mappings(
                env_or_setting(
                    "HARNESS_SCIM_GROUP_ROLE_MAPPINGS",
                    &settings,
                    &["control_api", "identity", "group_role_mappings"],
                )?
                .as_deref(),
            )?,
            gateway_kind: gateway_kind_setting(&settings)?,
            gateway_upstream_url: optional_env_reference_setting(
                "HARNESS_GATEWAY_URL",
                &settings,
                &["gateway", "url"],
            )?
            .or(optional_env_reference_setting(
                "HARNESS_LITELLM_BASE_URL",
                &settings,
                &["gateway", "litellm_base_url"],
            )?)
            .filter(|value| !value.trim().is_empty()),
            gateway_inference_proxy_url: optional_env_reference_setting(
                "HARNESS_INFERENCE_PROXY_URL",
                &settings,
                &["gateway", "inference_proxy_url"],
            )?
            .filter(|value| !value.trim().is_empty()),
            gateway_inference_proxy_health_url: env_or_setting(
                "HARNESS_INFERENCE_PROXY_HEALTH_URL",
                &settings,
                &["gateway", "inference_proxy_health_url"],
            )?
            .filter(|value| !value.trim().is_empty()),
            gateway_jwt_issuer: setting_or(
                "HARNESS_GATEWAY_JWT_ISSUER",
                &settings,
                &["gateway", "inference_jwt", "issuer"],
                "http://127.0.0.1:8080",
            )?,
            gateway_jwt_audience: setting_or(
                "HARNESS_GATEWAY_JWT_AUDIENCE",
                &settings,
                &["gateway", "inference_jwt", "audience"],
                "blue-inference-proxy",
            )?,
            gateway_inference_token_ttl_seconds: positive_setting(
                "HARNESS_GATEWAY_INFERENCE_TOKEN_TTL_SECONDS",
                &settings,
                &["gateway", "inference_jwt", "token_ttl_seconds"],
                43_200,
            )? as u64,
            gateway_jwt_active_kid: env_or_setting(
                "HARNESS_GATEWAY_JWT_ACTIVE_KID",
                &settings,
                &["gateway", "inference_jwt", "active_kid"],
            )?
            .filter(|value| !value.trim().is_empty()),
            gateway_jwt_private_key_file: env_or_setting(
                "HARNESS_GATEWAY_JWT_PRIVATE_KEY_FILE",
                &settings,
                &["gateway", "inference_jwt", "private_key_file"],
            )?
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from),
            gateway_jwt_jwks_file: env_or_setting(
                "HARNESS_GATEWAY_JWT_JWKS_FILE",
                &settings,
                &["gateway", "inference_jwt", "jwks_file"],
            )?
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from),
            gateway_jwt_private_key_pem: std::env::var("HARNESS_GATEWAY_JWT_PRIVATE_KEY_PEM")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            gateway_jwt_jwks_json: std::env::var("HARNESS_GATEWAY_JWT_JWKS_JSON")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            internal_allowed_client_id: setting_or(
                "HARNESS_INTERNAL_ALLOWED_CLIENT_ID",
                &settings,
                &["gateway", "internal_allowed_client_id"],
                "blue-inference-proxy",
            )?,
            gateway_provisioner: gateway_provisioner(&settings)?,
            gateway_encryption: gateway_encryption(&settings)?,
            package_source_connections: package_source_connections(&settings)?,
        }
        .with_gateway_gated_on_type())
    }

    /// `gateway.type` is the single switch for gateway mode. With no type,
    /// ignore every other gateway setting (YAML or env) so a governance-only
    /// deployment can't be tripped into a partial-gateway state — e.g. a
    /// compose file that injects `HARNESS_INFERENCE_PROXY_URL` unconditionally.
    fn with_gateway_gated_on_type(mut self) -> Self {
        if self.gateway_kind.is_none() {
            let had_runtime = self.gateway_upstream_url.is_some()
                || self.gateway_inference_proxy_url.is_some()
                || self.gateway_inference_proxy_health_url.is_some()
                || self.gateway_jwt_active_kid.is_some()
                || self.gateway_jwt_private_key_file.is_some()
                || self.gateway_jwt_jwks_file.is_some()
                || self.gateway_jwt_private_key_pem.is_some()
                || self.gateway_jwt_jwks_json.is_some()
                || self.gateway_provisioner.is_some()
                || self.gateway_encryption.is_some();
            if had_runtime {
                tracing::warn!(
                    "gateway.type is unset; ignoring gateway runtime settings \
                     (inference_proxy_url, url, provisioner, secret_encryption) \
                     and running governance-only"
                );
            }
            self.gateway_upstream_url = None;
            self.gateway_inference_proxy_url = None;
            self.gateway_inference_proxy_health_url = None;
            self.gateway_jwt_active_kid = None;
            self.gateway_jwt_private_key_file = None;
            self.gateway_jwt_jwks_file = None;
            self.gateway_jwt_private_key_pem = None;
            self.gateway_jwt_jwks_json = None;
            self.gateway_provisioner = None;
            self.gateway_encryption = None;
        }
        self
    }

    /// Validate the complete opt-in gateway envelope before any network or
    /// database side effects. Report configuration names only so startup logs
    /// can be shared without disclosing secret values.
    fn validate_gateway_runtime(&self) -> Result<(), ApiError> {
        if self.gateway_kind.is_none() {
            return Ok(());
        }

        let blank = |value: Option<&str>| value.is_none_or(|value| value.trim().is_empty());
        let mut missing = Vec::new();
        if blank(self.gateway_upstream_url.as_deref()) {
            missing.push("gateway.url (or HARNESS_GATEWAY_URL)");
        }
        if blank(self.gateway_inference_proxy_url.as_deref()) {
            missing.push("gateway.inference_proxy_url (or HARNESS_INFERENCE_PROXY_URL)");
        }
        if self.internal_allowed_client_id.trim().is_empty() {
            missing.push("gateway.internal_allowed_client_id");
        }
        if self.gateway_jwt_issuer.trim().is_empty() {
            missing.push("gateway.inference_jwt.issuer");
        }
        if self.gateway_jwt_audience.trim().is_empty() {
            missing.push("gateway.inference_jwt.audience");
        }
        if self.gateway_jwt_active_kid.is_none() {
            missing.push("gateway.inference_jwt.active_kid (or HARNESS_GATEWAY_JWT_ACTIVE_KID)");
        }
        if self.gateway_jwt_private_key_file.is_none() && self.gateway_jwt_private_key_pem.is_none()
        {
            missing.push(
                "gateway.inference_jwt.private_key_file (or HARNESS_GATEWAY_JWT_PRIVATE_KEY_PEM)",
            );
        }
        if self.gateway_jwt_jwks_file.is_none() && self.gateway_jwt_jwks_json.is_none() {
            missing.push("gateway.inference_jwt.jwks_file (or HARNESS_GATEWAY_JWT_JWKS_JSON)");
        }
        if self.gateway_provisioner.is_none() {
            missing.push("gateway.provisioner");
        }
        match self.gateway_encryption.as_ref() {
            None => missing.push("gateway.secret_encryption"),
            Some(encryption)
                if encryption.provider == "environment" && blank(encryption.key.as_deref()) =>
            {
                missing.push("gateway.secret_encryption.key (or HARNESS_GATEWAY_ENCRYPTION_KEY)");
            }
            Some(encryption)
                if encryption.provider == "aws-kms" && blank(encryption.key_id.as_deref()) =>
            {
                missing.push("gateway.secret_encryption.key_id");
            }
            Some(_) => {}
        }

        if missing.is_empty() {
            Ok(())
        } else {
            Err(ApiError::internal(format!(
                "invalid gateway runtime configuration:\n{}",
                missing
                    .into_iter()
                    .map(|name| format!("- {name} is required"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )))
        }
    }
}

fn gateway_provisioner(
    settings: &Option<serde_yaml::Value>,
) -> Result<Option<GatewayProvisionerConfig>, ApiError> {
    let Some(value) = settings
        .as_ref()
        .and_then(|v| v.get("gateway"))
        .and_then(|v| v.get("provisioner"))
    else {
        return Ok(None);
    };
    let mut config: GatewayProvisionerConfig = serde_yaml::from_value(value.clone())
        .map_err(|e| ApiError::internal(format!("parsing gateway.provisioner: {e}")))?;
    config.executable_path = env_or_setting(
        "HARNESS_PROVISIONER_EXECUTABLE_PATH",
        settings,
        &["gateway", "provisioner", "executable_path"],
    )?
    .map(PathBuf::from);
    config.executable_sha256 = env_or_setting(
        "HARNESS_PROVISIONER_EXECUTABLE_SHA256",
        settings,
        &["gateway", "provisioner", "executable_sha256"],
    )?;
    if config.kind.trim().is_empty() {
        return Err(ApiError::internal(
            "gateway.provisioner.type must not be empty",
        ));
    }
    if config.reconcile_ttl_seconds <= 0 {
        return Err(ApiError::internal(
            "gateway.provisioner.reconcile_ttl_seconds must be greater than zero",
        ));
    }
    if config.timeout_seconds == 0 {
        return Err(ApiError::internal(
            "gateway.provisioner.timeout_seconds must be greater than zero",
        ));
    }
    if config.max_concurrency == 0 {
        return Err(ApiError::internal(
            "gateway.provisioner.max_concurrency must be greater than zero",
        ));
    }
    Ok(Some(config))
}

fn gateway_encryption(
    settings: &Option<serde_yaml::Value>,
) -> Result<Option<GatewayEncryptionConfig>, ApiError> {
    let value = settings
        .as_ref()
        .and_then(|v| v.get("gateway"))
        .and_then(|v| v.get("secret_encryption"));
    let mut config: Option<GatewayEncryptionConfig> = value
        .map(|v| {
            serde_yaml::from_value(v.clone())
                .map_err(|e| ApiError::internal(format!("parsing gateway.secret_encryption: {e}")))
        })
        .transpose()?;
    if let Some(encryption) = config.as_mut() {
        match encryption.provider.as_str() {
            "environment" => {
                encryption.key = optional_env_reference_setting(
                    "HARNESS_GATEWAY_ENCRYPTION_KEY",
                    settings,
                    &["gateway", "secret_encryption", "key"],
                )?;
            }
            "aws-kms" => {
                encryption.key_id = optional_env_reference_setting(
                    "HARNESS_GATEWAY_KMS_KEY_ID",
                    settings,
                    &["gateway", "secret_encryption", "key_id"],
                )?;
            }
            _ => {}
        }
    }
    Ok(config)
}

fn gateway_kind_setting(settings: &Option<serde_yaml::Value>) -> Result<Option<String>, ApiError> {
    if settings
        .as_ref()
        .and_then(|settings| settings.get("governance"))
        .and_then(|governance| governance.get("gateway"))
        .is_some()
    {
        return Err(ApiError::internal(
            "`governance.gateway` is no longer supported; configure gateway once in the top-level `gateway` section",
        ));
    }
    let kind = env_or_setting("HARNESS_GATEWAY_TYPE", settings, &["gateway", "type"])?
        .filter(|value| !value.trim().is_empty());
    if kind.as_deref().is_some_and(|value| {
        !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    }) {
        return Err(ApiError::internal(
            "gateway.type may contain only letters, numbers, dashes, and underscores",
        ));
    }
    if let Some(value) = kind.as_deref() {
        if gh_gateway::gateway_adapter(value).is_none() {
            return Err(ApiError::internal(format!(
                "unsupported gateway.type `{value}` (compiled: {})",
                gh_gateway::supported_gateway_types().join(", ")
            )));
        }
    }
    Ok(kind)
}

fn configured_gateway_policy(kind: Option<&str>) -> Option<gh_service::GatewayConfig> {
    kind.map(|kind| gh_service::GatewayConfig {
        kind: kind.to_owned(),
        proxy_url: None,
        token: None,
        auth_style: "bearer".into(),
    })
}

/// Force the gateway block to whatever this deployment is configured for, then
/// re-stamp the capability floor so the document still declares the
/// capabilities the gateway implies. The two always travel together: the server
/// decides gateway mode on its own authority, so it also owns the
/// `gateway_inference_jwt` capability that `validate_complete_governance` then
/// requires.
fn apply_deployment_gateway_policy(config: &mut gh_service::GovernanceConfig, kind: Option<&str>) {
    config.gateway = configured_gateway_policy(kind);
    stamp_version_aware_client_floor(config);
}

fn package_source_connections(
    settings: &Option<serde_yaml::Value>,
) -> Result<Vec<PackageSourceConnection>, ApiError> {
    let Some(value) = settings
        .as_ref()
        .and_then(|value| value.get("control_api"))
        .and_then(|value| value.get("package_sources"))
        .and_then(|value| value.get("connections"))
    else {
        return Ok(Vec::new());
    };
    let mut connections: Vec<PackageSourceConnection> = serde_yaml::from_value(value.clone())
        .map_err(|error| {
            ApiError::internal(format!("parsing package source connections: {error}"))
        })?;
    let mut ids = std::collections::BTreeSet::new();
    for connection in &mut connections {
        if connection.id.is_empty()
            || !connection
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !ids.insert(connection.id.clone())
        {
            return Err(ApiError::internal(format!(
                "package source connection id `{}` is invalid or duplicated",
                connection.id
            )));
        }
        connection.private_key = connection
            .private_key
            .take()
            .map(|value| resolve_secret_reference(&value))
            .transpose()?;
        connection.app_id = connection
            .app_id
            .take()
            .map(|value| resolve_secret_reference(&value))
            .transpose()?;
        connection.token = connection
            .token
            .take()
            .map(|value| resolve_secret_reference(&value))
            .transpose()?;
        connection.api_base_url = connection
            .api_base_url
            .take()
            .map(|value| resolve_secret_reference(&value))
            .transpose()?;
        connection.web_base_url = connection
            .web_base_url
            .take()
            .map(|value| resolve_secret_reference(&value))
            .transpose()?;
        connection.ca_bundle = connection
            .ca_bundle
            .take()
            .map(|value| resolve_secret_reference(&value))
            .transpose()?;
        for host in &mut connection.download_hosts {
            let normalized = host.to_ascii_lowercase();
            if normalized.is_empty()
                || normalized.contains(['/', ':', '@'])
                || reqwest::Url::parse(&format!("https://{normalized}"))
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned))
                    .as_deref()
                    != Some(normalized.as_str())
            {
                return Err(ApiError::internal(format!(
                    "package source connection `{}` has invalid download host `{host}`",
                    connection.id
                )));
            }
            *host = normalized;
        }
        match connection.provider.as_str() {
            "github" if connection.app_id.is_some() && connection.private_key.is_some() => {}
            "bitbucket_cloud" | "bitbucket_data_center" if connection.token.is_some() => {}
            "github" => {
                return Err(ApiError::internal(format!(
                    "GitHub connection `{}` requires app_id and private_key",
                    connection.id
                )))
            }
            "bitbucket_cloud" | "bitbucket_data_center" => {
                return Err(ApiError::internal(format!(
                    "Bitbucket connection `{}` requires token",
                    connection.id
                )))
            }
            provider => {
                return Err(ApiError::internal(format!(
                    "package source connection `{}` has unsupported provider `{provider}`",
                    connection.id
                )))
            }
        }
    }
    Ok(connections)
}

fn resolve_secret_reference(value: &str) -> Result<String, ApiError> {
    if let Some(name) = value
        .strip_prefix("os.environ/")
        .or_else(|| value.strip_prefix("env://"))
    {
        return std::env::var(name)
            .map_err(|_| ApiError::internal(format!("missing package source secret {name}")));
    }
    if let Some(path) = value.strip_prefix("file://") {
        return std::fs::read_to_string(path).map_err(|error| {
            ApiError::internal(format!("reading package source secret: {error}"))
        });
    }
    Ok(value.to_owned())
}

fn identity_mode(value: &str) -> Result<String, ApiError> {
    match value.trim().to_lowercase().as_str() {
        "password" => Ok("password".into()),
        "oidc" => Ok("oidc".into()),
        _ => Err(ApiError::internal(
            "HARNESS_AUTH_MODE must be password or oidc",
        )),
    }
}

fn role_mappings(value: Option<&str>) -> Result<BTreeMap<String, String>, ApiError> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(BTreeMap::new());
    };
    let mappings: BTreeMap<String, String> = serde_json::from_str(value).map_err(|error| {
        ApiError::internal(format!("parsing SCIM group role mappings: {error}"))
    })?;
    if mappings
        .values()
        .any(|role| role != "admin" && role != "member")
    {
        return Err(ApiError::internal(
            "SCIM group role mappings may contain only admin or member roles",
        ));
    }
    Ok(mappings)
}

fn load_blue_settings() -> Result<(PathBuf, Vec<PathBuf>, serde_yaml::Value), ApiError> {
    let path = std::env::var("BLUE_CONFIG_FILE")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| "blue.yaml".into());
    let overlays = std::env::var("BLUE_CONFIG_OVERLAY_FILES")
        .ok()
        .into_iter()
        .flat_map(|overlays| {
            overlays
                .split(',')
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let path = PathBuf::from(path);
    let settings = read_blue_config_with_overlays(&path, &overlays)?;
    Ok((path, overlays, settings))
}

/// Recursively merge a deployment overlay. Mappings merge by key; sequences
/// and scalar values replace the base value. Both roots must remain mappings.
fn merge_blue_config(
    base: &mut serde_yaml::Value,
    overlay: serde_yaml::Value,
) -> Result<(), ApiError> {
    if !base.is_mapping() || !overlay.is_mapping() {
        return Err(ApiError::internal(
            "Blue config and overlays must be YAML mappings",
        ));
    }

    fn merge(base: &mut serde_yaml::Value, overlay: serde_yaml::Value) {
        match (base, overlay) {
            (serde_yaml::Value::Mapping(base), serde_yaml::Value::Mapping(overlay)) => {
                for (key, value) in overlay {
                    match base.get_mut(&key) {
                        Some(existing) => merge(existing, value),
                        None => {
                            base.insert(key, value);
                        }
                    }
                }
            }
            (base, overlay) => *base = overlay,
        }
    }

    merge(base, overlay);
    Ok(())
}

fn env_or_setting(
    env_name: &str,
    settings: &Option<serde_yaml::Value>,
    path: &[&str],
) -> Result<Option<String>, ApiError> {
    if let Some(value) = std::env::var(env_name)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Some(value));
    }
    let Some(mut value) = settings.as_ref() else {
        return Ok(None);
    };
    for part in path {
        value = match value.get(*part) {
            Some(value) => value,
            None => return Ok(None),
        };
    }
    let raw = match value {
        serde_yaml::Value::Null => return Ok(None),
        serde_yaml::Value::String(value) => value.clone(),
        serde_yaml::Value::Bool(value) => value.to_string(),
        serde_yaml::Value::Number(value) => value.to_string(),
        _ => {
            return Err(ApiError::internal(format!(
                "server setting {} must be a scalar",
                path.join(".")
            )))
        }
    };
    if let Some(name) = raw
        .strip_prefix("os.environ/")
        .or_else(|| raw.strip_prefix("env://"))
    {
        return std::env::var(name).map(Some).map_err(|_| {
            ApiError::internal(format!(
                "server setting {} references missing environment variable {name}",
                path.join(".")
            ))
        });
    }
    Ok(Some(raw))
}

/// Gateway startup validates its complete required envelope in one pass. Treat
/// an absent environment reference as a missing value here so that validator
/// can report every missing setting together; other malformed values still
/// fail immediately.
fn optional_env_reference_setting(
    env_name: &str,
    settings: &Option<serde_yaml::Value>,
    path: &[&str],
) -> Result<Option<String>, ApiError> {
    if let Some(value) = std::env::var(env_name)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Some(value));
    }
    let Some(mut value) = settings.as_ref() else {
        return Ok(None);
    };
    for part in path {
        value = match value.get(*part) {
            Some(value) => value,
            None => return Ok(None),
        };
    }
    let raw = match value {
        serde_yaml::Value::Null => return Ok(None),
        serde_yaml::Value::String(value) => value.clone(),
        serde_yaml::Value::Bool(value) => value.to_string(),
        serde_yaml::Value::Number(value) => value.to_string(),
        _ => {
            return Err(ApiError::internal(format!(
                "server setting {} must be a scalar",
                path.join(".")
            )))
        }
    };
    if let Some(name) = raw
        .strip_prefix("os.environ/")
        .or_else(|| raw.strip_prefix("env://"))
    {
        return Ok(std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty()));
    }
    Ok(Some(raw))
}

fn required_setting(
    env_name: &str,
    settings: &Option<serde_yaml::Value>,
    path: &[&str],
) -> Result<String, ApiError> {
    env_or_setting(env_name, settings, path)?.ok_or_else(|| {
        ApiError::internal(format!(
            "required setting {} (or {env_name}) is missing",
            path.join(".")
        ))
    })
}

fn setting_or(
    env_name: &str,
    settings: &Option<serde_yaml::Value>,
    path: &[&str],
    default: &str,
) -> Result<String, ApiError> {
    Ok(env_or_setting(env_name, settings, path)?.unwrap_or_else(|| default.to_owned()))
}

fn positive_setting(
    env_name: &str,
    settings: &Option<serde_yaml::Value>,
    path: &[&str],
    default: i64,
) -> Result<i64, ApiError> {
    let value = env_or_setting(env_name, settings, path)?
        .map(|value| {
            value.parse::<i64>().map_err(|error| {
                ApiError::internal(format!("invalid setting {}: {error}", path.join(".")))
            })
        })
        .unwrap_or(Ok(default))?;
    if value <= 0 {
        Err(ApiError::internal(format!(
            "setting {} must be positive",
            path.join(".")
        )))
    } else {
        Ok(value)
    }
}

fn parse_bool_setting(value: &str) -> Result<bool, ApiError> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ApiError::internal(format!(
            "invalid boolean server setting {value}"
        ))),
    }
}

pub struct AppState {
    pool: PgPool,
    gateway_log_pool: PgPool,
    blob: BlobStore,
    package_blob: BlobStore,
    config: AppConfig,
    http: reqwest::Client,
    secret_protector: Option<SecretProtector>,
    gateway_provisioner: Option<Arc<dyn GatewayProvisioner>>,
    jwks: tokio::sync::RwLock<Option<CachedJwks>>,
    gateway_jwt: Option<GatewayJwtKeyRing>,
    revision_events: tokio::sync::broadcast::Sender<RevisionSignal>,
    gateway_kms_permits: Arc<tokio::sync::Semaphore>,
    gateway_provisioning_permits: Arc<tokio::sync::Semaphore>,
}

struct GatewayJwtKeyRing {
    issuer: String,
    audience: String,
    token_ttl: Duration,
    active_kid: String,
    signing_key: EncodingKey,
    public_jwks: JwkSet,
}

fn load_gateway_jwt_key_ring(config: &AppConfig) -> Result<Option<GatewayJwtKeyRing>, ApiError> {
    if config.gateway_kind.is_none() {
        return Ok(None);
    }
    let active_kid = config
        .gateway_jwt_active_kid
        .clone()
        .ok_or_else(|| ApiError::internal("HARNESS_GATEWAY_JWT_ACTIVE_KID is required"))?;
    let private_pem =
        match config.gateway_jwt_private_key_pem.as_ref() {
            Some(value) => value.as_bytes().to_vec(),
            None => std::fs::read(config.gateway_jwt_private_key_file.as_ref().ok_or_else(
                || ApiError::internal("gateway JWT private key material is required"),
            )?)
            .map_err(|error| {
                ApiError::internal(format!("reading gateway JWT private key: {error}"))
            })?,
        };
    let signing_key = EncodingKey::from_rsa_pem(&private_pem).map_err(|error| {
        ApiError::internal(format!("decoding gateway JWT private key: {error}"))
    })?;
    let jwks_json = match config.gateway_jwt_jwks_json.as_ref() {
        Some(value) => value.as_bytes().to_vec(),
        None => std::fs::read(
            config
                .gateway_jwt_jwks_file
                .as_ref()
                .ok_or_else(|| ApiError::internal("gateway JWT JWKS material is required"))?,
        )
        .map_err(|error| ApiError::internal(format!("reading gateway JWT JWKS: {error}")))?,
    };
    let public_jwks: JwkSet = serde_json::from_slice(&jwks_json)
        .map_err(|error| ApiError::internal(format!("decoding gateway JWT JWKS: {error}")))?;
    let active_public_key = public_jwks
        .keys
        .iter()
        .find(|key| key.common.key_id.as_deref() == Some(active_kid.as_str()))
        .ok_or_else(|| ApiError::internal("gateway JWT JWKS does not contain active_kid"))?;
    let mut probe_header = Header::new(Algorithm::RS256);
    probe_header.kid = Some(active_kid.clone());
    let probe = encode(
        &probe_header,
        &json!({"sub":"gateway-key-probe"}),
        &signing_key,
    )
    .map_err(|error| ApiError::internal(format!("testing gateway JWT signing key: {error}")))?;
    let verification_key = DecodingKey::from_jwk(active_public_key).map_err(|error| {
        ApiError::internal(format!(
            "decoding active gateway JWT verification key: {error}"
        ))
    })?;
    let mut probe_validation = Validation::new(Algorithm::RS256);
    probe_validation.required_spec_claims.clear();
    probe_validation.validate_exp = false;
    decode::<serde_json::Value>(&probe, &verification_key, &probe_validation).map_err(|_| {
        ApiError::internal("gateway JWT private key does not match active JWKS key")
    })?;
    Ok(Some(GatewayJwtKeyRing {
        issuer: config.gateway_jwt_issuer.clone(),
        audience: config.gateway_jwt_audience.clone(),
        token_ttl: Duration::seconds(config.gateway_inference_token_ttl_seconds as i64),
        active_kid,
        signing_key,
        public_jwks,
    }))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RevisionEvent {
    organization_id: Uuid,
    revision: String,
}

#[derive(Clone, Debug)]
enum RevisionSignal {
    Revision(RevisionEvent),
    Resync,
}

struct CachedJwks {
    set: JwkSet,
    fetched_at: std::time::Instant,
}

fn credential_digest(credential: &str) -> String {
    hex::encode(Sha256::digest(credential.as_bytes()))
}

pub struct AppRouters {
    pub public: Router,
    pub internal: Router,
}

pub async fn build_apps(config: AppConfig) -> Result<AppRouters, ApiError> {
    if let Some(provisioner_config) = config.gateway_provisioner.as_ref().filter(|provisioner| {
        provisioner.executable_path.is_some() || provisioner.executable_sha256.is_some()
    }) {
        let provisioner =
            executable_provisioner::ExecutableGatewayProvisioner::load(provisioner_config)
                .await
                .map_err(ApiError::internal)?;
        return build_app_inner(config, Some(Arc::new(provisioner))).await;
    }
    let provisioner: Option<Arc<dyn GatewayProvisioner>> = match config
        .gateway_provisioner
        .as_ref()
        .map(|config| config.kind.as_str())
    {
        None => None,
        Some("builtin-litellm") => {
            #[cfg(not(feature = "builtin-litellm"))]
            return Err(ApiError::internal(
                "gateway provisioner `builtin-litellm` is not linked into this control-api binary",
            ));
            #[cfg(feature = "builtin-litellm")]
            {
                let base_url = config.gateway_upstream_url.clone().ok_or_else(|| {
                    ApiError::internal("gateway.url is required for builtin-litellm")
                })?;
                let admin_key = std::env::var("HARNESS_LITELLM_ADMIN_KEY").map_err(|_| {
                    ApiError::internal("HARNESS_LITELLM_ADMIN_KEY is required for builtin-litellm")
                })?;
                Some(Arc::new(
                    gh_gateway_provisioner::litellm::LiteLlmProvisioner::new(base_url, admin_key)
                        .map_err(|error| ApiError::internal(error.to_string()))?,
                ))
            }
        }
        Some(kind) => {
            return Err(ApiError::internal(format!(
                "gateway provisioner `{kind}` requires executable_path, executable_sha256, and policy_revision"
            )))
        }
    };
    build_app_inner(config, provisioner).await
}

/// Build the public and internal Control API routers with a deployer-owned
/// provisioner supplied directly by an embedding binary.
pub async fn build_apps_with_provisioner(
    config: AppConfig,
    provisioner: Arc<dyn GatewayProvisioner>,
) -> Result<AppRouters, ApiError> {
    build_app_inner(config, Some(provisioner)).await
}

/// Serve a deployer-owned Control API binary with the same public/internal
/// listener split and shutdown behavior as the bundled binary.
pub async fn serve_with_provisioner(
    config: AppConfig,
    provisioner: Arc<dyn GatewayProvisioner>,
) -> anyhow::Result<()> {
    let bind = config.listen.clone();
    let internal_bind = config.internal_listen.clone();
    let tls = internal_tls_config(&config)?;
    let apps = build_apps_with_provisioner(config, provisioner).await?;
    serve_apps(apps, bind, internal_bind, tls).await
}

/// Serve the bundled Control API implementation.
pub async fn serve(config: AppConfig) -> anyhow::Result<()> {
    let bind = config.listen.clone();
    let internal_bind = config.internal_listen.clone();
    let tls = internal_tls_config(&config)?;
    let apps = build_apps(config).await?;
    serve_apps(apps, bind, internal_bind, tls).await
}

async fn serve_apps(
    apps: AppRouters,
    bind: String,
    internal_bind: String,
    tls: Option<InternalTlsConfig>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!(
        "control API listening on {bind}; internal gateway API on {}://{internal_bind}",
        if tls.is_some() { "https" } else { "http" }
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    let mut public_shutdown = shutdown_rx.clone();
    let mut internal_shutdown = shutdown_rx.clone();
    let public = axum::serve(listener, apps.public).with_graceful_shutdown(async move {
        let _ = public_shutdown.changed().await;
    });
    if let Some(tls) = tls {
        let rustls = tls.rustls.clone();
        tokio::spawn(internal_tls_reload_worker(tls, shutdown_rx.clone()));
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            let _ = internal_shutdown.changed().await;
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
        });
        let internal = axum_server::bind_rustls(internal_bind.parse()?, rustls)
            .handle(handle)
            .serve(apps.internal.into_make_service());
        tokio::try_join!(public, internal)?;
    } else {
        let internal_listener = tokio::net::TcpListener::bind(&internal_bind).await?;
        let internal =
            axum::serve(internal_listener, apps.internal).with_graceful_shutdown(async move {
                let _ = internal_shutdown.changed().await;
            });
        tokio::try_join!(public, internal)?;
    }
    Ok(())
}

fn internal_tls_config(config: &AppConfig) -> anyhow::Result<Option<InternalTlsConfig>> {
    let configured = config.internal_tls_cert_file.is_some()
        || config.internal_tls_key_file.is_some()
        || config.internal_tls_client_ca_file.is_some();
    match config.internal_transport_mode.as_str() {
        "insecure-http" => {
            if configured {
                anyhow::bail!("internal TLS files must not be configured in insecure-http mode");
            }
            tracing::warn!("INSECURE INTERNAL TRANSPORT ENABLED: decrypted gateway credentials cross the network in plaintext; use only on a private, trusted network");
            return Ok(None);
        }
        "mtls" => {}
        value => anyhow::bail!(
            "HARNESS_INTERNAL_TRANSPORT_MODE must be `mtls` or `insecure-http`, got `{value}`"
        ),
    }
    if !configured {
        if config.gateway_inference_proxy_url.is_none() {
            return Ok(None);
        }
        anyhow::bail!("internal mTLS is required; configure HARNESS_INTERNAL_TLS_CERT_FILE, HARNESS_INTERNAL_TLS_KEY_FILE, and HARNESS_INTERNAL_TLS_CLIENT_CA_FILE");
    }
    let cert_path = config
        .internal_tls_cert_file
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("HARNESS_INTERNAL_TLS_CERT_FILE is required"))?;
    let key_path = config
        .internal_tls_key_file
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("HARNESS_INTERNAL_TLS_KEY_FILE is required"))?;
    let ca_path = config
        .internal_tls_client_ca_file
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("HARNESS_INTERNAL_TLS_CLIENT_CA_FILE is required"))?;
    let server = load_internal_server_config(cert_path, key_path, ca_path)?;
    Ok(Some(InternalTlsConfig {
        rustls: axum_server::tls_rustls::RustlsConfig::from_config(server),
        cert_path: cert_path.clone(),
        key_path: key_path.clone(),
        ca_path: ca_path.clone(),
    }))
}

#[derive(Clone)]
struct InternalTlsConfig {
    rustls: axum_server::tls_rustls::RustlsConfig,
    cert_path: PathBuf,
    key_path: PathBuf,
    ca_path: PathBuf,
}

fn load_internal_server_config(
    cert_path: &PathBuf,
    key_path: &PathBuf,
    ca_path: &PathBuf,
) -> anyhow::Result<std::sync::Arc<rustls::ServerConfig>> {
    use rustls::pki_types::PrivateKeyDer;
    use rustls::server::WebPkiClientVerifier;
    use std::io::BufReader;
    // The production dependency graph enables more than one rustls provider
    // (direct rustls plus transitive HTTP/AWS clients), so selecting one is
    // mandatory before ServerConfig construction. Re-installation is harmless:
    // install_default returns Err once the process-wide provider is set.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let certs = rustls_pemfile::certs(&mut BufReader::new(std::fs::File::open(cert_path)?))
        .collect::<Result<Vec<_>, _>>()?;
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(std::fs::File::open(key_path)?))?
            .ok_or_else(|| anyhow::anyhow!("internal TLS key file contains no private key"))?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(
        rustls_pemfile::certs(&mut BufReader::new(std::fs::File::open(ca_path)?))
            .filter_map(Result::ok),
    );
    if roots.is_empty() {
        anyhow::bail!("internal TLS client CA file contains no valid certificates");
    }
    let verifier = WebPkiClientVerifier::builder(roots.into()).build()?;
    let mut server = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)?;
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(std::sync::Arc::new(server))
}

async fn internal_tls_reload_worker(
    tls: InternalTlsConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut fingerprint = internal_tls_fingerprint(&tls);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    interval.tick().await;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = interval.tick() => {
                let next = internal_tls_fingerprint(&tls);
                if next.is_none() {
                    INTERNAL_TLS_RELOAD_ERRORS.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("unable to read internal mTLS certificate material; retaining last-known-good configuration");
                } else if next != fingerprint {
                    match load_internal_server_config(&tls.cert_path, &tls.key_path, &tls.ca_path) {
                        Ok(config) => {
                            tls.rustls.reload_from_config(config);
                            fingerprint = next;
                            INTERNAL_TLS_RELOAD_SUCCESSES.fetch_add(1, Ordering::Relaxed);
                            tracing::info!("reloaded internal mTLS certificate material");
                        }
                        Err(error) => {
                            INTERNAL_TLS_RELOAD_ERRORS.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(%error, "failed to reload internal mTLS certificate material; retaining last-known-good configuration");
                        }
                    }
                }
            }
        }
    }
}

fn internal_tls_fingerprint(tls: &InternalTlsConfig) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(std::fs::read(&tls.cert_path).ok()?);
    hash.update(std::fs::read(&tls.key_path).ok()?);
    hash.update(std::fs::read(&tls.ca_path).ok()?);
    Some(hash.finalize().into())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
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

fn validate_linked_provisioner(
    configured: Option<&GatewayProvisionerConfig>,
    linked: &Option<Arc<dyn GatewayProvisioner>>,
) -> Result<(), ApiError> {
    match (configured, linked) {
        (Some(config), Some(provisioner)) if config.kind == provisioner.kind() => Ok(()),
        (Some(config), Some(provisioner)) => Err(ApiError::internal(format!(
            "gateway.provisioner.type is `{}`, but the linked provisioner identifies as `{}`",
            config.kind,
            provisioner.kind()
        ))),
        (Some(_), None) => Err(ApiError::internal(
            "gateway provisioner implementation is unavailable",
        )),
        // A deployment may mount an organization provisioner while running in
        // governance-only mode. It remains inactive until selected.
        (None, Some(_)) | (None, None) => Ok(()),
    }
}

async fn build_app_inner(
    config: AppConfig,
    gateway_provisioner: Option<Arc<dyn GatewayProvisioner>>,
) -> Result<AppRouters, ApiError> {
    config.validate_gateway_runtime()?;
    let gateway_jwt = load_gateway_jwt_key_ring(&config)?;
    if config.auth_mode == "oidc" && config.scim_bearer_token.is_none() {
        return Err(ApiError::internal(
            "HARNESS_SCIM_BEARER_TOKEN is required when HARNESS_AUTH_MODE=oidc",
        ));
    }
    validate_linked_provisioner(config.gateway_provisioner.as_ref(), &gateway_provisioner)?;
    let gateway_provisioner = config.gateway_provisioner.as_ref().and(gateway_provisioner);
    if gateway_provisioner.is_some() && config.database_max_connections < 2 {
        return Err(ApiError::internal(
            "gateway provisioning requires at least two control database connections",
        ));
    }
    let pools = db::connect(&config).await?;
    let pool = pools.control;
    let gateway_log_pool = pools.gateway_logs;
    let secret_protector = match (&config.gateway_kind, &config.gateway_encryption) {
        (None, None) => None,
        (Some(_), Some(encryption)) if encryption.provider == "environment" => Some(
            SecretProtector::environment(encryption.key.as_deref().ok_or_else(|| {
                ApiError::internal(
                    "gateway.secret_encryption.key is required for environment provider",
                )
            })?)
            .await?,
        ),
        (Some(_), Some(encryption)) if encryption.provider == "aws-kms" => Some(
            SecretProtector::aws_kms(encryption.key_id.clone().ok_or_else(|| {
                ApiError::internal(
                    "gateway.secret_encryption.key_id is required for aws-kms provider",
                )
            })?)
            .await?,
        ),
        (Some(_), Some(encryption)) => {
            return Err(ApiError::internal(format!(
                "unsupported gateway secret encryption provider {}",
                encryption.provider
            )))
        }
        (Some(_), None) => {
            return Err(ApiError::internal(
                "gateway.secret_encryption must be configured when gateway mode is enabled",
            ))
        }
        (None, Some(_)) => {
            return Err(ApiError::internal(
                "gateway.secret_encryption requires gateway mode",
            ))
        }
    };
    if let Some(protector) = &secret_protector {
        migrate_legacy_gateway_credentials(&pool, protector).await?;
    }
    let blob = BlobStore::from_config(&config).await?;
    let package_blob =
        BlobStore::from_config_with_bucket(&config, config.s3_package_bucket.clone()).await?;
    bootstrap_deployment(&pool, &config, &package_blob).await?;
    let (revision_events, _) = tokio::sync::broadcast::channel(256);
    let gateway_kms_max_concurrency = config.gateway_kms_max_concurrency;
    let gateway_provisioning_max_concurrency = config
        .gateway_provisioner
        .as_ref()
        .map(|value| value.max_concurrency)
        .unwrap_or_else(default_provisioner_max_concurrency)
        .min((config.database_max_connections as usize / 2).max(1));
    let state = Arc::new(AppState {
        pool,
        gateway_log_pool,
        blob,
        package_blob,
        config,
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|error| ApiError::internal(format!("building auth client: {error}")))?,
        secret_protector,
        gateway_provisioner,
        jwks: tokio::sync::RwLock::new(None),
        gateway_jwt,
        revision_events: revision_events.clone(),
        gateway_kms_permits: Arc::new(tokio::sync::Semaphore::new(gateway_kms_max_concurrency)),
        gateway_provisioning_permits: Arc::new(tokio::sync::Semaphore::new(
            gateway_provisioning_max_concurrency,
        )),
    });
    tokio::spawn(listen_for_revision_events(
        state.pool.clone(),
        revision_events,
    ));
    if state.config.run_background_jobs {
        tokio::spawn(cleanup_gateway_request_logs(state.gateway_log_pool.clone()));
        tokio::spawn(process_gateway_revocations(state.clone()));
    }

    let public_routes = Router::new()
        .route("/health", get(health))
        .route("/health/schema", get(schema_health))
        .route("/health/worker", get(worker_health))
        .route("/health/object-storage", get(object_storage_health))
        .route(
            "/health/credential-resolver",
            get(credential_resolver_health),
        )
        .route("/ready", get(readiness))
        .route("/metrics", get(control_metrics))
        .route("/.well-known/metaharness", get(metaharness_discovery))
        .route("/gateway/jwks", get(gateway_auth::gateway_jwks))
        .route("/branding", get(branding));
    let authenticated_routes = Router::new()
        .route("/auth/me", get(current_identity))
        .route(
            "/package-artifacts/:id/download",
            post(download_package_artifact),
        )
        .route("/session-uploads", get(list_captured_sessions))
        .route("/session-uploads/members", get(session_share_members))
        .route("/session-uploads/facets", get(captured_session_facets))
        .route("/session-uploads/:id", get(captured_session_detail))
        .route("/session-uploads/:id/download", post(download_session))
        .route(
            "/session-uploads/:id/sharing",
            get(session_sharing).put(update_session_sharing),
        )
        .route_layer(middleware::from_fn_with_state(
            UserGuardState::new(state.clone(), UserPolicy::Authenticated),
            user_auth_guard,
        ));
    let governance_routes = Router::new()
        .route("/health/dependencies", get(dependency_health))
        .route("/gateway/status", get(gateway_status))
        .route("/gateway/key", get(gateway_key))
        .route("/gateway/key/ensure", post(ensure_gateway_key))
        .route("/gateway/key/validate", post(validate_gateway_key))
        .route(
            "/gateway/session/revoke",
            post(revoke_current_gateway_session),
        )
        .route("/governance-config", get(governance_config))
        .route("/harness-metadata", get(harness_metadata))
        .route("/governance-config/events", get(governance_config_events))
        .route_layer(middleware::from_fn_with_state(
            UserGuardState::new(state.clone(), UserPolicy::Scope("governance:read")),
            user_auth_guard,
        ));
    let client_status_routes = Router::new()
        .route("/client-status", post(report_client_status))
        .route_layer(middleware::from_fn_with_state(
            UserGuardState::new(state.clone(), UserPolicy::Scope("client-status:write")),
            user_auth_guard,
        ));
    let session_write_routes = Router::new()
        .route("/session-uploads/presign", post(presign_upload))
        .route("/session-uploads/:id/complete", post(complete_upload))
        .route_layer(middleware::from_fn_with_state(
            UserGuardState::new(state.clone(), UserPolicy::Scope("session:write")),
            user_auth_guard,
        ));
    let admin_routes = Router::new()
        .route("/gateway/proxy/health", post(gateway_proxy_health))
        .route("/gateway/request-logs", get(gateway_request_logs))
        .route(
            "/gateway/request-logs/facets",
            get(gateway_request_log_facets),
        )
        .route("/admin/identity/status", get(identity_status))
        .route(
            "/admin/governance-config",
            get(admin_governance_config).put(update_governance_config),
        )
        .route(
            "/admin/governance-config/revisions",
            get(governance_revisions),
        )
        .route("/admin/blue-config/export", get(export_blue_config))
        .route("/admin/branding", axum::routing::put(update_branding))
        .route("/admin/package-catalog", get(package_catalog))
        .route(
            "/admin/package-source/connections",
            get(package_source_connections_api),
        )
        .route(
            "/admin/package-source/inspect",
            post(inspect_package_source),
        )
        .route(
            "/admin/governance-extensions",
            axum::routing::put(update_governance_extensions),
        )
        .route(
            "/admin/harnesses/:harness/managed-config",
            get(get_harness_managed_config).put(update_harness_managed_config),
        )
        .route(
            "/admin/harnesses/managed-configs",
            get(list_harness_managed_configs),
        )
        .route("/admin/users", get(list_users))
        .route(
            "/admin/users/options",
            get(list_user_options).post(search_user_options),
        )
        .route(
            "/admin/users/:id",
            get(get_user).patch(update_user).delete(delete_user),
        )
        .route(
            "/admin/users/:id/sessions/revoke",
            post(revoke_user_sessions),
        )
        .route(
            "/admin/invitations",
            get(list_invitations).post(create_invitation),
        )
        .route(
            "/admin/invitations/:id",
            get(get_invitation).delete(cancel_invitation),
        )
        .route("/admin/invitations/:id/resend", post(resend_invitation))
        .route("/admin/client-status", get(list_client_status))
        .route("/admin/client-status/facets", get(client_status_facets))
        .route(
            "/admin/client-status/:id",
            axum::routing::delete(delete_client_status),
        )
        .route_layer(middleware::from_fn_with_state(
            UserGuardState::new(state.clone(), UserPolicy::Admin),
            user_auth_guard,
        ));
    let public = public_routes
        .merge(authenticated_routes)
        .merge(governance_routes)
        .merge(client_status_routes)
        .merge(session_write_routes)
        .merge(admin_routes)
        .nest("/scim/v2", scim::router(state.clone()))
        .with_state(state.clone());
    let internal = Router::new()
        .route("/internal/gateway/jwks", get(gateway_auth::gateway_jwks))
        .route("/internal/gateway/resolve", post(resolve_gateway_key))
        .route("/internal/gateway/events", get(gateway_cache_events))
        .route(
            "/internal/gateway/credential-invalid",
            post(report_invalid_gateway_credential),
        )
        .route(
            "/internal/gateway/request-logs",
            post(ingest_gateway_request_log),
        )
        .route(
            "/internal/gateway/request-logs/batch",
            post(ingest_gateway_request_log_batch),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            internal_auth_guard,
        ))
        .with_state(state);
    Ok(AppRouters { public, internal })
}

async fn health() -> &'static str {
    "ok"
}

async fn schema_health(State(state): State<Arc<AppState>>) -> Response {
    match db::verify_schema(&state.pool).await {
        Ok(()) => (StatusCode::OK, "compatible").into_response(),
        Err(error) => {
            tracing::warn!(%error, "control API schema compatibility check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "schema incompatible").into_response()
        }
    }
}

async fn worker_health(State(state): State<Arc<AppState>>) -> Response {
    if !state.config.run_background_jobs {
        return (StatusCode::OK, "disabled").into_response();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let age = now.saturating_sub(WORKER_LAST_SUCCESS_UNIX.load(Ordering::Relaxed));
    if age <= 120 {
        (StatusCode::OK, "fresh").into_response()
    } else {
        tracing::warn!(age_seconds = age, "background worker is stale");
        (StatusCode::SERVICE_UNAVAILABLE, "stale").into_response()
    }
}

async fn object_storage_health(State(state): State<Arc<AppState>>) -> Response {
    let blobs = state.blob.health().await;
    let packages = state.package_blob.health().await;
    if blobs.is_ok() && packages.is_ok() {
        (StatusCode::OK, "healthy").into_response()
    } else {
        tracing::warn!(
            blob_storage = blobs.is_ok(),
            package_storage = packages.is_ok(),
            "object storage health check failed"
        );
        (StatusCode::SERVICE_UNAVAILABLE, "unhealthy").into_response()
    }
}

async fn credential_resolver_health(State(state): State<Arc<AppState>>) -> Response {
    if state.config.gateway_kind.is_none() {
        return (StatusCode::OK, "disabled").into_response();
    }
    match sqlx::query_scalar!("SELECT 1 AS \"one!\" FROM public.gateway_key_selections LIMIT 1")
        .fetch_optional(&state.pool)
        .await
    {
        Ok(_) => (StatusCode::OK, "healthy").into_response(),
        Err(error) => {
            tracing::warn!(%error, "gateway credential resolver health check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "unhealthy").into_response()
        }
    }
}

async fn readiness(State(state): State<Arc<AppState>>) -> Response {
    match db::verify_schema(&state.pool).await {
        Ok(()) => (StatusCode::OK, "ready").into_response(),
        Err(error) => {
            tracing::warn!(%error, "control API readiness schema check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "schema unavailable").into_response()
        }
    }
}

async fn control_metrics(State(state): State<Arc<AppState>>) -> String {
    format!(
        concat!(
            "gateway_control_db_pool_size {}\n",
            "gateway_control_db_pool_idle {}\n",
            "gateway_control_log_pool_size {}\n",
            "gateway_control_log_pool_idle {}\n",
            "gateway_control_kms_permits_available {}\n",
            "gateway_control_tls_reload_successes_total {}\n",
            "gateway_control_tls_reload_errors_total {}\n"
        ),
        state.pool.size(),
        state.pool.num_idle(),
        state.gateway_log_pool.size(),
        state.gateway_log_pool.num_idle(),
        state.gateway_kms_permits.available_permits(),
        INTERNAL_TLS_RELOAD_SUCCESSES.load(Ordering::Relaxed),
        INTERNAL_TLS_RELOAD_ERRORS.load(Ordering::Relaxed),
    )
}

#[derive(Serialize)]
struct DependencyHealthCheck {
    name: &'static str,
    status: &'static str,
    latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct DependencyHealthResponse {
    status: &'static str,
    checked_at: String,
    checks: Vec<DependencyHealthCheck>,
}

fn dependency_check(
    name: &'static str,
    started: Instant,
    result: Result<(), impl std::fmt::Display>,
) -> DependencyHealthCheck {
    DependencyHealthCheck {
        name,
        status: if result.is_ok() {
            "healthy"
        } else {
            "unhealthy"
        },
        latency_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        error: result.err().map(|error| error.to_string()),
    }
}

async fn dependency_health(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DependencyHealthResponse>, ApiError> {
    let database_started = Instant::now();
    let database = dependency_check(
        "database",
        database_started,
        sqlx::query_scalar!("select 1")
            .fetch_one(&state.pool)
            .await
            .map(|_| ())
            .map_err(|_| "database query failed"),
    );

    let blob_started = Instant::now();
    let blob_storage = dependency_check(
        "blob_storage",
        blob_started,
        state
            .blob
            .health()
            .await
            .map_err(|_| "blob storage check failed"),
    );

    let packages_started = Instant::now();
    let package_storage = dependency_check(
        "package_storage",
        packages_started,
        state
            .package_blob
            .health()
            .await
            .map_err(|_| "package storage check failed"),
    );

    let mut checks = vec![database, blob_storage, package_storage];
    if let Some(configured) = state
        .config
        .gateway_inference_proxy_health_url
        .as_ref()
        .or(state.config.gateway_inference_proxy_url.as_ref())
    {
        let proxy_started = Instant::now();
        let proxy_result = match reqwest::Url::parse(configured).and_then(|url| url.join("/health"))
        {
            Ok(url) => state
                .http
                .get(url)
                .timeout(std::time::Duration::from_secs(3))
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map(|_| ())
                .map_err(|_| "inference proxy check failed"),
            Err(_) => Err("inference proxy URL is invalid"),
        };
        checks.push(dependency_check(
            "inference_proxy",
            proxy_started,
            proxy_result,
        ));
    }

    let status = if checks.iter().all(|check| check.status == "healthy") {
        "healthy"
    } else {
        "degraded"
    };
    let checked_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| ApiError::internal(format!("formatting health timestamp: {error}")))?;
    Ok(Json(DependencyHealthResponse {
        status,
        checked_at,
        checks,
    }))
}

#[derive(Debug, Serialize)]
struct MetaharnessDiscovery {
    version: u32,
    control_api_url: String,
    oauth: MetaharnessOAuth,
}

#[derive(Debug, Serialize)]
struct MetaharnessOAuth {
    issuer: String,
    client_id: String,
    scopes: Vec<&'static str>,
}

/// Public bootstrap metadata. It deliberately contains no credentials or
/// organization policy and therefore sits outside the authentication layer.
async fn metaharness_discovery(State(state): State<Arc<AppState>>) -> Json<MetaharnessDiscovery> {
    Json(MetaharnessDiscovery {
        version: 1,
        control_api_url: state.config.auth_audience.trim_end_matches('/').to_owned(),
        oauth: MetaharnessOAuth {
            issuer: state.config.auth_issuer.trim_end_matches('/').to_owned(),
            client_id: state.config.auth_client_id.clone(),
            scopes: vec![
                "openid",
                "profile",
                "email",
                "offline_access",
                "governance:read",
                "session:write",
                "client-status:write",
            ],
        },
    })
}

#[cfg(test)]
mod metaharness_discovery_tests {
    use super::*;

    #[test]
    fn discovery_is_versioned_and_contains_only_public_bootstrap_fields() {
        let value = serde_json::to_value(MetaharnessDiscovery {
            version: 1,
            control_api_url: "https://control.example.com".into(),
            oauth: MetaharnessOAuth {
                issuer: "https://auth.example.com".into(),
                client_id: "blue-cli".into(),
                scopes: vec!["openid", "governance:read"],
            },
        })
        .unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["control_api_url"], "https://control.example.com");
        assert!(value.get("oauth").is_some());
        let root = value.as_object().unwrap();
        assert_eq!(
            root.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "control_api_url".to_owned(),
                "oauth".to_owned(),
                "version".to_owned()
            ])
        );
        let oauth = value["oauth"].as_object().unwrap();
        assert_eq!(
            oauth.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "client_id".to_owned(),
                "issuer".to_owned(),
                "scopes".to_owned()
            ])
        );
    }
}

#[derive(FromRow, Serialize)]
struct BrandingResponse {
    logo_url: Option<String>,
    favicon_url: Option<String>,
}

async fn branding(State(state): State<Arc<AppState>>) -> Result<Json<BrandingResponse>, ApiError> {
    let value = sqlx::query_as!(
        BrandingResponse,
        "SELECT logo_url, favicon_url FROM deployment_branding WHERE singleton=true"
    )
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct UpdateBrandingRequest {
    logo_url: Option<String>,
    favicon_url: Option<String>,
}

fn normalize_branding_url(value: Option<String>, field: &str) -> Result<Option<String>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().count() > 2048 {
        return Err(ApiError::bad_request(format!(
            "{field} must be 2048 characters or fewer"
        )));
    }
    let parsed = reqwest::Url::parse(value).map_err(|_| {
        ApiError::bad_request(format!("{field} must be an absolute HTTP or HTTPS URL"))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(ApiError::bad_request(format!(
            "{field} must be an absolute HTTP or HTTPS URL"
        )));
    }
    Ok(Some(value.to_owned()))
}

async fn update_branding(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Json(input): Json<UpdateBrandingRequest>,
) -> Result<Json<BrandingResponse>, ApiError> {
    let logo_url = normalize_branding_url(input.logo_url, "logo_url")?;
    let favicon_url = normalize_branding_url(input.favicon_url, "favicon_url")?;
    let value = sqlx::query_as!(
        BrandingResponse,
        "UPDATE deployment_branding SET logo_url=$1, favicon_url=$2, updated_by=$3, \
         updated_at=now() WHERE singleton=true RETURNING logo_url, favicon_url",
        logo_url,
        favicon_url,
        who.user_id
    )
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(value))
}

async fn harness_metadata() -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(json!({
        "contract_version": gh_service::GovernanceConfig::CONTRACT_VERSION,
        "harnesses": gh_config::implementations::registry_metadata(),
    })))
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "invalid or expired session")
    }
    fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "administrator access required")
    }
    fn forbidden_message(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
    /// A 401 whose cause the client cannot infer from the status alone. The
    /// generic `unauthorized()` says "invalid or expired session", which is
    /// true of a malformed token and of a dead browser binding alike — and
    /// only one of those tells the user what to do.
    fn unauthorized_message(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref()
            == Some("23505")
        {
            return ApiError::conflict("record already exists");
        }
        tracing::error!(%error, "database error");
        ApiError::internal("database operation failed")
    }
}

fn now_text(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

fn read_blue_config(path: &std::path::Path) -> Result<serde_yaml::Value, ApiError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        ApiError::internal(format!("reading Blue config {}: {error}", path.display()))
    })?;
    serde_yaml::from_str(&text).map_err(|error| {
        ApiError::internal(format!("parsing Blue config {}: {error}", path.display()))
    })
}

fn read_blue_config_with_overlays(
    path: &std::path::Path,
    overlays: &[PathBuf],
) -> Result<serde_yaml::Value, ApiError> {
    let mut document = read_blue_config(path)?;
    for overlay in overlays {
        merge_blue_config(&mut document, read_blue_config(overlay)?)?;
    }
    Ok(document)
}

fn governance_seed_yaml(
    document: &serde_yaml::Value,
    gateway_kind: Option<&str>,
) -> Result<String, ApiError> {
    let mut governance = document.get("governance").cloned().ok_or_else(|| {
        ApiError::internal("Blue config must contain a top-level `governance` section")
    })?;
    let governance = governance
        .as_mapping_mut()
        .ok_or_else(|| ApiError::internal("Blue config `governance` must be a mapping"))?;
    governance.remove(yaml_key("gateway"));
    if let Some(policy) = configured_gateway_policy(gateway_kind) {
        governance.insert(
            yaml_key("gateway"),
            serde_yaml::to_value(policy).map_err(|error| {
                ApiError::internal(format!("serializing gateway policy: {error}"))
            })?,
        );
    }
    serde_yaml::to_string(&governance)
        .map_err(|error| ApiError::internal(format!("serializing governance seed: {error}")))
}

async fn bootstrap_identity(pool: &PgPool, config: &AppConfig) -> Result<(Uuid, Uuid), ApiError> {
    let org_id: Uuid = sqlx::query_scalar!(
        "INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $3) \
         ON CONFLICT (slug) DO UPDATE SET name = EXCLUDED.name RETURNING id",
        Uuid::new_v4(),
        &config.bootstrap_org,
        &config.bootstrap_org_name
    )
    .fetch_one(pool)
    .await?;
    let user_id: Uuid = sqlx::query_scalar!("INSERT INTO users (id, organization_id, subject, email, role, status, active, protected) \
         VALUES ($1, $2, $3, $4, 'admin', 'active', true, true) \
         ON CONFLICT (organization_id, email) DO UPDATE SET \
         role = 'admin', status = 'active', active = true, protected = true, updated_at = now() RETURNING id",
        Uuid::new_v4(),
        org_id,
        &config.bootstrap_admin_sub,
        &config.bootstrap_admin_email)
    .fetch_one(pool)
    .await?;
    Ok((org_id, user_id))
}

/// Seed the deployment's organization, admin and governance revision, holding a
/// database-wide lock for the whole sequence.
///
/// The chart runs this binary as two Deployments — `control-api` and `worker` —
/// and on a cold start both reach this path against an empty database. Left to
/// race, they write the same bootstrap rows from two sessions: one wins and the
/// other takes a unique violation, which `From<sqlx::Error>` turns into
/// `record already exists` and kills the process on its first start. The lock
/// makes the loser wait and then read what the winner wrote.
///
/// The lock transaction carries no writes of its own, so every early return
/// drops it and releases the lock.
async fn bootstrap_deployment(
    pool: &PgPool,
    config: &AppConfig,
    package_blob: &BlobStore,
) -> Result<(), ApiError> {
    let bootstrap_lock = acquire_bootstrap_lock(pool, &config.bootstrap_org).await?;
    let (org_id, user_id) = bootstrap_identity(pool, config).await?;
    reconcile_deployment_governance(pool, config, package_blob, org_id, user_id).await?;
    bootstrap_lock.rollback().await?;
    Ok(())
}

/// The lock is keyed by the bootstrap organization so unrelated deployments
/// sharing a database never wait on each other. It reuses the salt of the
/// gateway lifecycle lock; the key spaces stay disjoint because that one is
/// keyed by user id.
async fn acquire_bootstrap_lock(
    pool: &PgPool,
    bootstrap_org: &str,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, ApiError> {
    let mut bootstrap_lock = pool.begin().await?;
    sqlx::query!(
        "select pg_advisory_xact_lock(hashtextextended($1, 928451))",
        format!("bootstrap-identity:{bootstrap_org}")
    )
    .execute(&mut *bootstrap_lock)
    .await?;
    Ok(bootstrap_lock)
}

fn normalized_governance_value(
    config: &gh_service::GovernanceConfig,
) -> Result<serde_json::Value, ApiError> {
    let mut value = serde_json::to_value(config)
        .map_err(|error| ApiError::internal(format!("serializing governance config: {error}")))?;
    if let Some(root) = value.as_object_mut() {
        root.remove("revision");
    }
    Ok(value)
}

fn keyed_array_field(path: &str) -> Option<&'static str> {
    if path == "packages" {
        Some("id")
    } else if path.ends_with(".mcp") {
        Some("name")
    } else {
        None
    }
}

fn keyed_array<'a>(
    value: Option<&'a serde_json::Value>,
    key: &str,
) -> Option<BTreeMap<String, &'a serde_json::Value>> {
    let array = value?.as_array()?;
    let mut result = BTreeMap::new();
    for item in array {
        let identity = item.get(key)?.as_str()?.to_owned();
        if result.insert(identity, item).is_some() {
            return None;
        }
    }
    Some(result)
}

fn merge_governance_value(
    baseline: Option<&serde_json::Value>,
    current: Option<&serde_json::Value>,
    incoming: Option<&serde_json::Value>,
    path: &str,
    conflicts: &mut Vec<String>,
) -> Option<serde_json::Value> {
    if current == baseline {
        return incoming.cloned();
    }
    if incoming == baseline || current == incoming {
        return current.cloned();
    }

    if let (Some(now), Some(next)) = (
        current.and_then(serde_json::Value::as_object),
        incoming.and_then(serde_json::Value::as_object),
    ) {
        let base = baseline.and_then(serde_json::Value::as_object);
        let keys = base
            .into_iter()
            .flat_map(|value| value.keys())
            .chain(now.keys())
            .chain(next.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut merged = serde_json::Map::new();
        for key in keys {
            let child_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            if let Some(value) = merge_governance_value(
                base.and_then(|value| value.get(&key)),
                now.get(&key),
                next.get(&key),
                &child_path,
                conflicts,
            ) {
                merged.insert(key, value);
            }
        }
        return Some(serde_json::Value::Object(merged));
    }

    if let Some(key) = keyed_array_field(path) {
        if let (Some(now), Some(next)) = (keyed_array(current, key), keyed_array(incoming, key)) {
            let base = keyed_array(baseline, key).unwrap_or_default();
            let identities = base
                .keys()
                .chain(now.keys())
                .chain(next.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            let mut merged = Vec::new();
            for identity in identities {
                let child_path = format!("{path}[{key}={identity}]");
                if let Some(value) = merge_governance_value(
                    base.get(&identity).copied(),
                    now.get(&identity).copied(),
                    next.get(&identity).copied(),
                    &child_path,
                    conflicts,
                ) {
                    merged.push(value);
                }
            }
            return Some(serde_json::Value::Array(merged));
        }
    }

    conflicts.push(if path.is_empty() {
        "<root>".into()
    } else {
        path.into()
    });
    current.cloned()
}

fn changed_paths(
    left: Option<&serde_json::Value>,
    right: Option<&serde_json::Value>,
    path: &str,
    changes: &mut Vec<String>,
) {
    if left == right {
        return;
    }
    if let (Some(left), Some(right)) = (
        left.and_then(serde_json::Value::as_object),
        right.and_then(serde_json::Value::as_object),
    ) {
        for key in left
            .keys()
            .chain(right.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
        {
            let child = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            changed_paths(left.get(&key), right.get(&key), &child, changes);
        }
        return;
    }
    if let Some(key) = keyed_array_field(path) {
        if let (Some(left), Some(right)) = (keyed_array(left, key), keyed_array(right, key)) {
            for identity in left
                .keys()
                .chain(right.keys())
                .cloned()
                .collect::<BTreeSet<_>>()
            {
                let child = format!("{path}[{key}={identity}]");
                changed_paths(
                    left.get(&identity).copied(),
                    right.get(&identity).copied(),
                    &child,
                    changes,
                );
            }
            return;
        }
    }
    changes.push(if path.is_empty() {
        "<root>".into()
    } else {
        path.into()
    });
}

#[derive(Deserialize)]
struct DeploymentManagedSource {
    connection_id: String,
    repository: String,
    requested_ref: String,
    resolved_commit: String,
}

async fn materialize_managed_source(
    pool: &PgPool,
    config: &AppConfig,
    package_blob: &BlobStore,
    org_id: Uuid,
    user_id: Uuid,
    source: &DeploymentManagedSource,
    expected_sha256: &str,
) -> Result<(Uuid, String), ApiError> {
    #[derive(FromRow)]
    struct Existing {
        id: Uuid,
        source_ref: String,
        sha256: String,
    }
    if let Some(existing) = sqlx::query_as!(Existing,
        "SELECT id,source_ref,sha256 FROM package_artifacts WHERE organization_id=$1 AND connection_id=$2 AND repository=$3 AND resolved_commit=$4",
        org_id,
        &source.connection_id,
        &source.repository,
        &source.resolved_commit)
        .fetch_optional(pool).await?
    {
        if !existing.sha256.eq_ignore_ascii_case(expected_sha256) {
            return Err(ApiError::internal(format!(
                "managed package {}@{} has a different stored digest",
                source.repository, source.resolved_commit
            )));
        }
        return Ok((existing.id, existing.source_ref));
    }

    let org_slug = organization_slug(pool, org_id).await?;
    let connection = config
        .package_source_connections
        .iter()
        .find(|connection| connection.id == source.connection_id)
        .ok_or_else(|| {
            ApiError::internal(format!(
                "managed package connection `{}` is not configured",
                source.connection_id
            ))
        })?;
    let (namespace, _) = safe_repository(&source.repository)?;
    let allowed = connection
        .organizations
        .get(&org_slug)
        .is_some_and(|namespaces| {
            namespaces
                .iter()
                .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(namespace))
        });
    if !allowed {
        return Err(ApiError::internal(format!(
            "managed package repository `{}` is not allowed for organization `{org_slug}`",
            source.repository
        )));
    }
    let (client, resolved) =
        resolve_provider_source(connection, &source.repository, &source.resolved_commit).await?;
    if resolved.commit != source.resolved_commit {
        return Err(ApiError::internal(format!(
            "managed package source resolved to {} instead of pinned commit {}",
            resolved.commit, source.resolved_commit
        )));
    }
    let bytes = fetch_provider_archive(&client, connection, &source.repository, &resolved).await?;
    use sha2::Digest as _;
    let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
    if !sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(ApiError::internal(format!(
            "managed package {}@{} digest mismatch",
            source.repository, source.resolved_commit
        )));
    }
    let object_key = format!("{org_id}/{sha256}.tar.gz");
    let size_bytes = bytes.len() as i64;
    package_blob
        .put(&object_key, "application/gzip", &sha256, bytes)
        .await?;
    let id = Uuid::new_v4();
    let stored: Existing = sqlx::query_as!(Existing,
        "INSERT INTO package_artifacts (id,organization_id,connection_id,provider,repository,requested_ref,resolved_commit,source_ref,object_key,sha256,size_bytes,created_by) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT (organization_id,connection_id,repository,resolved_commit) DO UPDATE SET resolved_commit=EXCLUDED.resolved_commit RETURNING id,source_ref,sha256",
        id, org_id, &source.connection_id, &connection.provider, &source.repository,
        &source.requested_ref, &source.resolved_commit, &resolved.source_ref, object_key,
        &sha256, size_bytes, user_id
    )
        .fetch_one(pool).await?;
    if stored.sha256 != sha256 {
        return Err(ApiError::internal(
            "managed package artifact changed during import",
        ));
    }
    Ok((stored.id, stored.source_ref))
}

async fn materialize_deployment_package_sources(
    pool: &PgPool,
    config: &AppConfig,
    package_blob: &BlobStore,
    org_id: Uuid,
    user_id: Uuid,
    yaml: &str,
) -> Result<String, ApiError> {
    let mut document: serde_yaml::Value = serde_yaml::from_str(yaml).map_err(|error| {
        ApiError::internal(format!("parsing deployment governance config: {error}"))
    })?;
    let Some(packages) = document
        .get_mut(yaml_key("packages"))
        .and_then(serde_yaml::Value::as_sequence_mut)
    else {
        return Ok(yaml.to_owned());
    };
    for package in packages {
        let Some(mapping) = package.as_mapping_mut() else {
            continue;
        };
        let Some(source_value) = mapping.remove(yaml_key("managed_source")) else {
            continue;
        };
        let source: DeploymentManagedSource =
            serde_yaml::from_value(source_value).map_err(|error| {
                ApiError::internal(format!("parsing managed package source: {error}"))
            })?;
        let sha256 = mapping
            .get(yaml_key("sha256"))
            .and_then(serde_yaml::Value::as_str)
            .ok_or_else(|| ApiError::internal("managed package source requires sha256"))?
            .to_owned();
        let (artifact_id, source_ref) = materialize_managed_source(
            pool,
            config,
            package_blob,
            org_id,
            user_id,
            &source,
            &sha256,
        )
        .await?;
        mapping.insert(
            yaml_key("artifact_id"),
            serde_yaml::Value::String(artifact_id.to_string()),
        );
        mapping.insert(
            yaml_key("source_ref"),
            serde_yaml::Value::String(source_ref),
        );
    }
    serde_yaml::to_string(&document).map_err(|error| {
        ApiError::internal(format!(
            "serializing materialized governance config: {error}"
        ))
    })
}

async fn upsert_deployment_governance_state<'a>(
    executor: impl sqlx::PgExecutor<'a>,
    org_id: Uuid,
    baseline_document: &serde_json::Value,
    source_sha256: &str,
) -> Result<(), ApiError> {
    sqlx::query!("INSERT INTO deployment_governance_state (organization_id,baseline_document,source_sha256) VALUES ($1,$2,$3) ON CONFLICT (organization_id) DO UPDATE SET baseline_document=EXCLUDED.baseline_document,source_sha256=EXCLUDED.source_sha256,updated_at=now()",
        org_id,
        baseline_document,
        source_sha256).execute(executor).await?;
    Ok(())
}

async fn reconcile_deployment_governance(
    pool: &PgPool,
    config: &AppConfig,
    package_blob: &BlobStore,
    org_id: Uuid,
    bootstrap_user_id: Uuid,
) -> Result<(), ApiError> {
    let document = read_blue_config_with_overlays(
        &config.blue_config_file,
        &config.blue_config_overlay_files,
    )?;
    let source_yaml = governance_seed_yaml(&document, config.gateway_kind.as_deref())?;
    use sha2::Digest as _;
    let source_sha256 = hex::encode(sha2::Sha256::digest(source_yaml.as_bytes()));
    let source_yaml = materialize_deployment_package_sources(
        pool,
        config,
        package_blob,
        org_id,
        bootstrap_user_id,
        &source_yaml,
    )
    .await?;
    let incoming =
        gh_service::source::parse_config(std::path::Path::new("config.yaml"), &source_yaml)
            .map_err(|error| {
                ApiError::internal(format!("invalid deployment governance config: {error}"))
            })?;
    validate_complete_governance(&incoming).map_err(ApiError::internal)?;
    if let Some(error) = uncertified_harness_range(&incoming) {
        tracing::warn!("deployment governance config: {error}");
    }
    let incoming_value = normalized_governance_value(&incoming)?;

    let current = sqlx::query_as!(ConfigRow,
        "SELECT revision,yaml,document,created_at FROM governance_config_revisions WHERE organization_id=$1 ORDER BY id DESC LIMIT 1",
        org_id)
    .fetch_optional(pool)
    .await?;

    if current.is_none() {
        let row = insert_governance_revision(
            pool,
            org_id,
            Some(bootstrap_user_id),
            &source_yaml,
            "bootstrap",
            None,
            None,
        )
        .await?;
        let baseline =
            normalized_governance_value(&serde_json::from_value(row.document).map_err(
                |error| ApiError::internal(format!("decoding bootstrap config: {error}")),
            )?)?;
        // The bootstrap row is written once per organization, but a replica that
        // lost the startup race can still arrive here behind a stale read, so the
        // seed upserts like every other writer of this table.
        upsert_deployment_governance_state(pool, org_id, &baseline, &source_sha256).await?;
        return Ok(());
    }
    let current = current.expect("checked above");
    let current_config: gh_service::GovernanceConfig =
        serde_json::from_value(current.document.clone()).map_err(|error| {
            ApiError::internal(format!("decoding current governance config: {error}"))
        })?;
    let current_value = normalized_governance_value(&current_config)?;
    let baseline: Option<serde_json::Value> = sqlx::query_scalar!(
        "SELECT baseline_document FROM deployment_governance_state WHERE organization_id=$1",
        org_id
    )
    .fetch_optional(pool)
    .await?;
    let baseline = match baseline {
        Some(value) => value,
        None => {
            let oldest: serde_json::Value = sqlx::query_scalar!("SELECT document FROM governance_config_revisions WHERE organization_id=$1 ORDER BY id ASC LIMIT 1",
        org_id).fetch_one(pool).await?;
            let oldest: gh_service::GovernanceConfig =
                serde_json::from_value(oldest).map_err(|error| {
                    ApiError::internal(format!("decoding original governance config: {error}"))
                })?;
            normalized_governance_value(&oldest)?
        }
    };
    let mut conflicts = Vec::new();
    let merged_value = merge_governance_value(
        Some(&baseline),
        Some(&current_value),
        Some(&incoming_value),
        "",
        &mut conflicts,
    )
    .ok_or_else(|| ApiError::internal("deployment governance merge removed the document"))?;
    let mut merged_document = merged_value.clone();
    merged_document
        .as_object_mut()
        .ok_or_else(|| ApiError::internal("merged governance config is not an object"))?
        .insert(
            "revision".into(),
            serde_json::Value::String(current.revision.clone()),
        );
    let mut merged: gh_service::GovernanceConfig = serde_json::from_value(merged_document)
        .map_err(|error| {
            ApiError::internal(format!("decoding merged governance config: {error}"))
        })?;
    validate_complete_governance(&merged).map_err(|error| {
        ApiError::internal(format!("merged governance config is invalid: {error}"))
    })?;
    if let Some(error) = uncertified_harness_range(&merged) {
        tracing::warn!("effective governance config: {error}");
    }

    let mut transaction = pool.begin().await?;
    if merged_value != current_value {
        let revision = Uuid::new_v4().to_string();
        merged.revision = revision.clone();
        let canonical_yaml = serde_yaml::to_string(&merged).map_err(|error| {
            ApiError::internal(format!("serializing deployment revision: {error}"))
        })?;
        let document = serde_json::to_value(&merged).map_err(|error| {
            ApiError::internal(format!("serializing deployment revision: {error}"))
        })?;
        sqlx::query!("INSERT INTO governance_config_revisions (organization_id,revision,yaml,document,created_by,origin) VALUES ($1,$2,$3,$4,NULL,'deployment')",
        org_id,
        &revision,
        canonical_yaml,
        document).execute(&mut *transaction).await?;
        persist_package_audiences(
            &mut transaction,
            Some(&current.revision),
            &revision,
            &merged.packages,
            None,
        )
        .await?;
    }
    upsert_deployment_governance_state(&mut *transaction, org_id, &incoming_value, &source_sha256)
        .await?;
    transaction.commit().await?;

    let effective: serde_json::Value = if merged_value == current_value {
        current_value
    } else {
        merged_value
    };
    let mut divergence = Vec::new();
    changed_paths(Some(&incoming_value), Some(&effective), "", &mut divergence);
    divergence.extend(conflicts);
    divergence.sort();
    divergence.dedup();
    if !divergence.is_empty() {
        tracing::warn!(
            organization_id = %org_id,
            skipped_paths = ?divergence,
            skipped_count = divergence.len(),
            "deployment governance values were not loaded because saved dashboard configuration takes precedence"
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Principal {
    user_id: Uuid,
    organization_id: Uuid,
    subject: String,
    email: String,
    role: String,
    expires_at: OffsetDateTime,
    scopes: Vec<String>,
    oauth_session_id: Option<String>,
    /// Signature-verified `iat` of the presenting bearer token. Gateway
    /// reactivation compares it against the recorded revocation instant, so
    /// it must come from the token, never from the server clock.
    issued_at: Option<OffsetDateTime>,
    bearer_token: bool,
}

#[derive(Clone, Copy)]
enum UserPolicy {
    Authenticated,
    Scope(&'static str),
    Admin,
}

#[derive(Clone)]
struct UserGuardState {
    app: Arc<AppState>,
    policy: UserPolicy,
}

impl UserGuardState {
    fn new(app: Arc<AppState>, policy: UserPolicy) -> Self {
        Self { app, policy }
    }
}

async fn user_auth_guard(
    State(guard): State<UserGuardState>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let who = principal(&guard.app, request.headers()).await?;
    authorize_user_policy(&who, guard.policy)?;
    request.extensions_mut().insert(who);
    Ok(next.run(request).await)
}

fn authorize_user_policy(who: &Principal, policy: UserPolicy) -> Result<(), ApiError> {
    match policy {
        UserPolicy::Authenticated => {}
        UserPolicy::Scope(scope) => require_scope(who, scope)?,
        UserPolicy::Admin => require_admin(who)?,
    }
    Ok(())
}

fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(ApiError::unauthorized)
}

async fn principal(state: &AppState, headers: &HeaderMap) -> Result<Principal, ApiError> {
    let identity = if let Ok(token) = bearer(headers) {
        jwt_identity(state, token).await?
    } else {
        dashboard_identity(state, headers).await?
    };
    upsert_principal(state, identity).await
}

#[derive(Deserialize)]
struct JwtClaims {
    sub: String,
    email: String,
    org_id: String,
    role: String,
    exp: i64,
    #[serde(default)]
    iat: Option<i64>,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    sid: Option<String>,
}

struct AuthIdentity {
    subject: String,
    email: String,
    organization_id: Uuid,
    role: String,
    expires_at: OffsetDateTime,
    issued_at: Option<OffsetDateTime>,
    bearer_token: bool,
    scopes: Vec<String>,
    oauth_session_id: Option<String>,
}

/// Verifies a Better Auth-issued JWT (signature via cached JWKS, issuer,
/// audience and expiry) and deserializes its claims into `T`. Shared by the
/// user-token path (`jwt_identity`) and the service-token path
/// (`authorize_internal`) so both apply identical cryptographic validation.
async fn decode_and_validate_jwt<T: serde::de::DeserializeOwned>(
    state: &AppState,
    token: &str,
) -> Result<T, ApiError> {
    let header = decode_header(token).map_err(|_| ApiError::unauthorized())?;
    let kid = header.kid.as_deref().ok_or_else(ApiError::unauthorized)?;
    let jwk = jwk_for(state, kid).await?;
    let key = DecodingKey::from_jwk(&jwk).map_err(|_| ApiError::unauthorized())?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[state.config.auth_issuer.as_str()]);
    validation.set_audience(&[state.config.auth_audience.as_str()]);
    Ok(decode::<T>(token, &key, &validation)
        .map_err(|error| {
            tracing::warn!(%error, "OAuth access token rejected");
            ApiError::unauthorized()
        })?
        .claims)
}

#[derive(Deserialize)]
struct ServiceClaims {
    sub: String,
    client_id: String,
    azp: String,
    #[allow(dead_code)]
    exp: i64,
    #[serde(default)]
    scope: String,
}

/// Authorizes an internal `/internal/gateway/*` request from the inference
/// proxy. Verifies the OAuth2 client-credentials service token against the
/// better-auth JWKS (via [`decode_and_validate_jwt`]), then requires the
/// `gateway:resolve` scope and pins the subject to the configured client id.
/// Unlike [`principal`], it never touches the `users`/`organizations` tables —
/// the service token intentionally carries no `org_id`/`email`.
///
/// Returns `401` for a missing/invalid/expired/wrong-subject token and `403`
/// when the token is valid but lacks the `gateway:resolve` scope.
async fn authorize_internal(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let token = bearer(headers)?;
    let claims: ServiceClaims = decode_and_validate_jwt(state, token).await?;
    authorize_service_claims(&claims, &state.config.internal_allowed_client_id)
}

async fn internal_auth_guard(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    authorize_internal(&state, request.headers()).await?;
    Ok(next.run(request).await)
}

/// Post-verification authorization decision for an already cryptographically
/// validated service token: pins the subject to the allowed client id (401 on
/// mismatch) and requires the `gateway:resolve` scope (403 when absent).
fn authorize_service_claims(
    claims: &ServiceClaims,
    allowed_client_id: &str,
) -> Result<(), ApiError> {
    if claims.sub != allowed_client_id
        || claims.client_id != allowed_client_id
        || claims.azp != allowed_client_id
    {
        tracing::warn!(sub = %claims.sub, client_id = %claims.client_id, azp = %claims.azp, "internal gateway token has an unexpected client identity");
        return Err(ApiError::unauthorized());
    }
    let authorized = claims
        .scope
        .split_whitespace()
        .any(|scope| scope == "gateway:resolve");
    if !authorized {
        return Err(ApiError::forbidden_message(
            "token is missing the gateway:resolve scope",
        ));
    }
    Ok(())
}

async fn jwt_identity(state: &AppState, token: &str) -> Result<AuthIdentity, ApiError> {
    let claims: JwtClaims = decode_and_validate_jwt(state, token).await?;
    Ok(AuthIdentity {
        subject: claims.sub,
        email: claims.email,
        organization_id: claims
            .org_id
            .parse()
            .map_err(|_| ApiError::unauthorized())?,
        role: claims.role,
        expires_at: OffsetDateTime::from_unix_timestamp(claims.exp)
            .map_err(|_| ApiError::unauthorized())?,
        issued_at: claims
            .iat
            .map(OffsetDateTime::from_unix_timestamp)
            .transpose()
            .map_err(|_| ApiError::unauthorized())?,
        bearer_token: true,
        scopes: claims.scope.split_whitespace().map(str::to_owned).collect(),
        oauth_session_id: claims.sid,
    })
}

async fn jwk_for(state: &AppState, kid: &str) -> Result<Jwk, ApiError> {
    {
        let cached = state.jwks.read().await;
        if let Some(cache) = cached
            .as_ref()
            .filter(|cache| cache.fetched_at.elapsed().as_secs() < 300)
        {
            if let Some(key) = cache
                .set
                .keys
                .iter()
                .find(|key| key.common.key_id.as_deref() == Some(kid))
            {
                return Ok(key.clone());
            }
        }
    }
    let mut last_error = None;
    let mut set = None;
    for delay_ms in [0, 100, 250, 500, 1000] {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        match state.http.get(&state.config.auth_jwks_url).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<JwkSet>().await {
                    Ok(value) => {
                        set = Some(value);
                        break;
                    }
                    Err(error) => last_error = Some(format!("decoding auth JWKS: {error}")),
                },
                Err(error) => last_error = Some(format!("auth JWKS rejected: {error}")),
            },
            Err(error) => last_error = Some(format!("fetching auth JWKS: {error}")),
        }
    }
    let set = set.ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            last_error.unwrap_or_else(|| "auth JWKS is unavailable".into()),
        )
    })?;
    let key = set
        .keys
        .iter()
        .find(|key| key.common.key_id.as_deref() == Some(kid))
        .cloned()
        .ok_or_else(ApiError::unauthorized)?;
    *state.jwks.write().await = Some(CachedJwks {
        set,
        fetched_at: std::time::Instant::now(),
    });
    Ok(key)
}

#[derive(Deserialize)]
struct DashboardSession {
    user: DashboardUser,
    session: DashboardSessionData,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DashboardUser {
    id: String,
    email: String,
    organization_id: Option<String>,
    governance_role: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DashboardSessionData {
    expires_at: String,
}

async fn dashboard_identity(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthIdentity, ApiError> {
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(ApiError::unauthorized)?;
    let response = state
        .http
        .get(&state.config.auth_session_url)
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|error| ApiError::internal(format!("checking dashboard session: {error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::unauthorized());
    }
    let session = response
        .json::<Option<DashboardSession>>()
        .await
        .map_err(|error| ApiError::internal(format!("decoding dashboard session: {error}")))?
        .ok_or_else(ApiError::unauthorized)?;
    Ok(AuthIdentity {
        subject: session.user.id,
        email: session.user.email,
        organization_id: session
            .user
            .organization_id
            .as_deref()
            .ok_or_else(ApiError::unauthorized)?
            .parse()
            .map_err(|_| ApiError::unauthorized())?,
        role: session
            .user
            .governance_role
            .unwrap_or_else(|| "member".into()),
        expires_at: OffsetDateTime::parse(&session.session.expires_at, &Rfc3339)
            .map_err(|_| ApiError::unauthorized())?,
        issued_at: None,
        bearer_token: false,
        scopes: vec![
            "governance:read".into(),
            "session:write".into(),
            "client-status:write".into(),
        ],
        oauth_session_id: None,
    })
}

async fn upsert_principal(state: &AppState, identity: AuthIdentity) -> Result<Principal, ApiError> {
    if identity.role != "admin" && identity.role != "member" {
        return Err(ApiError::forbidden());
    }
    let exists: bool = sqlx::query_scalar!(
        "select exists(select 1 from organizations where id=$1) as \"exists!\"",
        identity.organization_id
    )
    .fetch_one(&state.pool)
    .await?;
    if !exists {
        return Err(ApiError::forbidden());
    }
    let existing = sqlx::query!(
        "select id,subject,status,tokens_valid_after from users \
         where organization_id=$1 and lower(email)=lower($2)",
        identity.organization_id,
        &identity.email
    )
    .fetch_optional(&state.pool)
    .await?;
    let user_id = if let Some(existing) = existing {
        let id = existing.id;
        let subject = existing.subject;
        let status = existing.status;
        let tokens_valid_after = existing.tokens_valid_after;
        if status == "suspended" || (status == "removed" && subject == identity.subject) {
            return Err(ApiError::unauthorized());
        }
        if let Some(valid_after) = tokens_valid_after {
            if identity.bearer_token && identity.issued_at.is_none() {
                return Err(ApiError::unauthorized());
            }
            if let Some(issued_at) = identity.issued_at {
                if issued_at <= valid_after {
                    return Err(ApiError::unauthorized());
                }
            }
        }
        sqlx::query!(
            "update users set subject=$1,role=$2,status='active',active=true,updated_at=now() \
             where id=$3",
            &identity.subject,
            &identity.role,
            id
        )
        .execute(&state.pool)
        .await?;
        id
    } else {
        sqlx::query_scalar!(
            "insert into users (id,organization_id,subject,email,role,status,active) \
             values ($1,$2,$3,$4,$5,'active',true) returning id",
            Uuid::new_v4(),
            identity.organization_id,
            &identity.subject,
            &identity.email,
            &identity.role
        )
        .fetch_one(&state.pool)
        .await?
    };
    Ok(Principal {
        user_id,
        organization_id: identity.organization_id,
        subject: identity.subject,
        email: identity.email,
        role: identity.role,
        expires_at: identity.expires_at,
        scopes: identity.scopes,
        oauth_session_id: identity.oauth_session_id,
        issued_at: identity.issued_at,
        bearer_token: identity.bearer_token,
    })
}

fn require_admin(principal: &Principal) -> Result<(), ApiError> {
    if principal.role == "admin" {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

fn require_scope(principal: &Principal, scope: &str) -> Result<(), ApiError> {
    if principal.scopes.iter().any(|candidate| candidate == scope) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("missing OAuth scope {scope}"),
        ))
    }
}

#[derive(Serialize)]
struct IdentityResponse {
    id: Uuid,
    sub: String,
    email: String,
    org_id: Uuid,
    role: String,
    expires_at: i64,
    current_revision: String,
}

async fn current_identity(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<IdentityResponse>, ApiError> {
    let current_revision = current_config(&state.pool, who.organization_id)
        .await?
        .revision;
    Ok(Json(IdentityResponse {
        id: who.user_id,
        sub: who.subject,
        email: who.email,
        org_id: who.organization_id,
        role: who.role,
        expires_at: who.expires_at.unix_timestamp(),
        current_revision,
    }))
}

#[derive(FromRow)]
struct ConfigRow {
    revision: String,
    yaml: String,
    document: serde_json::Value,
    created_at: OffsetDateTime,
}

async fn current_config(pool: &PgPool, org_id: Uuid) -> Result<ConfigRow, ApiError> {
    sqlx::query_as!(
        ConfigRow,
        "SELECT revision, yaml, document, created_at FROM governance_config_revisions \
         WHERE organization_id = $1 ORDER BY id DESC LIMIT 1",
        org_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::not_found("organization has no governance configuration"))
}

async fn listen_for_revision_events(
    pool: PgPool,
    sender: tokio::sync::broadcast::Sender<RevisionSignal>,
) {
    loop {
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(listener) => listener,
            Err(error) => {
                tracing::warn!(%error, "governance revision listener could not connect");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        if let Err(error) = listener.listen("governance_revision").await {
            tracing::warn!(%error, "governance revision listener could not subscribe");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }
        // Any notifications committed while the database listener was down
        // may have been lost. Reconnect every SSE client so its initial event
        // resynchronizes against the current database row.
        let _ = sender.send(RevisionSignal::Resync);
        loop {
            match listener.recv().await {
                Ok(notification) => {
                    match serde_json::from_str::<RevisionEvent>(notification.payload()) {
                        Ok(event) => {
                            // No active SSE subscribers is normal.
                            let _ = sender.send(RevisionSignal::Revision(event));
                        }
                        Err(error) => {
                            tracing::warn!(%error, "invalid governance revision notification")
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "governance revision listener disconnected");
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn insert_governance_revision(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Option<Uuid>,
    yaml: &str,
    origin: &str,
    expected_base_revision: Option<&str>,
    package_audiences: Option<&BTreeMap<String, PackageAudience>>,
) -> Result<ConfigRow, ApiError> {
    let mut config = gh_service::source::parse_config(std::path::Path::new("config.yaml"), yaml)
        .map_err(|error| ApiError::bad_request(format!("invalid governance config: {error}")))?;
    validate_complete_governance(&config).map_err(ApiError::bad_request)?;
    // Every admin write lands here; the deployment's own bootstrap revision is
    // the one caller that must still go through, so it is never rejected for a
    // range the deployment file already carries.
    if origin != "bootstrap" {
        if let Some(error) = uncertified_harness_range(&config) {
            return Err(ApiError::bad_request(error));
        }
    }
    let revision = Uuid::new_v4().to_string();
    config.revision = revision.clone();
    let canonical_yaml = serde_yaml::to_string(&config)
        .map_err(|error| ApiError::internal(format!("serializing config: {error}")))?;
    let document = serde_json::to_value(&config)
        .map_err(|error| ApiError::internal(format!("serializing config: {error}")))?;
    let mut transaction = pool.begin().await?;
    let _organization_lock = sqlx::query_scalar!(
        "SELECT id FROM organizations WHERE id=$1 FOR UPDATE",
        org_id
    )
    .fetch_one(&mut *transaction)
    .await?;
    let previous_revision: Option<String> = sqlx::query_scalar!("SELECT revision FROM governance_config_revisions WHERE organization_id=$1 ORDER BY id DESC LIMIT 1",
        org_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if expected_base_revision.is_some_and(|expected| previous_revision.as_deref() != Some(expected))
    {
        return Err(ApiError::conflict(
            "governance configuration changed; reload before saving",
        ));
    }
    let row = sqlx::query_as!(ConfigRow,
        "INSERT INTO governance_config_revisions \
         (organization_id, revision, yaml, document, created_by, origin) VALUES ($1,$2,$3,$4,$5,$6) \
         RETURNING revision, yaml, document, created_at",
        org_id,
        &revision,
        canonical_yaml,
        document,
        user_id,
        origin)
    .fetch_one(&mut *transaction)
    .await?;
    persist_package_audiences(
        &mut transaction,
        previous_revision.as_deref(),
        &revision,
        &config.packages,
        package_audiences,
    )
    .await?;
    transaction.commit().await?;
    Ok(row)
}

async fn persist_package_audiences(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    previous_revision: Option<&str>,
    revision: &str,
    packages: &[gh_service::ManagedPackage],
    replacement: Option<&BTreeMap<String, PackageAudience>>,
) -> Result<(), ApiError> {
    for package in packages {
        let audience = if let Some(replacement) = replacement {
            replacement
                .get(&package.id)
                .cloned()
                .unwrap_or_else(PackageAudience::organization)
        } else if let Some(previous_revision) = previous_revision {
            let scope: Option<String> = sqlx::query_scalar!("SELECT scope FROM governance_package_audiences WHERE revision=$1 AND package_id=$2",
        previous_revision,
        &package.id)
            .fetch_optional(&mut **transaction)
            .await?;
            match scope.as_deref() {
                Some("users") => PackageAudience {
                    scope: PackageAudienceScope::Users,
                    user_ids: sqlx::query_scalar!("SELECT user_id FROM governance_package_audience_users WHERE revision=$1 AND package_id=$2 ORDER BY user_id",
        previous_revision,
        &package.id)
                    .fetch_all(&mut **transaction)
                    .await?,
                },
                _ => PackageAudience::organization(),
            }
        } else {
            PackageAudience::organization()
        };
        sqlx::query!("INSERT INTO governance_package_audiences (revision,package_id,scope) VALUES ($1,$2,$3)",
        revision,
        &package.id,
        audience.scope.as_str())
        .execute(&mut **transaction)
        .await?;
        for user_id in audience.user_ids {
            sqlx::query!("INSERT INTO governance_package_audience_users (revision,package_id,user_id) VALUES ($1,$2,$3)",
        revision,
        &package.id,
        user_id)
            .execute(&mut **transaction)
            .await?;
        }
    }
    Ok(())
}

async fn package_audiences_for_revision(
    pool: &PgPool,
    revision: &str,
    packages: &[gh_service::ManagedPackage],
) -> Result<BTreeMap<String, PackageAudience>, ApiError> {
    let mut audiences = packages
        .iter()
        .map(|package| (package.id.clone(), PackageAudience::organization()))
        .collect::<BTreeMap<_, _>>();
    let rows = sqlx::query!(
        "SELECT a.package_id,a.scope,m.user_id as \"user_id?\" FROM governance_package_audiences a \
         LEFT JOIN governance_package_audience_users m ON m.revision=a.revision AND m.package_id=a.package_id \
         WHERE a.revision=$1 ORDER BY a.package_id,m.user_id",
        revision
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let Some(audience) = audiences.get_mut(&row.package_id) else {
            continue;
        };
        if row.scope == "users" {
            if audience.scope != PackageAudienceScope::Users {
                audience.scope = PackageAudienceScope::Users;
                audience.user_ids.clear();
            }
            if let Some(user_id) = row.user_id {
                audience.user_ids.push(user_id);
            }
        }
    }
    Ok(audiences)
}

#[derive(Debug, Serialize)]
struct IdentityStatusResponse {
    auth_mode: String,
    scim: ScimIdentityStatus,
}

#[derive(Debug, Serialize)]
struct ScimIdentityStatus {
    configured: bool,
    group_role_mappings: BTreeMap<String, String>,
}

fn identity_status_response(config: &AppConfig) -> IdentityStatusResponse {
    IdentityStatusResponse {
        auth_mode: config.auth_mode.clone(),
        scim: ScimIdentityStatus {
            configured: config.scim_bearer_token.is_some(),
            group_role_mappings: config.scim_group_role_mappings.clone(),
        },
    }
}

async fn identity_status(State(state): State<Arc<AppState>>) -> Json<IdentityStatusResponse> {
    Json(identity_status_response(&state.config))
}

#[derive(Serialize)]
struct GatewayHarnessStatus {
    name: String,
    gateway_type: String,
}

#[derive(Serialize)]
struct GatewayRuntimeChecks {
    gateway_policy: bool,
    gateway_url: bool,
    inference_proxy_url: bool,
    provisioner: bool,
    secret_encryption: bool,
    internal_auth: bool,
    inference_jwt_signing: bool,
}

#[derive(Serialize)]
struct GatewayStatusResponse {
    enabled: bool,
    runtime_configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_gateway_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provisioner_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    harnesses: Option<Vec<GatewayHarnessStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_checks: Option<GatewayRuntimeChecks>,
}

#[derive(Serialize)]
struct GatewayProxyHealthResponse {
    status: &'static str,
    checked_at: String,
    latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn gateway_proxy_health(
    State(state): State<Arc<AppState>>,
) -> Result<Json<GatewayProxyHealthResponse>, ApiError> {
    let configured = state
        .config
        .gateway_inference_proxy_health_url
        .as_ref()
        .or(state.config.gateway_inference_proxy_url.as_ref())
        .ok_or_else(|| ApiError::bad_request("inference proxy URL is not configured"))?;
    let url = reqwest::Url::parse(configured)
        .and_then(|url| url.join("/health"))
        .map_err(|error| {
            ApiError::internal(format!("building inference proxy health URL: {error}"))
        })?;
    let started = Instant::now();
    let result = state
        .http
        .get(url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await;
    let latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let checked_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| {
            ApiError::internal(format!("formatting proxy health timestamp: {error}"))
        })?;
    let response = match result {
        Ok(response) if response.status().is_success() => GatewayProxyHealthResponse {
            status: "healthy",
            checked_at,
            latency_ms,
            http_status: Some(response.status().as_u16()),
            error: None,
        },
        Ok(response) => GatewayProxyHealthResponse {
            status: "unhealthy",
            checked_at,
            latency_ms,
            http_status: Some(response.status().as_u16()),
            error: Some(format!(
                "proxy returned HTTP {}",
                response.status().as_u16()
            )),
        },
        Err(error) => GatewayProxyHealthResponse {
            status: "unhealthy",
            checked_at,
            latency_ms,
            http_status: None,
            error: Some(if error.is_timeout() {
                "proxy health check timed out".into()
            } else {
                format!("proxy health check failed: {error}")
            }),
        },
    };
    Ok(Json(response))
}

async fn gateway_status(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<GatewayStatusResponse>, ApiError> {
    let row = current_config(&state.pool, who.organization_id).await?;
    let config: gh_service::GovernanceConfig = serde_json::from_value(row.document)
        .map_err(|error| ApiError::internal(format!("decoding stored config: {error}")))?;
    let enabled = config.gateway.is_some();
    let harnesses = config
        .gateway
        .as_ref()
        .map(|gateway| {
            config
                .allowed_harnesses
                .iter()
                .map(|name| GatewayHarnessStatus {
                    name: name.clone(),
                    gateway_type: gateway.kind.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    // The inference proxy now authenticates with a better-auth OAuth2
    // client-credentials token verified via JWKS, so "internal auth configured"
    // means the auth issuer/audience/JWKS endpoints are set (they always have
    // defaults) rather than a shared secret being present.
    let internal_auth = !state.config.auth_jwks_url.trim().is_empty()
        && !state.config.auth_issuer.trim().is_empty()
        && !state.config.auth_audience.trim().is_empty();
    let runtime_configured = managed_gateway_settings(&state.config)
        .ok()
        .flatten()
        .is_some()
        && internal_auth
        && state.secret_protector.is_some()
        && state.gateway_jwt.is_some();
    let runtime_checks = (who.role == "admin").then(|| GatewayRuntimeChecks {
        gateway_policy: enabled,
        gateway_url: state.config.gateway_upstream_url.is_some(),
        inference_proxy_url: state.config.gateway_inference_proxy_url.is_some(),
        provisioner: state.config.gateway_provisioner.is_some(),
        secret_encryption: state.secret_protector.is_some(),
        internal_auth,
        inference_jwt_signing: state.gateway_jwt.is_some(),
    });
    Ok(Json(GatewayStatusResponse {
        enabled,
        runtime_configured,
        inference_proxy_url: (who.role == "admin")
            .then(|| state.config.gateway_inference_proxy_url.clone())
            .flatten(),
        upstream_gateway_url: (who.role == "admin")
            .then(|| state.config.gateway_upstream_url.clone())
            .flatten(),
        provisioner_type: (who.role == "admin")
            .then(|| {
                state
                    .config
                    .gateway_provisioner
                    .as_ref()
                    .map(|p| p.kind.clone())
            })
            .flatten(),
        harnesses: (who.role == "admin").then_some(harnesses),
        runtime_checks,
    }))
}

#[derive(Clone, FromRow)]
struct ManagedGatewayCredentialRow {
    gateway_email: String,
    source_key_hash: Option<String>,
    source_key_alias: Option<String>,
    proxy_key_alias: Option<String>,
    provisioner_config_hash: Option<String>,
    provisioner_metadata: serde_json::Value,
    credential_expires_at: Option<OffsetDateTime>,
    last_reconciled_at: Option<OffsetDateTime>,
    provisioning_error: Option<String>,
    credential_ciphertext: Option<Vec<u8>>,
    credential_nonce: Option<Vec<u8>>,
    credential_wrapped_key: Option<Vec<u8>>,
    encryption_key_id: Option<String>,
    credential_version: Uuid,
    credential_state: String,
    invalidated_at: Option<OffsetDateTime>,
    invalidation_reason: Option<String>,
    recovery_attempts: i32,
    next_recovery_at: Option<OffsetDateTime>,
    recovery_lease_until: Option<OffsetDateTime>,
    last_validated_at: Option<OffsetDateTime>,
}

#[derive(Serialize)]
struct GatewayKeyResponse {
    enabled: bool,
    email: String,
    status: String,
    alias: Option<String>,
    external_id: Option<String>,
    expires_at: Option<String>,
    last_reconciled_at: Option<String>,
    error: Option<String>,
    invalidated_at: Option<String>,
    invalidation_reason: Option<String>,
    next_retry_at: Option<String>,
}

fn managed_gateway_settings(
    config: &AppConfig,
) -> Result<Option<(&str, &GatewayProvisionerConfig)>, ApiError> {
    match (
        &config.gateway_kind,
        &config.gateway_inference_proxy_url,
        &config.gateway_provisioner,
    ) {
        // With gateway.type unset, `AppConfig::with_gateway_gated_on_type` has
        // already nulled the runtime fields, so `gateway_kind == None` always
        // lands here as governance-only.
        (None, _, _) => Ok(None),
        (Some(_), Some(proxy), Some(provisioner)) if config.gateway_encryption.is_some() => {
            Ok(Some((proxy, provisioner)))
        }
        (Some(_), _, _) => Err(ApiError::internal(
            "gateway mode requires inference_proxy_url, provisioner, and secret_encryption",
        )),
    }
}

async fn managed_gateway_row(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<ManagedGatewayCredentialRow>, ApiError> {
    Ok(sqlx::query_as!(ManagedGatewayCredentialRow,
        "select gateway_email,source_key_hash,source_key_alias,proxy_key_alias,provisioner_config_hash,provisioner_metadata,credential_expires_at,last_reconciled_at,provisioning_error,credential_ciphertext,credential_nonce,credential_wrapped_key,encryption_key_id,credential_version,credential_state,invalidated_at,invalidation_reason,recovery_attempts,next_recovery_at,recovery_lease_until,last_validated_at from public.gateway_key_selections where user_id=$1",
        user_id).fetch_optional(pool).await?)
}

fn gateway_key_response(
    enabled: bool,
    email: String,
    row: Option<&ManagedGatewayCredentialRow>,
) -> GatewayKeyResponse {
    GatewayKeyResponse {
        enabled,
        email,
        // Gateway mode off is an authoritative, non-error state: report it as
        // "disabled" rather than "missing" so callers can tell "governance-only"
        // apart from "gateway on but not yet provisioned".
        status: if !enabled {
            "disabled"
        } else {
            match row {
                Some(row) if row.credential_state == "recovering" => "recovering",
                Some(row) if row.credential_state == "invalid" => "invalid",
                Some(row) if row.provisioning_error.is_some() => "error",
                Some(row) if row.credential_ciphertext.is_some() => "ready",
                _ => "missing",
            }
        }
        .into(),
        alias: row.and_then(|r| {
            r.proxy_key_alias
                .clone()
                .filter(|alias| !alias.trim().is_empty())
                .or_else(|| r.source_key_alias.clone())
        }),
        // A legacy row's source_key_hash identifies the old selected source
        // key, not the encrypted Blue-managed key. Do not present it as the
        // managed gateway identifier before normalization.
        external_id: row.and_then(|r| {
            r.provisioner_config_hash
                .as_ref()
                .and_then(|_| r.source_key_hash.clone())
        }),
        expires_at: row
            .and_then(|r| r.credential_expires_at)
            .and_then(|v| v.format(&Rfc3339).ok()),
        last_reconciled_at: row
            .and_then(|r| r.last_reconciled_at)
            .and_then(|v| v.format(&Rfc3339).ok()),
        error: row.and_then(|r| r.provisioning_error.clone()),
        invalidated_at: row
            .and_then(|r| r.invalidated_at)
            .and_then(|v| v.format(&Rfc3339).ok()),
        invalidation_reason: row.and_then(|r| r.invalidation_reason.clone()),
        next_retry_at: row
            .and_then(|r| r.next_recovery_at)
            .and_then(|v| v.format(&Rfc3339).ok()),
    }
}

fn defer_gateway_retry(
    manual: bool,
    attempts: i32,
    next_retry_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> bool {
    !manual && (attempts >= 5 || next_retry_at.is_some_and(|at| at > now))
}

async fn gateway_key(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<GatewayKeyResponse>, ApiError> {
    let enabled = managed_gateway_settings(&state.config)?.is_some();
    let row = if enabled {
        managed_gateway_row(&state.pool, who.user_id).await?
    } else {
        None
    };
    Ok(Json(gateway_key_response(enabled, who.email, row.as_ref())))
}

fn provisioner_api_error(error: ProvisionerError) -> ApiError {
    let status = match &error {
        ProvisionerError::InvalidConfig(_) => StatusCode::BAD_REQUEST,
        ProvisionerError::AccountMissing(_) => StatusCode::FORBIDDEN,
        ProvisionerError::Conflict(_) => StatusCode::CONFLICT,
        ProvisionerError::CredentialInvalid(_) => StatusCode::CONFLICT,
        ProvisionerError::Unavailable(_) | ProvisionerError::Rejected(_) => StatusCode::BAD_GATEWAY,
    };
    ApiError::new(status, error.to_string())
}

async fn decrypt_row_value(
    state: &AppState,
    row: &ManagedGatewayCredentialRow,
) -> Result<String, ApiError> {
    let protector = state
        .secret_protector
        .as_ref()
        .ok_or_else(|| ApiError::internal("gateway secret encryption is unavailable"))?;
    let (ciphertext, nonce, wrapped_key) = (
        &row.credential_ciphertext,
        &row.credential_nonce,
        &row.credential_wrapped_key,
    );
    let envelope = Envelope {
        ciphertext: ciphertext
            .clone()
            .ok_or_else(|| ApiError::conflict("gateway key must be provisioned"))?,
        nonce: nonce
            .clone()
            .ok_or_else(|| ApiError::internal("encrypted gateway value has no nonce"))?,
        wrapped_key: wrapped_key
            .clone()
            .ok_or_else(|| ApiError::internal("encrypted gateway value has no wrapped key"))?,
        key_id: row
            .encryption_key_id
            .clone()
            .ok_or_else(|| ApiError::internal("encrypted gateway value has no key id"))?,
    };
    let _permit = state
        .gateway_kms_permits
        .acquire()
        .await
        .map_err(|_| ApiError::internal("gateway decryption concurrency limiter closed"))?;
    let plaintext = tokio::time::timeout(
        std::time::Duration::from_secs(state.config.gateway_kms_timeout_seconds),
        protector.decrypt(&envelope),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway decryption timed out",
        )
    })??;
    String::from_utf8(plaintext.to_vec())
        .map_err(|_| ApiError::internal("decrypted gateway value is invalid"))
}

async fn ensure_managed_gateway_key(
    state: &AppState,
    who: &Principal,
    force: bool,
    recover_invalid: bool,
    manual: bool,
) -> Result<GatewayKeyResponse, ApiError> {
    let Some((_, provisioner)) = managed_gateway_settings(&state.config)? else {
        // Governance-only is fully supported: ensure is a no-op here. Return the
        // same disabled signal as GET /gateway/key (enabled:false, 200) instead
        // of a 400, so ensure-first CLI ordering stays fatal-free without any
        // brittle client-side string matching. A partial-misconfig still errors
        // via the Err(..) arms of managed_gateway_settings above.
        return Ok(gateway_key_response(false, who.email.clone(), None));
    };
    let protector = state
        .secret_protector
        .as_ref()
        .ok_or_else(|| ApiError::internal("gateway secret encryption is unavailable"))?;
    let _provisioning_permit = state
        .gateway_provisioning_permits
        .acquire()
        .await
        .map_err(|_| ApiError::internal("gateway provisioning concurrency limiter closed"))?;
    // A transaction-scoped advisory lock serializes this user's upstream
    // lifecycle across every Control API replica. The transaction contains no
    // business writes; dropping it on any early return releases the lock.
    let mut lifecycle_lock = state.pool.begin().await?;
    sqlx::query!(
        "select pg_advisory_xact_lock(hashtextextended($1, 928451))",
        who.user_id.to_string()
    )
    .execute(&mut *lifecycle_lock)
    .await?;
    // The selector identifies the linked implementation. Gateway-specific
    // policy belongs in that implementation, not in blue.yaml.
    let policy_revision = state
        .gateway_provisioner
        .as_ref()
        .and_then(|implementation| implementation.policy_revision());
    let config_bytes = serde_json::to_vec(&json!({
        "type": provisioner.kind,
        "policy_revision": policy_revision,
    }))
    .map_err(|e| ApiError::internal(e.to_string()))?;
    let config_hash = hex::encode(Sha256::digest(config_bytes));
    let mut existing = managed_gateway_row(&state.pool, who.user_id).await?;
    // Treat incomplete encrypted legacy rows as invalid credentials instead of
    // attempting to decrypt them and returning a 500. Clearing the complete
    // local linkage makes replacement atomic from the caller's perspective and
    // prevents a malformed row from trapping every future reconcile attempt.
    if existing.as_ref().is_some_and(|row| {
        row.credential_ciphertext.is_some()
            && (row.credential_nonce.is_none()
                || row.credential_wrapped_key.is_none()
                || row.encryption_key_id.is_none())
    }) {
        sqlx::query!("update public.gateway_key_selections set gateway_user_id=null,source_key_hash=null,source_key_alias=null,source_models='[]'::jsonb,proxy_key_alias=null,provisioner_config_hash=null,provisioner_metadata='{}'::jsonb,credential_expires_at=null,credential_ciphertext=null,credential_nonce=null,credential_wrapped_key=null,encryption_key_id=null,credential_version=gen_random_uuid(),credential_state='invalid',invalidated_at=now(),invalidation_reason='stored gateway credential is incomplete and cannot be decrypted',recovery_attempts=0,next_recovery_at=null,recovery_lease_until=null,last_reconciled_at=null,provisioning_error=null,updated_at=now() where user_id=$1",
        who.user_id)
        .execute(&state.pool)
        .await?;
        existing = managed_gateway_row(&state.pool, who.user_id).await?;
    }
    // Legacy rows used source_key_hash for the user-selected upstream key while
    // credential_ciphertext contains the Blue-managed proxy key. Normalize the
    // record before it is ever sent to a provisioner as a previous credential;
    // otherwise reconciliation could attempt to revoke the user's source key.
    if let Some(row) = existing
        .as_mut()
        .filter(|row| row.provisioner_config_hash.is_none() && row.credential_ciphertext.is_some())
    {
        let credential = decrypt_row_value(state, row).await?;
        let external_id = credential_digest(&credential);
        let alias = row
            .proxy_key_alias
            .as_deref()
            .filter(|alias| !alias.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("blue:{}", who.email.trim().to_ascii_lowercase()));
        sqlx::query!("update public.gateway_key_selections set source_key_hash=$2,source_key_alias=$3,proxy_key_alias=$3,updated_at=now() where user_id=$1",
        who.user_id,
        &external_id,
        &alias)
        .execute(&state.pool)
        .await?;
        row.source_key_hash = Some(external_id);
        row.source_key_alias = Some(alias.clone());
        row.proxy_key_alias = Some(alias);
    }
    let due = existing
        .as_ref()
        .and_then(|r| r.last_reconciled_at)
        .is_none_or(|at| {
            at + Duration::seconds(provisioner.reconcile_ttl_seconds) <= OffsetDateTime::now_utc()
        });
    let changed = existing
        .as_ref()
        .and_then(|r| r.provisioner_config_hash.as_deref())
        != Some(config_hash.as_str());
    let missing = existing
        .as_ref()
        .is_none_or(|r| r.credential_ciphertext.is_none());
    let errored = existing
        .as_ref()
        .is_some_and(|row| row.credential_state == "error" || row.provisioning_error.is_some());
    if missing {
        if let Some(row) = existing.as_ref() {
            if row.credential_state == "invalid" && !recover_invalid {
                return Ok(gateway_key_response(
                    true,
                    who.email.clone(),
                    existing.as_ref(),
                ));
            }
            if row.credential_state == "recovering"
                && row
                    .recovery_lease_until
                    .is_some_and(|at| at > OffsetDateTime::now_utc())
            {
                return Ok(gateway_key_response(
                    true,
                    who.email.clone(),
                    existing.as_ref(),
                ));
            }
            if defer_gateway_retry(
                manual,
                row.recovery_attempts,
                row.next_recovery_at,
                OffsetDateTime::now_utc(),
            ) {
                return Ok(gateway_key_response(
                    true,
                    who.email.clone(),
                    existing.as_ref(),
                ));
            }
        }
    }
    if errored {
        let row = existing.as_ref().expect("errored gateway row exists");
        if defer_gateway_retry(
            manual,
            row.recovery_attempts,
            row.next_recovery_at,
            OffsetDateTime::now_utc(),
        ) {
            return Ok(gateway_key_response(
                true,
                who.email.clone(),
                existing.as_ref(),
            ));
        }
    }
    if !force && !due && !changed && !missing && !errored {
        return Ok(gateway_key_response(
            true,
            who.email.clone(),
            existing.as_ref(),
        ));
    }
    let reason = if missing
        && existing
            .as_ref()
            .is_some_and(|row| row.credential_state == "invalid")
    {
        gh_gateway_provisioner::EnsureReason::CredentialInvalidated
    } else if missing {
        gh_gateway_provisioner::EnsureReason::Missing
    } else if changed {
        gh_gateway_provisioner::EnsureReason::ConfigurationChanged
    } else {
        gh_gateway_provisioner::EnsureReason::ReconciliationDue
    };
    let groups = sqlx::query_scalar!("select g.display_name from scim_groups g join scim_group_members m on m.group_id=g.id where m.user_id=$1 order by lower(g.display_name)",
        who.user_id)
    .fetch_all(&state.pool)
    .await?;
    let mut request = gh_gateway_provisioner::EnsureRequest {
        identity: gh_gateway_provisioner::UserIdentity {
            id: who.user_id.to_string(),
            email: who.email.clone(),
            organization_id: who.organization_id.to_string(),
            groups,
        },
        reason,
        previous: (!missing)
            .then_some(existing.as_ref())
            .flatten()
            .and_then(|r| {
                Some(gh_gateway_provisioner::PreviousCredential {
                    external_id: r.source_key_hash.clone()?,
                    alias: r.source_key_alias.clone().unwrap_or_default(),
                    metadata: r.provisioner_metadata.clone(),
                })
            }),
    };
    let implementation = state
        .gateway_provisioner
        .as_ref()
        .ok_or_else(|| ApiError::internal("gateway provisioner implementation is unavailable"))?;
    if missing
        && existing
            .as_ref()
            .is_some_and(|row| row.invalidation_reason.is_some())
    {
        sqlx::query!("update public.gateway_key_selections set credential_state='recovering',recovery_lease_until=now() + interval '2 minutes',updated_at=now() where user_id=$1",
        who.user_id).execute(&state.pool).await?;
    }
    let provisioned = match implementation.ensure(request.clone()).await {
        Ok(value) => value,
        Err(ProvisionerError::CredentialInvalid(message)) => {
            sqlx::query!("update public.gateway_key_selections set gateway_user_id=null,source_key_hash=null,source_key_alias=null,source_models='[]'::jsonb,proxy_key_alias=null,provisioner_config_hash=null,provisioner_metadata='{}'::jsonb,credential_expires_at=null,credential_ciphertext=null,credential_nonce=null,credential_wrapped_key=null,encryption_key_id=null,credential_version=gen_random_uuid(),credential_state='invalid',invalidated_at=now(),invalidation_reason=$2,recovery_attempts=0,next_recovery_at=null,recovery_lease_until=null,last_reconciled_at=null,provisioning_error=null,updated_at=now() where user_id=$1",
        who.user_id,
        &message)
            .execute(&state.pool)
            .await?;
            existing = managed_gateway_row(&state.pool, who.user_id).await?;
            if !recover_invalid {
                return Ok(gateway_key_response(
                    true,
                    who.email.clone(),
                    existing.as_ref(),
                ));
            }
            sqlx::query!("update public.gateway_key_selections set credential_state='recovering',recovery_lease_until=now() + interval '2 minutes',updated_at=now() where user_id=$1",
        who.user_id).execute(&state.pool).await?;
            request.reason = gh_gateway_provisioner::EnsureReason::CredentialInvalidated;
            request.previous = None;
            match implementation.ensure(request).await {
                Ok(value) => value,
                Err(provisioner_error) => {
                    let error = provisioner_api_error(provisioner_error);
                    sqlx::query!("update public.gateway_key_selections set credential_state='invalid',recovery_attempts=recovery_attempts+1,next_recovery_at=now() + make_interval(secs => least(3600,30 * (1 << least(recovery_attempts,7))) * (0.8 + random() * 0.4)),recovery_lease_until=null,provisioning_error=$2,updated_at=now() where user_id=$1",
        who.user_id,
        &error.message).execute(&state.pool).await?;
                    return Err(error);
                }
            }
        }
        Err(provisioner_error) => {
            let error = provisioner_api_error(provisioner_error);
            sqlx::query!("insert into public.gateway_key_selections(user_id,gateway_email,source_models,provisioning_error,credential_state,recovery_attempts,next_recovery_at) values($1,$2,'[]'::jsonb,$3,'error',1,now() + interval '30 seconds') on conflict(user_id) do update set provisioning_error=$3,credential_state=case when gateway_key_selections.invalidation_reason is not null then 'invalid' else 'error' end,recovery_attempts=gateway_key_selections.recovery_attempts+1,next_recovery_at=now() + make_interval(secs => least(3600,30 * (1 << least(gateway_key_selections.recovery_attempts,7))) * (0.8 + random() * 0.4)),recovery_lease_until=null,updated_at=now()",
        who.user_id,
        &who.email,
        &error.message).execute(&state.pool).await?;
            return Err(error);
        }
    };
    if provisioned.external_id.trim().is_empty()
        || provisioned.alias.trim().is_empty()
        || (existing
            .as_ref()
            .is_none_or(|row| row.credential_ciphertext.is_none())
            && provisioned.credential.is_none())
    {
        let error = ApiError::new(
            StatusCode::BAD_GATEWAY,
            "gateway provisioner returned an incomplete replacement credential",
        );
        if existing
            .as_ref()
            .is_some_and(|row| row.invalidation_reason.is_some())
        {
            sqlx::query!("update public.gateway_key_selections set credential_state='invalid',recovery_attempts=recovery_attempts+1,next_recovery_at=now() + make_interval(secs => least(3600,30 * (1 << least(recovery_attempts,7))) * (0.8 + random() * 0.4)),recovery_lease_until=null,provisioning_error=$2,updated_at=now() where user_id=$1",
        who.user_id,
        &error.message).execute(&state.pool).await?;
        }
        return Err(error);
    }
    let credential_envelope = if let Some(credential) = &provisioned.credential {
        protector.encrypt(credential.expose().as_bytes()).await?
    } else {
        let row = existing
            .as_ref()
            .filter(|row| row.credential_ciphertext.is_some())
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "gateway provisioner omitted a newly required credential",
                )
            })?;
        Envelope {
            ciphertext: row.credential_ciphertext.clone().unwrap(),
            nonce: row.credential_nonce.clone().unwrap(),
            wrapped_key: row.credential_wrapped_key.clone().unwrap(),
            key_id: row.encryption_key_id.clone().unwrap(),
        }
    };
    let expires_at = provisioned
        .expires_at
        .as_deref()
        .map(|v| {
            OffsetDateTime::parse(v, &Rfc3339).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "gateway provisioner returned an invalid RFC 3339 expires_at",
                )
            })
        })
        .transpose()?
        .or_else(|| existing.as_ref().and_then(|r| r.credential_expires_at));
    sqlx::query!("insert into public.gateway_key_selections(user_id,gateway_user_id,gateway_email,source_key_hash,source_key_alias,source_models,proxy_key_alias,credential_ciphertext,credential_nonce,credential_wrapped_key,encryption_key_id,provisioner_config_hash,provisioner_metadata,credential_expires_at,last_reconciled_at,provisioning_error,credential_state) values($1,$2,$3,$4,$5,'[]'::jsonb,$5,$6,$7,$8,$9,$10,$11,$12,now(),null,'ready') on conflict(user_id) do update set gateway_user_id=$2,gateway_email=$3,source_key_hash=$4,source_key_alias=$5,proxy_key_alias=$5,credential_ciphertext=$6,credential_nonce=$7,credential_wrapped_key=$8,encryption_key_id=$9,provisioner_config_hash=$10,provisioner_metadata=$11,credential_expires_at=$12,credential_version=gen_random_uuid(),last_reconciled_at=now(),last_validated_at=now(),provisioning_error=null,credential_state='ready',invalidated_at=null,invalidation_reason=null,recovery_attempts=0,next_recovery_at=null,recovery_lease_until=null,proxy_virtual_key=null,updated_at=now()",
        who.user_id,
        &provisioned.external_id,
        &who.email,
        &provisioned.external_id,
        &provisioned.alias,
        credential_envelope.ciphertext,
        credential_envelope.nonce,
        credential_envelope.wrapped_key,
        credential_envelope.key_id,
        config_hash,
        provisioned.metadata,
        expires_at).execute(&state.pool).await?;
    let row = managed_gateway_row(&state.pool, who.user_id).await?;
    Ok(gateway_key_response(true, who.email.clone(), row.as_ref()))
}

#[derive(Default, Deserialize)]
struct EnsureGatewayKeyInput {
    #[serde(default)]
    manual: bool,
}

async fn ensure_gateway_key(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    input: Option<Json<EnsureGatewayKeyInput>>,
) -> Result<Json<GatewayKeyResponse>, ApiError> {
    let manual = input.is_some_and(|Json(input)| input.manual);
    ensure_managed_gateway_key(&state, &who, false, manual, manual)
        .await
        .map(Json)
}

async fn validate_gateway_key(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<GatewayKeyResponse>, ApiError> {
    let existing = managed_gateway_row(&state.pool, who.user_id).await?;
    if existing
        .as_ref()
        .is_none_or(|row| row.credential_ciphertext.is_none())
    {
        return Ok(Json(gateway_key_response(
            true,
            who.email,
            existing.as_ref(),
        )));
    }
    if existing
        .as_ref()
        .and_then(|row| row.last_validated_at)
        .is_some_and(|at| at + Duration::seconds(60) > OffsetDateTime::now_utc())
    {
        return Ok(Json(gateway_key_response(
            true,
            who.email,
            existing.as_ref(),
        )));
    }
    ensure_managed_gateway_key(&state, &who, true, false, false)
        .await
        .map(Json)
}

#[derive(Deserialize)]
struct ResolveGatewayKeyRequest {
    user_id: Uuid,
    blue_oauth_session_id: String,
}

struct ResolvedGatewayCredentialRow {
    user_id: Uuid,
    organization_id: Uuid,
    gateway_session_expires_at: OffsetDateTime,
    /// When this binding was last reactivated after a `blue logout`. The proxy
    /// rejects any JWT whose `iat` predates it.
    session_not_before: Option<OffsetDateTime>,
    gateway_email: String,
    source_key_hash: Option<String>,
    source_key_alias: Option<String>,
    proxy_key_alias: Option<String>,
    provisioner_config_hash: Option<String>,
    provisioner_metadata: serde_json::Value,
    credential_expires_at: Option<OffsetDateTime>,
    last_reconciled_at: Option<OffsetDateTime>,
    provisioning_error: Option<String>,
    credential_ciphertext: Option<Vec<u8>>,
    credential_nonce: Option<Vec<u8>>,
    credential_wrapped_key: Option<Vec<u8>>,
    encryption_key_id: Option<String>,
    credential_version: Uuid,
    credential_state: String,
    invalidated_at: Option<OffsetDateTime>,
    invalidation_reason: Option<String>,
    recovery_attempts: i32,
    next_recovery_at: Option<OffsetDateTime>,
    recovery_lease_until: Option<OffsetDateTime>,
    last_validated_at: Option<OffsetDateTime>,
}

impl ResolvedGatewayCredentialRow {
    fn credential(&self) -> ManagedGatewayCredentialRow {
        ManagedGatewayCredentialRow {
            gateway_email: self.gateway_email.clone(),
            source_key_hash: self.source_key_hash.clone(),
            source_key_alias: self.source_key_alias.clone(),
            proxy_key_alias: self.proxy_key_alias.clone(),
            provisioner_config_hash: self.provisioner_config_hash.clone(),
            provisioner_metadata: self.provisioner_metadata.clone(),
            credential_expires_at: self.credential_expires_at,
            last_reconciled_at: self.last_reconciled_at,
            provisioning_error: self.provisioning_error.clone(),
            credential_ciphertext: self.credential_ciphertext.clone(),
            credential_nonce: self.credential_nonce.clone(),
            credential_wrapped_key: self.credential_wrapped_key.clone(),
            encryption_key_id: self.encryption_key_id.clone(),
            credential_version: self.credential_version,
            credential_state: self.credential_state.clone(),
            invalidated_at: self.invalidated_at,
            invalidation_reason: self.invalidation_reason.clone(),
            recovery_attempts: self.recovery_attempts,
            next_recovery_at: self.next_recovery_at,
            recovery_lease_until: self.recovery_lease_until,
            last_validated_at: self.last_validated_at,
        }
    }
}

async fn resolved_gateway_credential_row(
    pool: &PgPool,
    user_id: Uuid,
    oauth_session_id: &str,
) -> Result<Option<ResolvedGatewayCredentialRow>, sqlx::Error> {
    sqlx::query_as!(ResolvedGatewayCredentialRow,
        "select s.user_id,u.organization_id,least(ga.source_expires_at,ba.\"expiresAt\") as \"gateway_session_expires_at!\",ga.reactivated_at as session_not_before,s.gateway_email,s.source_key_hash,s.source_key_alias,s.proxy_key_alias,s.provisioner_config_hash,s.provisioner_metadata,s.credential_expires_at,s.last_reconciled_at,s.provisioning_error,s.credential_ciphertext,s.credential_nonce,s.credential_wrapped_key,s.encryption_key_id,s.credential_version,s.credential_state,s.invalidated_at,s.invalidation_reason,s.recovery_attempts,s.next_recovery_at,s.recovery_lease_until,s.last_validated_at from public.gateway_key_selections s join users u on u.id=s.user_id join public.gateway_auth_sessions ga on ga.user_id=s.user_id join auth.\"session\" ba on ba.id=ga.oauth_session_id and ba.\"userId\"=u.subject where s.user_id=$1 and ga.oauth_session_id=$2 and ga.revoked_at is null and ga.source_expires_at>now() and ba.\"expiresAt\">now() and u.active=true",
        user_id,
        oauth_session_id)
    .fetch_optional(pool)
    .await
}

async fn resolve_gateway_key(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ResolveGatewayKeyRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if managed_gateway_settings(&state.config)?.is_none() {
        return Err(ApiError::bad_request("gateway mode is not enabled"));
    }
    let resolved =
        resolved_gateway_credential_row(&state.pool, input.user_id, &input.blue_oauth_session_id)
            .await?
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "inactive gateway session"))?;
    let credential = resolved.credential();
    if credential
        .credential_expires_at
        .is_some_and(|expires_at| expires_at <= OffsetDateTime::now_utc())
    {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "gateway credential has expired",
        ));
    }
    let upstream_credential = decrypt_row_value(&state, &credential).await?;
    Ok(Json(json!({
        "upstream_credential": upstream_credential,
        "user": credential.gateway_email,
        "user_id": resolved.user_id,
        "organization_id": resolved.organization_id,
        "profile_id": credential.source_key_hash,
        "profile_name": credential.source_key_alias,
        "credential_version": credential.credential_version,
        "credential_expires_at": credential.credential_expires_at.and_then(|value| value.format(&Rfc3339).ok()),
        "gateway_session_expires_at": resolved.gateway_session_expires_at.format(&Rfc3339).ok(),
        "session_not_before": resolved.session_not_before.and_then(|value| value.format(&Rfc3339).ok()),
    })))
}

#[derive(Deserialize)]
struct InvalidGatewayCredentialReport {
    user_id: Uuid,
    credential_version: Uuid,
    reason: Option<gh_gateway::InvalidCredentialReason>,
    /// Historical LiteLLM-specific field retained for one compatibility
    /// release so old and new proxy replicas can overlap.
    classification: Option<String>,
}

fn invalid_credential_reason(
    report: &InvalidGatewayCredentialReport,
) -> Result<gh_gateway::InvalidCredentialReason, ApiError> {
    let legacy = report.classification.as_deref().map(|classification| {
        gh_gateway::InvalidCredentialReason::from_legacy_classification(classification)
            .ok_or_else(|| ApiError::bad_request("unsupported invalid-key classification"))
    });
    let legacy = legacy.transpose()?;
    match (report.reason, legacy) {
        (Some(reason), Some(legacy)) if reason != legacy => Err(ApiError::bad_request(
            "invalid-credential reason and legacy classification disagree",
        )),
        (Some(reason), _) | (None, Some(reason)) => Ok(reason),
        (None, None) => Err(ApiError::bad_request(
            "invalid-credential reason is required",
        )),
    }
}

async fn report_invalid_gateway_credential(
    State(state): State<Arc<AppState>>,
    Json(report): Json<InvalidGatewayCredentialReport>,
) -> Result<StatusCode, ApiError> {
    let reason = invalid_credential_reason(&report)?.invalidation_message();
    let mut lifecycle_lock = state.pool.begin().await?;
    sqlx::query!(
        "select pg_advisory_xact_lock(hashtextextended($1, 928451))",
        report.user_id.to_string()
    )
    .execute(&mut *lifecycle_lock)
    .await?;
    // The version predicate is the cross-replica deduplication boundary. The
    // first reporter clears the credential and changes the version; every
    // concurrent or delayed reporter then affects zero rows and does no
    // upstream work.
    let invalidated = sqlx::query!("update public.gateway_key_selections set gateway_user_id=null,source_key_hash=null,source_key_alias=null,source_models='[]'::jsonb,proxy_key_alias=null,provisioner_config_hash=null,provisioner_metadata='{}'::jsonb,credential_expires_at=null,credential_ciphertext=null,credential_nonce=null,credential_wrapped_key=null,encryption_key_id=null,credential_version=gen_random_uuid(),credential_state='invalid',invalidated_at=now(),invalidation_reason=$3,recovery_attempts=0,next_recovery_at=null,recovery_lease_until=null,last_reconciled_at=null,provisioning_error=null,updated_at=now() where user_id=$1 and credential_version=$2 and credential_ciphertext is not null",
        report.user_id,
        report.credential_version,
        reason)
    .execute(&state.pool)
    .await?
    .rows_affected()
        == 1;
    if !invalidated {
        return Ok(StatusCode::ACCEPTED);
    }
    lifecycle_lock.commit().await?;
    let who = sqlx::query!(
        "select id as user_id,organization_id,subject,email,role,now() + interval '1 hour' as \"expires_at!\",array[]::text[] as \"scopes!\" from users where id=$1 and active=true",
        report.user_id)
    .fetch_optional(&state.pool)
    .await?
    .map(|row| Principal {
        user_id: row.user_id,
        organization_id: row.organization_id,
        subject: row.subject,
        email: row.email,
        role: row.role,
        expires_at: row.expires_at,
        scopes: row.scopes,
        oauth_session_id: None,
        issued_at: None,
        bearer_token: false,
    });
    if let Some(who) = who {
        // The proxy sends this callback off its response path. Returning an
        // error here leaves persisted invalid/backoff state for lifecycle or
        // manual retry; it never causes request replay.
        let _ = ensure_managed_gateway_key(&state, &who, false, true, false).await;
    }
    Ok(StatusCode::ACCEPTED)
}

#[derive(FromRow, Serialize)]
struct GatewayCacheEvent {
    id: i64,
    user_id: Uuid,
    oauth_session_id: Option<String>,
    credential_version: Option<Uuid>,
    reason: String,
}

async fn gateway_cache_events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let last_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let oldest_id: Option<i64> =
        sqlx::query_scalar!("select min(id) from public.gateway_cache_events")
            .fetch_one(&state.pool)
            .await?;
    let replay_gap = last_id > 0 && oldest_id.is_some_and(|oldest| last_id < oldest - 1);
    let cursor = if replay_gap {
        oldest_id.unwrap_or(1) - 1
    } else {
        last_id
    };
    let pool = state.pool.clone();
    let replay = futures_util::stream::once(async move {
        replay_gap.then(|| Ok(Event::default().event("resync").data("{\"resync\":true}")))
    })
    .filter_map(|event| async move { event });
    let events = futures_util::stream::unfold((pool, cursor), |(pool, cursor)| async move {
        loop {
            match sqlx::query_as!(GatewayCacheEvent,
        "select id,user_id,oauth_session_id,credential_version,reason from public.gateway_cache_events where id>$1 order by id limit 1",
        cursor)
            .fetch_optional(&pool)
            .await
            {
                Ok(Some(event)) => {
                    let next = event.id;
                    let item = Event::default()
                        .id(next.to_string())
                        .event("credential")
                        .json_data(&event)
                        .unwrap_or_else(|_| Event::default().event("resync").data("{}"));
                    return Some((Ok(item), (pool, next)));
                }
                Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
                Err(error) => {
                    tracing::warn!(%error, "gateway invalidation stream query failed");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });
    Ok(Sse::new(replay.chain(events)).keep_alive(KeepAlive::default()))
}

#[derive(Clone, Deserialize)]
struct GatewayRequestLogInput {
    id: Uuid,
    organization_id: Uuid,
    user_id: Uuid,
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

#[derive(Deserialize)]
struct GatewayRequestLogBatchInput {
    events: Vec<GatewayRequestLogInput>,
}

async fn ingest_gateway_request_log_batch(
    State(state): State<Arc<AppState>>,
    Json(input): Json<GatewayRequestLogBatchInput>,
) -> Result<StatusCode, ApiError> {
    if input.events.is_empty() || input.events.len() > 500 {
        return Err(ApiError::bad_request(
            "events must contain between 1 and 500 request logs",
        ));
    }
    let now = OffsetDateTime::now_utc();
    for event in &input.events {
        let occurred_at = OffsetDateTime::parse(&event.occurred_at, &Rfc3339)
            .map_err(|_| ApiError::bad_request("occurred_at must be an RFC 3339 timestamp"))?;
        if occurred_at > now + Duration::minutes(5)
            || event.method.trim().is_empty()
            || event.method.len() > 16
            || !event.path.starts_with('/')
            || event.path.chars().count() > 2048
            || event
                .http_status
                .is_some_and(|status| !(100..=599).contains(&status))
        {
            return Err(ApiError::bad_request(
                "batch contains an invalid request log",
            ));
        }
        i64::try_from(event.upstream_latency_ms)
            .map_err(|_| ApiError::bad_request("upstream_latency_ms is too large"))?;
        limited_log_value(event.profile_id.clone(), "profile_id", 512)?;
        limited_log_value(event.profile_name.clone(), "profile_name", 512)?;
        limited_log_value(event.model.clone(), "model", 512)?;
        limited_log_value(event.harness.clone(), "harness", 128)?;
        limited_log_value(event.repository.clone(), "repository", 2048)?;
        limited_log_value(event.branch.clone(), "branch", 512)?;
        limited_log_value(event.commit_sha.clone(), "commit_sha", 128)?;
        limited_log_value(event.run_id.clone(), "run_id", 512)?;
    }

    // SQLx's checked macros require a fixed number of bind parameters. Keep
    // this value-list query dynamic because batches contain 1..=500 rows.
    let mut membership = QueryBuilder::<Postgres>::new("select count(*) from (");
    membership.push_values(&input.events, |mut row, event| {
        row.push_bind(event.organization_id)
            .push_bind(event.user_id);
    });
    membership.push(
        ") as requested(organization_id,user_id) left join users u on u.id=requested.user_id and u.organization_id=requested.organization_id where u.id is null",
    );
    let invalid: i64 = membership
        .build_query_scalar()
        .fetch_one(&state.gateway_log_pool)
        .await?;
    if invalid > 0 {
        return Err(ApiError::bad_request(
            "gateway request log user does not belong to the organization",
        ));
    }

    let retention = state.config.gateway_request_log_retention_days;
    // This is the second intentional dynamic-query exception: QueryBuilder
    // binds every value while supporting a variable-sized bulk insert.
    let mut insert = QueryBuilder::<Postgres>::new(
        "insert into gateway_request_logs (id,organization_id,user_id,profile_id,profile_name,occurred_at,method,path,model,http_status,result,upstream_latency_ms,harness,repository,branch,commit_sha,dirty,run_id,expires_at) ",
    );
    insert.push_values(&input.events, |mut row, event| {
        let occurred_at = OffsetDateTime::parse(&event.occurred_at, &Rfc3339)
            .expect("batch timestamp was validated");
        row.push_bind(event.id)
            .push_bind(event.organization_id)
            .push_bind(event.user_id)
            .push_bind(event.profile_id.as_deref().map(str::trim))
            .push_bind(event.profile_name.as_deref().map(str::trim))
            .push_bind(occurred_at)
            .push_bind(event.method.trim().to_ascii_uppercase())
            .push_bind(&event.path)
            .push_bind(event.model.as_deref().map(str::trim))
            .push_bind(event.http_status.map(i32::from))
            .push_bind(request_result(event.http_status))
            .push_bind(i64::try_from(event.upstream_latency_ms).unwrap_or(i64::MAX))
            .push_bind(event.harness.as_deref().map(str::trim))
            .push_bind(event.repository.as_deref().map(str::trim))
            .push_bind(event.branch.as_deref().map(str::trim))
            .push_bind(event.commit_sha.as_deref().map(str::trim))
            .push_bind(event.dirty)
            .push_bind(event.run_id.as_deref().map(str::trim))
            .push_bind(occurred_at + Duration::days(retention));
    });
    insert.push(" on conflict (id) do nothing");
    insert.build().execute(&state.gateway_log_pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn request_result(status: Option<u16>) -> &'static str {
    match status {
        Some(200..=299) => "success",
        Some(300..=399) => "redirect",
        Some(400..=499) => "client_error",
        Some(500..=599) => "server_error",
        _ => "transport_error",
    }
}

fn limited_log_value(
    value: Option<String>,
    field: &str,
    max: usize,
) -> Result<Option<String>, ApiError> {
    value
        .map(|value| {
            let value = value.trim().to_owned();
            if value.is_empty() {
                return Ok(None);
            }
            if value.chars().count() > max {
                return Err(ApiError::bad_request(format!(
                    "{field} must be {max} characters or fewer"
                )));
            }
            Ok(Some(value))
        })
        .transpose()
        .map(Option::flatten)
}

async fn ingest_gateway_request_log(
    State(state): State<Arc<AppState>>,
    Json(input): Json<GatewayRequestLogInput>,
) -> Result<StatusCode, ApiError> {
    let occurred_at = OffsetDateTime::parse(&input.occurred_at, &Rfc3339)
        .map_err(|_| ApiError::bad_request("occurred_at must be an RFC 3339 timestamp"))?;
    let now = OffsetDateTime::now_utc();
    if occurred_at > now + Duration::minutes(5) {
        return Err(ApiError::bad_request(
            "occurred_at cannot be more than five minutes in the future",
        ));
    }
    let method = input.method.trim().to_ascii_uppercase();
    if method.is_empty() || method.len() > 16 {
        return Err(ApiError::bad_request(
            "method must be between 1 and 16 bytes",
        ));
    }
    if !input.path.starts_with('/') || input.path.chars().count() > 2048 {
        return Err(ApiError::bad_request(
            "path must be an absolute path with at most 2048 characters",
        ));
    }
    if input
        .http_status
        .is_some_and(|status| !(100..=599).contains(&status))
    {
        return Err(ApiError::bad_request(
            "http_status must be between 100 and 599",
        ));
    }
    let user_exists: bool = sqlx::query_scalar!(
        "select exists(select 1 from users where id=$1 and organization_id=$2) as \"exists!\"",
        input.user_id,
        input.organization_id
    )
    .fetch_one(&state.gateway_log_pool)
    .await?;
    if !user_exists {
        return Err(ApiError::bad_request(
            "gateway request log user does not belong to the organization",
        ));
    }
    let upstream_latency_ms = i64::try_from(input.upstream_latency_ms)
        .map_err(|_| ApiError::bad_request("upstream_latency_ms is too large"))?;
    let expires_at = occurred_at + Duration::days(state.config.gateway_request_log_retention_days);
    sqlx::query!("insert into gateway_request_logs \
         (id,organization_id,user_id,profile_id,profile_name,occurred_at,method,path,model,http_status,result,upstream_latency_ms,harness,repository,branch,commit_sha,dirty,run_id,expires_at) \
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19) on conflict (id) do nothing",
        input.id,
        input.organization_id,
        input.user_id,
        limited_log_value(input.profile_id, "profile_id", 512)?,
        limited_log_value(input.profile_name, "profile_name", 512)?,
        occurred_at,
        method,
        input.path,
        limited_log_value(input.model, "model", 512)?,
        input.http_status.map(i32::from),
        request_result(input.http_status),
        upstream_latency_ms,
        limited_log_value(input.harness, "harness", 128)?,
        limited_log_value(input.repository, "repository", 2048)?,
        limited_log_value(input.branch, "branch", 512)?,
        limited_log_value(input.commit_sha, "commit_sha", 128)?,
        input.dirty,
        limited_log_value(input.run_id, "run_id", 512)?,
        expires_at)
    .execute(&state.gateway_log_pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Clone, Deserialize, Default)]
struct GatewayRequestLogQuery {
    page: Option<i64>,
    per_page: Option<i64>,
    q: Option<String>,
    user_id: Option<Uuid>,
    profile_id: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    result: Option<String>,
    occurred_from: Option<String>,
    occurred_to: Option<String>,
    sort: Option<String>,
}

struct GatewayRequestLogFilters {
    user_id: Option<Uuid>,
    profile_id: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    result: Option<String>,
    search: Option<String>,
    occurred_from: Option<OffsetDateTime>,
    occurred_to_exclusive: Option<OffsetDateTime>,
    sort: String,
}

fn validate_gateway_request_log_query(
    query: &GatewayRequestLogQuery,
    scoped_user_id: Option<Uuid>,
) -> Result<(i64, i64, i64, GatewayRequestLogFilters), ApiError> {
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(25);
    if page < 1 || !(1..=100).contains(&per_page) {
        return Err(ApiError::bad_request(
            "page must be at least 1 and per_page must be between 1 and 100",
        ));
    }
    let offset = (page - 1)
        .checked_mul(per_page)
        .ok_or_else(|| ApiError::bad_request("page is too large"))?;
    if query.result.as_deref().is_some_and(|value| {
        ![
            "success",
            "redirect",
            "client_error",
            "server_error",
            "transport_error",
        ]
        .contains(&value)
    }) {
        return Err(ApiError::bad_request("invalid gateway request result"));
    }
    let sort = query.sort.as_deref().unwrap_or("occurred_desc");
    if !["occurred_desc", "occurred_asc"].contains(&sort) {
        return Err(ApiError::bad_request(
            "sort must be occurred_desc or occurred_asc",
        ));
    }
    let search = like_filter("q", query.q.as_deref())?;
    let occurred_from = query
        .occurred_from
        .as_deref()
        .map(|value| parse_session_date(value, "occurred_from"))
        .transpose()?;
    let occurred_to_exclusive = query
        .occurred_to
        .as_deref()
        .map(|value| {
            parse_session_date(value, "occurred_to")?
                .checked_add(Duration::days(1))
                .ok_or_else(|| ApiError::bad_request("occurred_to is outside the supported range"))
        })
        .transpose()?;
    if occurred_from
        .zip(occurred_to_exclusive)
        .is_some_and(|(from, to)| from >= to)
    {
        return Err(ApiError::bad_request(
            "occurred_from must be on or before occurred_to",
        ));
    }
    Ok((
        page,
        per_page,
        offset,
        GatewayRequestLogFilters {
            user_id: scoped_user_id,
            profile_id: limited_log_value(query.profile_id.clone(), "profile_id", 512)?,
            model: limited_log_value(query.model.clone(), "model", 512)?,
            harness: limited_log_value(query.harness.clone(), "harness", 128)?,
            result: query.result.clone(),
            search,
            occurred_from,
            occurred_to_exclusive,
            sort: sort.to_owned(),
        },
    ))
}

#[derive(FromRow)]
struct GatewayRequestLogRow {
    id: Uuid,
    user_id: Uuid,
    user_email: String,
    profile_id: Option<String>,
    profile_name: Option<String>,
    occurred_at: OffsetDateTime,
    method: String,
    path: String,
    model: Option<String>,
    http_status: Option<i32>,
    result: String,
    upstream_latency_ms: i64,
    harness: Option<String>,
    repository: Option<String>,
    branch: Option<String>,
    commit_sha: Option<String>,
    dirty: Option<bool>,
    run_id: Option<String>,
}

fn gateway_request_log_json(row: GatewayRequestLogRow) -> serde_json::Value {
    json!({
        "id": row.id,
        "user_id": row.user_id,
        "user_email": row.user_email,
        "profile_id": row.profile_id,
        "profile_name": row.profile_name,
        "occurred_at": now_text(row.occurred_at),
        "method": row.method,
        "path": row.path,
        "model": row.model,
        "http_status": row.http_status,
        "result": row.result,
        "upstream_latency_ms": row.upstream_latency_ms,
        "harness": row.harness,
        "repository": row.repository,
        "branch": row.branch,
        "commit_sha": row.commit_sha,
        "dirty": row.dirty,
        "run_id": row.run_id,
    })
}

async fn gateway_request_logs(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Query(query): Query<GatewayRequestLogQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (page, per_page, offset, filters) =
        validate_gateway_request_log_query(&query, query.user_id)?;
    let total: i64 = sqlx::query_scalar!("select count(*) as \"count!\" from gateway_request_logs l where l.organization_id=$1 and l.expires_at>now() \
         and ($2::uuid is null or l.user_id=$2) and ($3::text is null or l.profile_id=$3) \
         and ($4::text is null or l.model=$4) and ($5::text is null or l.harness=$5) \
         and ($6::text is null or l.result=$6) \
         and ($7::text is null or lower(l.path) like $7 escape E'\\\\' or lower(coalesce(l.model,'')) like $7 escape E'\\\\' \
              or lower(coalesce(l.repository,'')) like $7 escape E'\\\\' or lower(coalesce(l.branch,'')) like $7 escape E'\\\\' \
              or lower(coalesce(l.run_id,'')) like $7 escape E'\\\\' or lower(coalesce(l.profile_name,'')) like $7 escape E'\\\\') \
         and ($8::timestamptz is null or l.occurred_at >= $8) and ($9::timestamptz is null or l.occurred_at < $9)",
        who.organization_id,
        filters.user_id,
        filters.profile_id.clone(),
        filters.model.clone(),
        filters.harness.clone(),
        filters.result.clone(),
        filters.search.clone(),
        filters.occurred_from,
        filters.occurred_to_exclusive)
    .fetch_one(&state.pool)
    .await?;
    let rows = sqlx::query_as!(GatewayRequestLogRow,
        "select l.id,l.user_id,u.email as user_email,l.profile_id,l.profile_name,l.occurred_at,l.method,l.path,l.model,l.http_status,l.result,l.upstream_latency_ms, \
         l.harness,l.repository,l.branch,l.commit_sha,l.dirty,l.run_id from gateway_request_logs l join users u on u.id=l.user_id \
         where l.organization_id=$1 and l.expires_at>now() and ($2::uuid is null or l.user_id=$2) \
         and ($3::text is null or l.profile_id=$3) and ($4::text is null or l.model=$4) and ($5::text is null or l.harness=$5) \
         and ($6::text is null or l.result=$6) \
         and ($7::text is null or lower(l.path) like $7 escape E'\\\\' or lower(coalesce(l.model,'')) like $7 escape E'\\\\' \
              or lower(coalesce(l.repository,'')) like $7 escape E'\\\\' or lower(coalesce(l.branch,'')) like $7 escape E'\\\\' \
              or lower(coalesce(l.run_id,'')) like $7 escape E'\\\\' or lower(coalesce(l.profile_name,'')) like $7 escape E'\\\\') \
         and ($8::timestamptz is null or l.occurred_at >= $8) and ($9::timestamptz is null or l.occurred_at < $9) \
         order by case when $10='occurred_asc' then l.occurred_at end asc, case when $10='occurred_asc' then l.id end asc, \
                  case when $10='occurred_desc' then l.occurred_at end desc, case when $10='occurred_desc' then l.id end desc \
         offset $11 limit $12",
        who.organization_id,
        filters.user_id,
        filters.profile_id,
        filters.model,
        filters.harness,
        filters.result,
        filters.search,
        filters.occurred_from,
        filters.occurred_to_exclusive,
        filters.sort,
        offset,
        per_page)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.into_iter().map(gateway_request_log_json).collect::<Vec<_>>(),
        "page": page,
        "per_page": per_page,
        "total": total,
        "total_pages": page_count(total, per_page),
    })))
}

#[derive(FromRow)]
struct GatewayRequestProfileFacet {
    id: String,
    name: Option<String>,
}

async fn gateway_request_log_facets(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Query(query): Query<FacetUserQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_search = like_filter("user_q", query.user_q.as_deref())?;
    let users = sqlx::query_as!(SessionFacetUser,
        "select u.id as \"id!\",u.email as \"email!\" from gateway_request_logs l join users u on u.id=l.user_id \
         where l.organization_id=$1 and l.expires_at>now() and ($2::uuid is null or l.user_id=$2) \
         and ($3::text is null or lower(u.email) like $3 escape E'\\\\') \
         group by u.id,u.email order by (u.id=$4) desc,u.email limit 10",
        who.organization_id,
        None::<Uuid>,
        user_search.as_deref(),
        query.selected_user_id)
    .fetch_all(&state.pool)
    .await?;
    let profiles = sqlx::query_as!(GatewayRequestProfileFacet,
        "select l.profile_id as \"id!\",max(l.profile_name) as name from gateway_request_logs l \
         where l.organization_id=$1 and l.expires_at>now() and ($2::uuid is null or l.user_id=$2) and l.profile_id is not null \
         group by l.profile_id order by coalesce(max(l.profile_name),l.profile_id)",
        who.organization_id,
        None::<Uuid>)
    .fetch_all(&state.pool)
    .await?;
    let models: Vec<String> = sqlx::query_scalar!("select distinct model as \"model!\" from gateway_request_logs where organization_id=$1 and expires_at>now() \
         and ($2::uuid is null or user_id=$2) and model is not null order by model",
        who.organization_id,
        None::<Uuid>)
    .fetch_all(&state.pool)
    .await?;
    let harnesses: Vec<String> = sqlx::query_scalar!("select distinct harness as \"harness!\" from gateway_request_logs where organization_id=$1 and expires_at>now() \
         and ($2::uuid is null or user_id=$2) and harness is not null order by harness",
        who.organization_id,
        None::<Uuid>)
    .fetch_all(&state.pool)
    .await?;
    let results: Vec<String> = sqlx::query_scalar!("select distinct result from gateway_request_logs where organization_id=$1 and expires_at>now() \
         and ($2::uuid is null or user_id=$2) order by result",
        who.organization_id,
        None::<Uuid>)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "users": users.into_iter().map(|user| json!({"id":user.id,"email":user.email})).collect::<Vec<_>>(),
        "profiles": profiles.into_iter().map(|profile| json!({"id":profile.id,"name":profile.name})).collect::<Vec<_>>(),
        "models": models,
        "harnesses": harnesses,
        "results": results,
    })))
}

async fn cleanup_gateway_request_logs(pool: PgPool) {
    loop {
        match sqlx::query!("delete from gateway_request_logs where expires_at <= now()")
            .execute(&pool)
            .await
        {
            Ok(result) if result.rows_affected() > 0 => {
                record_worker_success();
                tracing::info!(
                    deleted = result.rows_affected(),
                    "expired gateway request logs removed"
                );
            }
            Ok(_) => record_worker_success(),
            Err(error) => tracing::warn!(%error, "failed to remove expired gateway request logs"),
        }
        if let Err(error) = sqlx::query!("delete from public.gateway_cache_events where created_at < now() - interval '24 hours'",)
        .execute(&pool)
        .await
        {
            tracing::warn!(%error, "failed to remove expired gateway cache events");
        }
        if let Err(error) = sqlx::query!("delete from public.gateway_auth_sessions where source_expires_at < now() - interval '24 hours' or revoked_at < now() - interval '24 hours'",)
        .execute(&pool)
        .await
        {
            tracing::warn!(%error, "failed to remove expired gateway auth sessions");
        }
        tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
    }
}

async fn personalize_gateway_config(
    state: &AppState,
    who: &Principal,
    config: &mut gh_service::GovernanceConfig,
) -> Result<(), ApiError> {
    if config.gateway.is_none() {
        return Ok(());
    }
    let Some((proxy_url, _)) = managed_gateway_settings(&state.config)? else {
        return Err(ApiError::internal(
            "gateway policy is active but gateway runtime settings are not configured",
        ));
    };
    // Personalized config must report invalid state without silently creating
    // another upstream credential. Interactive clients and the dashboard use
    // the manual ensure endpoint to authorize the single replacement attempt.
    ensure_managed_gateway_key(state, who, false, false, false).await?;
    let selection = managed_gateway_row(&state.pool, who.user_id)
        .await?
        .ok_or_else(|| {
            ApiError::conflict("gateway key provisioning is required; run `blue gateway`")
        })?;
    if selection.credential_state != "ready" || selection.credential_ciphertext.is_none() {
        return Err(ApiError::conflict(
            "gateway key provisioning is required; run `blue gateway`",
        ));
    }
    let token = gateway_auth::mint_gateway_inference_token(state, who).await?;
    if let Some(gateway) = config.gateway.as_mut() {
        gateway.proxy_url = Some(proxy_url.to_owned());
        gateway.token = Some(token);
    }
    Ok(())
}

async fn revoke_current_gateway_session(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<StatusCode, ApiError> {
    let oauth_session_id = who
        .oauth_session_id
        .as_deref()
        .ok_or_else(ApiError::unauthorized)?;
    sqlx::query!(
        "update public.gateway_auth_sessions set revoked_at=coalesce(revoked_at,now()),updated_at=now() where oauth_session_id=$1 and user_id=$2",
        oauth_session_id,
        who.user_id
    )
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn governance_config(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    headers: HeaderMap,
) -> Result<Json<gh_service::GovernanceConfig>, ApiError> {
    let row = current_config(&state.pool, who.organization_id).await?;
    let revision = row.revision;
    let mut config = serde_json::from_value(row.document)
        .map_err(|error| ApiError::internal(format!("decoding stored config: {error}")))?;
    enforce_client_capabilities(&headers, &config)?;
    personalize_package_config(&state.pool, &revision, who.user_id, &mut config).await?;
    personalize_gateway_config(&state, &who, &mut config).await?;
    Ok(Json(config))
}

async fn personalize_package_config(
    pool: &PgPool,
    revision: &str,
    user_id: Uuid,
    config: &mut gh_service::GovernanceConfig,
) -> Result<(), ApiError> {
    let audiences = package_audiences_for_revision(pool, revision, &config.packages).await?;
    apply_package_audiences(user_id, config, &audiences);
    Ok(())
}

fn apply_package_audiences(
    user_id: Uuid,
    config: &mut gh_service::GovernanceConfig,
    audiences: &BTreeMap<String, PackageAudience>,
) {
    let hidden = config
        .packages
        .iter()
        .filter(|package| {
            audiences.get(&package.id).is_some_and(|audience| {
                audience.scope == PackageAudienceScope::Users
                    && !audience.user_ids.contains(&user_id)
            })
        })
        .map(|package| package.id.clone())
        .collect::<BTreeSet<_>>();
    if hidden.is_empty() {
        return;
    }
    config
        .packages
        .retain(|package| !hidden.contains(&package.id));
    for policy in config.harnesses.values_mut() {
        policy
            .package_overrides
            .retain(|package_id, _| !hidden.contains(package_id));
    }
}

fn enforce_client_capabilities(
    headers: &HeaderMap,
    config: &gh_service::GovernanceConfig,
) -> Result<(), ApiError> {
    let contract = headers
        .get("x-blue-contract-version")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(1);
    if contract < config.contract_version {
        return Err(ApiError::new(
            StatusCode::UPGRADE_REQUIRED,
            format!(
                "governance contract {} requires a newer client; client reports {contract}",
                config.contract_version
            ),
        ));
    }
    let supported = headers
        .get("x-blue-capabilities")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>();
    let missing = config
        .required_capabilities
        .iter()
        .filter(|capability| !supported.contains(capability.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(ApiError::new(
            StatusCode::UPGRADE_REQUIRED,
            format!(
                "governance requires unsupported client capabilities: {}",
                missing.join(", ")
            ),
        ));
    }
    Ok(())
}

async fn governance_config_events(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    // Subscribe before reading the current row so a concurrently committed
    // revision is either the initial value or queued for the stream.
    let receiver = state.revision_events.subscribe();
    let row = current_config(&state.pool, who.organization_id).await?;
    let initial = RevisionEvent {
        organization_id: who.organization_id,
        revision: row.revision,
    };
    let remaining = who
        .expires_at
        .unix_timestamp()
        .saturating_sub(OffsetDateTime::now_utc().unix_timestamp())
        .max(1) as u64;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(remaining);
    let organization_id = who.organization_id;

    let stream = futures_util::stream::unfold(
        (Some(initial), receiver, deadline),
        move |(mut initial, mut receiver, deadline)| async move {
            if let Some(event) = initial.take() {
                let item = revision_sse_event(&event);
                return Some((Ok(item), (initial, receiver, deadline)));
            }
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return None,
                    received = receiver.recv() => match received {
                        Ok(RevisionSignal::Revision(event)) if event.organization_id == organization_id => {
                            let item = revision_sse_event(&event);
                            return Some((Ok(item), (initial, receiver, deadline)));
                        }
                        Ok(RevisionSignal::Revision(_)) => continue,
                        Ok(RevisionSignal::Resync)
                        | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return None,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    }
                }
            }
        },
    );

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keepalive"),
    ))
}

fn revision_sse_event(event: &RevisionEvent) -> Event {
    Event::default()
        .event("revision")
        .id(event.revision.clone())
        .data(json!({ "revision": event.revision }).to_string())
}

#[derive(Deserialize)]
struct ClientStatusRequest {
    instance_id: String,
    hostname: Option<String>,
    client_version: String,
    platform: String,
    #[serde(default)]
    architecture: Option<String>,
    config_revision: Option<String>,
    applied: bool,
    files_ok: bool,
    #[serde(default)]
    harnesses: serde_json::Value,
    #[serde(default)]
    packages: serde_json::Value,
    #[serde(default)]
    reconciliation_attempt: Option<ReconciliationAttempt>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ReconciliationAttempt {
    harness: String,
    revision: String,
}

struct RevisionHealth {
    matches: Option<bool>,
    reasons: Vec<String>,
}

fn reported_version(entry: &serde_json::Value) -> (Option<semver::Version>, Option<&str>) {
    let raw = entry.get("raw_version").and_then(serde_json::Value::as_str);
    let normalized = entry.get("version").and_then(serde_json::Value::as_str);
    let version = normalized
        .and_then(|value| semver::Version::parse(value).ok())
        .or_else(|| {
            raw.and_then(|value| {
                semver::Version::parse(value).ok().or_else(|| {
                    value
                        .split_whitespace()
                        .map(|token| token.trim_start_matches('v'))
                        .find_map(|token| semver::Version::parse(token).ok())
                })
            })
        });
    (version, raw.or(normalized))
}

fn expected_package_digests(
    package: &gh_service::ManagedPackage,
    platform: &str,
    architecture: Option<&str>,
) -> BTreeSet<String> {
    if package.platform_sources.is_empty() {
        return BTreeSet::from([package.sha256.to_ascii_lowercase()]);
    }
    if let Some(architecture) = architecture {
        return package
            .platform_sources
            .get(&format!("{platform}-{architecture}"))
            .map(|source| BTreeSet::from([source.sha256.to_ascii_lowercase()]))
            .unwrap_or_default();
    }
    let prefix = format!("{platform}-");
    package
        .platform_sources
        .iter()
        .filter(|(target, _)| target.starts_with(&prefix))
        .map(|(_, source)| source.sha256.to_ascii_lowercase())
        .collect()
}

fn reconciliation_scope(
    input: &ClientStatusRequest,
    revision: &str,
    allowed: &BTreeSet<&str>,
) -> BTreeSet<String> {
    let applied_report = input.config_revision.as_deref() == Some(revision);
    let mut scope = input
        .harnesses
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let name = entry
                .as_str()
                .or_else(|| entry.get("name").and_then(serde_json::Value::as_str))?;
            let reconciled = entry.as_str().is_some()
                || entry
                    .get("reconciled")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
            (applied_report && reconciled && allowed.contains(name)).then(|| name.to_owned())
        })
        .collect::<BTreeSet<_>>();
    if let Some(attempt) = input.reconciliation_attempt.as_ref() {
        if attempt.revision == revision && allowed.contains(attempt.harness.as_str()) {
            scope.insert(attempt.harness.clone());
        }
    }
    scope
}

async fn classify_client_revision(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
    input: &ClientStatusRequest,
) -> Result<RevisionHealth, ApiError> {
    let revision = input
        .reconciliation_attempt
        .as_ref()
        .map(|attempt| attempt.revision.as_str())
        .or(input.config_revision.as_deref());
    let Some(revision) = revision else {
        return Ok(RevisionHealth {
            matches: None,
            reasons: Vec::new(),
        });
    };
    let document: Option<serde_json::Value> = sqlx::query_scalar!(
        "SELECT document FROM governance_config_revisions WHERE organization_id=$1 AND revision=$2",
        organization_id,
        revision
    )
    .fetch_optional(pool)
    .await?;
    let Some(document) = document else {
        return Ok(RevisionHealth {
            matches: None,
            reasons: Vec::new(),
        });
    };
    let mut config: gh_service::GovernanceConfig = serde_json::from_value(document)
        .map_err(|error| ApiError::internal(format!("decoding stored config: {error}")))?;
    personalize_package_config(pool, revision, user_id, &mut config).await?;

    let mut reasons = Vec::new();
    let applied_for_revision = input.applied && input.config_revision.as_deref() == Some(revision);
    if !applied_for_revision {
        reasons.push("Configuration has not been applied for this revision.".to_owned());
    }
    if !input.files_ok {
        reasons.push("Managed configuration is missing or has drifted.".to_owned());
    }
    if let Some(error) = input
        .error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty() && *error != "configuration is not current")
    {
        reasons.push(error.to_owned());
    }

    let allowed = config
        .allowed_harnesses
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let reconciled = reconciliation_scope(input, revision, &allowed);
    if let Some(entries) = input.harnesses.as_array() {
        for entry in entries {
            if entry.as_str().is_some() {
                continue;
            }
            let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !reconciled.contains(name) {
                continue;
            }
            if !entry
                .get("client_supported")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true)
            {
                reasons.push(format!(
                    "{name}: this client does not support the governed harness."
                ));
                continue;
            }
            let Ok(harness) = name.parse::<gh_common::Harness>() else {
                reasons.push(format!(
                    "{name}: this client does not support the governed harness."
                ));
                continue;
            };
            let (version, raw_version) = reported_version(entry);
            let default_policy = gh_service::HarnessPolicy::default();
            let policy = config.policy(name).unwrap_or(&default_policy);
            if let Err(error) =
                gh_config::resolve_compatibility(harness, version.as_ref(), raw_version, policy)
            {
                reasons.push(format!("{name}: {error}"));
            }
        }
    }

    #[derive(Clone)]
    struct ExpectedPackage {
        version: String,
        digests: BTreeSet<String>,
    }
    let mut expected = BTreeMap::<(String, String), ExpectedPackage>::new();
    for harness in &reconciled {
        let policy = config.policy(harness).cloned().unwrap_or_default();
        for package in &config.packages {
            let enabled = policy
                .package_overrides
                .get(&package.id)
                .and_then(|item| item.enabled)
                .unwrap_or(true);
            if enabled && package.adapters.contains_key(harness) {
                let digests = expected_package_digests(
                    package,
                    &input.platform,
                    input.architecture.as_deref(),
                );
                if digests.is_empty() {
                    reasons.push(format!(
                        "{} ({harness}): no package archive matches {}{}.",
                        package.id,
                        input.platform,
                        input
                            .architecture
                            .as_deref()
                            .map(|value| format!("-{value}"))
                            .unwrap_or_default()
                    ));
                }
                expected.insert(
                    (package.id.clone(), harness.clone()),
                    ExpectedPackage {
                        version: package.version.clone(),
                        digests,
                    },
                );
            }
        }
    }

    let mut reported = BTreeMap::<(String, String), &serde_json::Value>::new();
    if let Some(packages) = input.packages.as_array() {
        for package in packages {
            let Some(id) = package.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(harness) = package.get("harness").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !reconciled.contains(harness) {
                continue;
            }
            let key = (id.to_owned(), harness.to_owned());
            if reported.insert(key, package).is_some() {
                reasons.push(format!(
                    "{id} ({harness}): package was reported more than once."
                ));
            }
        }
    }
    for ((id, harness), wanted) in &expected {
        let Some(package) = reported.remove(&(id.clone(), harness.clone())) else {
            reasons.push(format!("{id} ({harness}): expected package is missing."));
            continue;
        };
        let state = package
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        if state != "applied" {
            let detail = package
                .get("error")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| format!(": {value}"))
                .unwrap_or_default();
            reasons.push(format!("{id} ({harness}): package is {state}{detail}."));
        }
        let version = package
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if version != wanted.version {
            reasons.push(format!(
                "{id} ({harness}): package version {version} does not match expected {}.",
                wanted.version
            ));
        }
        let digest = package
            .get("sha256")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if !wanted.digests.is_empty() && !wanted.digests.contains(&digest) {
            reasons.push(format!(
                "{id} ({harness}): package digest does not match the revision."
            ));
        }
    }
    for ((id, harness), package) in reported {
        let state = package
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        reasons.push(format!(
            "{id} ({harness}): unexpected package is still reported as {state}."
        ));
    }
    reasons.sort();
    reasons.dedup();
    Ok(RevisionHealth {
        matches: Some(reasons.is_empty()),
        reasons,
    })
}

async fn report_client_status(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Json(input): Json<ClientStatusRequest>,
) -> Result<StatusCode, ApiError> {
    if input.instance_id.trim().is_empty() || input.client_version.trim().is_empty() {
        return Err(ApiError::bad_request(
            "instance_id and client_version are required",
        ));
    }
    if let Some(attempt) = input.reconciliation_attempt.as_ref() {
        if attempt.harness.trim().is_empty()
            || attempt.revision.trim().is_empty()
            || input
                .error
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            return Err(ApiError::bad_request(
                "reconciliation_attempt requires harness, revision, and a non-empty error",
            ));
        }
    }
    let health =
        classify_client_revision(&state.pool, who.organization_id, who.user_id, &input).await?;
    sqlx::query!("insert into public.client_status (id,organization_id,user_id,instance_id,hostname,client_version,platform,architecture,config_revision,applied,files_ok,harnesses,packages,error,revision_matches,status_reasons) \
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) \
         on conflict (user_id,instance_id) do update set hostname=excluded.hostname,client_version=excluded.client_version, \
         platform=excluded.platform,architecture=excluded.architecture,config_revision=excluded.config_revision, \
         applied=excluded.applied,files_ok=excluded.files_ok,harnesses=excluded.harnesses,packages=excluded.packages, \
         error=excluded.error,revision_matches=excluded.revision_matches,status_reasons=excluded.status_reasons,last_seen_at=now()",
        Uuid::new_v4(),
        who.organization_id,
        who.user_id,
        input.instance_id,
        input.hostname,
        input.client_version,
        input.platform,
        input.architecture,
        input.config_revision,
        input.applied,
        input.files_ok,
        input.harnesses,
        input.packages,
        input.error,
        health.matches,
        json!(health.reasons))
        .execute(&state.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(FromRow)]
struct ClientStatusRow {
    id: Uuid,
    instance_id: String,
    hostname: Option<String>,
    client_version: String,
    platform: String,
    architecture: Option<String>,
    config_revision: Option<String>,
    applied: bool,
    files_ok: bool,
    harnesses: serde_json::Value,
    packages: serde_json::Value,
    error: Option<String>,
    revision_matches: Option<bool>,
    status_reasons: serde_json::Value,
    first_seen_at: OffsetDateTime,
    last_seen_at: OffsetDateTime,
    user_email: String,
}

fn client_health_status(
    config_revision: Option<&str>,
    revision_matches: Option<bool>,
    current_revision: Option<&str>,
) -> &'static str {
    if revision_matches == Some(false) {
        "attention"
    } else if config_revision != current_revision {
        "outdated"
    } else if revision_matches == Some(true) {
        "current"
    } else {
        "outdated"
    }
}

fn client_status_json(row: ClientStatusRow, current_revision: Option<&str>) -> serde_json::Value {
    let status = client_health_status(
        row.config_revision.as_deref(),
        row.revision_matches,
        current_revision,
    );
    let activity_status = client_activity_status(row.last_seen_at, OffsetDateTime::now_utc());
    json!({
        "id": row.id, "instance_id": row.instance_id, "hostname": row.hostname, "client_version": row.client_version,
        "platform": row.platform, "architecture": row.architecture, "config_revision": row.config_revision, "applied": row.applied,
        "files_ok": row.files_ok, "harnesses": row.harnesses, "packages": row.packages, "error": row.error,
        "status": status, "status_reasons": row.status_reasons,
        "first_seen_at": now_text(row.first_seen_at), "last_seen_at": now_text(row.last_seen_at),
        "activity_status": activity_status, "user_email": row.user_email,
    })
}

fn client_activity_status(last_seen_at: OffsetDateTime, now: OffsetDateTime) -> &'static str {
    if last_seen_at <= now - Duration::days(30) {
        "stale"
    } else {
        "recent"
    }
}

#[derive(Clone, Deserialize, Default)]
struct ClientStatusListQuery {
    page: Option<i64>,
    per_page: Option<i64>,
    q: Option<String>,
    user_id: Option<Uuid>,
    harness: Option<String>,
    health: Option<String>,
    last_seen_from: Option<String>,
    last_seen_to: Option<String>,
    sort: Option<String>,
}

struct ClientStatusPageFilters {
    user_id: Option<Uuid>,
    harness: Option<String>,
    health: Option<String>,
    search: Option<String>,
    last_seen_from: Option<OffsetDateTime>,
    last_seen_to_exclusive: Option<OffsetDateTime>,
    sort: String,
}

fn validate_client_status_page_query(
    query: &ClientStatusListQuery,
) -> Result<(i64, i64, i64, ClientStatusPageFilters), ApiError> {
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(25);
    if page < 1 {
        return Err(ApiError::bad_request("page must be at least 1"));
    }
    if !(1..=100).contains(&per_page) {
        return Err(ApiError::bad_request("per_page must be between 1 and 100"));
    }
    let offset = (page - 1)
        .checked_mul(per_page)
        .ok_or_else(|| ApiError::bad_request("page is too large"))?;
    if query
        .health
        .as_deref()
        .is_some_and(|value| !["current", "outdated", "attention"].contains(&value))
    {
        return Err(ApiError::bad_request(
            "health must be current, outdated, or attention",
        ));
    }
    let sort = query.sort.as_deref().unwrap_or("last_seen_desc");
    if !["last_seen_desc", "last_seen_asc"].contains(&sort) {
        return Err(ApiError::bad_request(
            "sort must be last_seen_desc or last_seen_asc",
        ));
    }
    let search = like_filter("q", query.q.as_deref())?;
    let last_seen_from = query
        .last_seen_from
        .as_deref()
        .map(|value| parse_session_date(value, "last_seen_from"))
        .transpose()?;
    let last_seen_to_exclusive = query
        .last_seen_to
        .as_deref()
        .map(|value| {
            parse_session_date(value, "last_seen_to")?
                .checked_add(Duration::days(1))
                .ok_or_else(|| ApiError::bad_request("last_seen_to is outside the supported range"))
        })
        .transpose()?;
    if last_seen_from
        .zip(last_seen_to_exclusive)
        .is_some_and(|(from, to)| from >= to)
    {
        return Err(ApiError::bad_request(
            "last_seen_from must be on or before last_seen_to",
        ));
    }
    Ok((
        page,
        per_page,
        offset,
        ClientStatusPageFilters {
            user_id: query.user_id,
            harness: query.harness.clone(),
            health: query.health.clone(),
            search,
            last_seen_from,
            last_seen_to_exclusive,
            sort: sort.to_owned(),
        },
    ))
}

async fn list_client_status(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Query(query): Query<ClientStatusListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (page, per_page, offset, filters) = validate_client_status_page_query(&query)?;
    let current_revision: Option<String> = sqlx::query_scalar!("SELECT revision FROM governance_config_revisions WHERE organization_id=$1 ORDER BY id DESC LIMIT 1",
        who.organization_id)
    .fetch_optional(&state.pool)
    .await?;
    let total: i64 = sqlx::query_scalar!("SELECT count(*) as \"count!\" FROM public.client_status c JOIN users u ON u.id=c.user_id \
         WHERE c.organization_id=$1 AND ($2::uuid IS NULL OR c.user_id=$2) \
         AND ($3::text IS NULL OR EXISTS (SELECT 1 FROM jsonb_array_elements( \
              CASE WHEN jsonb_typeof(c.harnesses)='array' THEN c.harnesses ELSE '[]'::jsonb END) entry \
              WHERE CASE WHEN jsonb_typeof(entry)='string' THEN entry #>> '{}' ELSE entry->>'name' END=$3)) \
         AND ($4::text IS NULL OR ($4='current' AND c.config_revision=$8 AND c.revision_matches IS TRUE) \
              OR ($4='outdated' AND c.config_revision IS DISTINCT FROM $8 AND c.revision_matches IS DISTINCT FROM FALSE) \
              OR ($4='attention' AND c.revision_matches IS FALSE)) \
         AND ($5::text IS NULL OR lower(c.instance_id) LIKE $5 ESCAPE E'\\\\' \
              OR lower(coalesce(c.hostname,'')) LIKE $5 ESCAPE E'\\\\' \
              OR lower(u.email) LIKE $5 ESCAPE E'\\\\') \
         AND ($6::timestamptz IS NULL OR c.last_seen_at >= $6) \
         AND ($7::timestamptz IS NULL OR c.last_seen_at < $7)",
        who.organization_id,
        filters.user_id,
        filters.harness.clone(),
        filters.health.clone(),
        filters.search.clone(),
        filters.last_seen_from,
        filters.last_seen_to_exclusive,
        current_revision.clone())
    .fetch_one(&state.pool)
    .await?;
    let rows = sqlx::query_as!(ClientStatusRow,
        "SELECT c.id,c.instance_id,c.hostname,c.client_version,c.platform,c.architecture,c.config_revision,c.applied,c.files_ok, \
         c.harnesses,c.packages,c.error,c.revision_matches,c.status_reasons,c.first_seen_at,c.last_seen_at,u.email AS user_email \
         FROM public.client_status c JOIN users u ON u.id=c.user_id \
         WHERE c.organization_id=$1 AND ($2::uuid IS NULL OR c.user_id=$2) \
         AND ($3::text IS NULL OR EXISTS (SELECT 1 FROM jsonb_array_elements( \
              CASE WHEN jsonb_typeof(c.harnesses)='array' THEN c.harnesses ELSE '[]'::jsonb END) entry \
              WHERE CASE WHEN jsonb_typeof(entry)='string' THEN entry #>> '{}' ELSE entry->>'name' END=$3)) \
         AND ($4::text IS NULL OR ($4='current' AND c.config_revision=$8 AND c.revision_matches IS TRUE) \
              OR ($4='outdated' AND c.config_revision IS DISTINCT FROM $8 AND c.revision_matches IS DISTINCT FROM FALSE) \
              OR ($4='attention' AND c.revision_matches IS FALSE)) \
         AND ($5::text IS NULL OR lower(c.instance_id) LIKE $5 ESCAPE E'\\\\' \
              OR lower(coalesce(c.hostname,'')) LIKE $5 ESCAPE E'\\\\' \
              OR lower(u.email) LIKE $5 ESCAPE E'\\\\') \
         AND ($6::timestamptz IS NULL OR c.last_seen_at >= $6) \
         AND ($7::timestamptz IS NULL OR c.last_seen_at < $7) \
         ORDER BY CASE WHEN $9='last_seen_asc' THEN c.last_seen_at END ASC, \
                  CASE WHEN $9='last_seen_asc' THEN c.id END ASC, \
                  CASE WHEN $9='last_seen_desc' THEN c.last_seen_at END DESC, \
                  CASE WHEN $9='last_seen_desc' THEN c.id END DESC \
         OFFSET $10 LIMIT $11",
        who.organization_id,
        filters.user_id,
        filters.harness,
        filters.health,
        filters.search,
        filters.last_seen_from,
        filters.last_seen_to_exclusive,
        current_revision.clone(),
        filters.sort,
        offset,
        per_page)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.into_iter().map(|row| client_status_json(row, current_revision.as_deref())).collect::<Vec<_>>(),
        "page": page,
        "per_page": per_page,
        "total": total,
        "total_pages": page_count(total, per_page),
        "current_revision": current_revision,
    })))
}

async fn delete_client_status(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let result = sqlx::query!(
        "DELETE FROM public.client_status WHERE id=$1 AND organization_id=$2",
        id,
        who.organization_id
    )
    .execute(&state.pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("client status not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(FromRow)]
struct ClientStatusFacetUser {
    id: Uuid,
    email: String,
}

async fn client_status_facets(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Query(query): Query<FacetUserQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_search = like_filter("user_q", query.user_q.as_deref())?;
    let users = sqlx::query_as!(ClientStatusFacetUser,
        "SELECT u.id as \"id!\",u.email as \"email!\" FROM public.client_status c JOIN users u ON u.id=c.user_id \
         WHERE c.organization_id=$1 AND ($2::text IS NULL OR lower(u.email) LIKE $2 ESCAPE E'\\\\') \
         GROUP BY u.id,u.email ORDER BY (u.id=$3) DESC,u.email LIMIT 10",
        who.organization_id,
        user_search.as_deref(),
        query.selected_user_id)
        .fetch_all(&state.pool)
        .await?;
    let harnesses: Vec<String> = sqlx::query_scalar!("SELECT DISTINCT harness as \"harness!\" FROM ( \
           SELECT CASE WHEN jsonb_typeof(entry)='string' THEN entry #>> '{}' ELSE entry->>'name' END AS harness \
           FROM public.client_status c CROSS JOIN LATERAL jsonb_array_elements( \
             CASE WHEN jsonb_typeof(c.harnesses)='array' THEN c.harnesses ELSE '[]'::jsonb END) entry \
           WHERE c.organization_id=$1 \
         ) available WHERE harness IS NOT NULL AND harness <> '' ORDER BY harness",
        who.organization_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "users": users.into_iter().map(|user| json!({"id":user.id,"email":user.email})).collect::<Vec<_>>(),
        "harnesses": harnesses,
    })))
}

#[derive(Serialize)]
struct AdminConfigResponse {
    revision: String,
    yaml: String,
    managed_yaml: String,
    document: gh_service::GovernanceConfig,
    package_audiences: BTreeMap<String, PackageAudience>,
    created_at: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PackageAudienceScope {
    Organization,
    Users,
}

impl PackageAudienceScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Organization => "organization",
            Self::Users => "users",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct PackageAudience {
    scope: PackageAudienceScope,
    #[serde(default)]
    user_ids: Vec<Uuid>,
}

impl PackageAudience {
    fn organization() -> Self {
        Self {
            scope: PackageAudienceScope::Organization,
            user_ids: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct BlueConfigRedaction {
    path: String,
    environment_variable: String,
}

#[derive(Serialize)]
struct BlueConfigExportResponse {
    yaml: String,
    redactions: Vec<BlueConfigRedaction>,
}

fn yaml_key(value: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(value.to_owned())
}

fn redact_literal_secret(
    document: &mut serde_yaml::Value,
    path: &[&str],
    environment_variable: String,
    redactions: &mut Vec<BlueConfigRedaction>,
) {
    let mut current = document;
    for segment in &path[..path.len().saturating_sub(1)] {
        let Some(next) = current.get_mut(yaml_key(segment)) else {
            return;
        };
        current = next;
    }
    let Some(mapping) = current.as_mapping_mut() else {
        return;
    };
    let Some(value) = mapping.get_mut(yaml_key(path[path.len() - 1])) else {
        return;
    };
    let Some(raw) = value.as_str() else { return };
    if raw.starts_with("os.environ/") || raw.starts_with("env://") || raw.starts_with("file://") {
        return;
    }
    *value = serde_yaml::Value::String(format!("os.environ/{environment_variable}"));
    redactions.push(BlueConfigRedaction {
        path: path.join("."),
        environment_variable,
    });
}

fn connection_environment_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(FromRow)]
struct ArtifactProvenance {
    id: Uuid,
    connection_id: String,
    repository: String,
    requested_ref: String,
    resolved_commit: String,
}

async fn export_governance_value(
    pool: &PgPool,
    org_id: Uuid,
    config: &gh_service::GovernanceConfig,
) -> Result<serde_yaml::Value, ApiError> {
    let mut value = serde_yaml::to_value(config)
        .map_err(|error| ApiError::internal(format!("serializing governance export: {error}")))?;
    let artifacts = sqlx::query_as!(ArtifactProvenance,
        "SELECT id,connection_id,repository,requested_ref,resolved_commit FROM package_artifacts WHERE organization_id=$1",
        org_id).fetch_all(pool).await?
        .into_iter().map(|item| (item.id.to_string(), item)).collect::<BTreeMap<_, _>>();
    let Some(packages) = value
        .get_mut(yaml_key("packages"))
        .and_then(serde_yaml::Value::as_sequence_mut)
    else {
        return Ok(value);
    };
    for package in packages {
        let Some(mapping) = package.as_mapping_mut() else {
            continue;
        };
        let Some(artifact_id) = mapping
            .get(yaml_key("artifact_id"))
            .and_then(serde_yaml::Value::as_str)
        else {
            continue;
        };
        let Some(artifact) = artifacts.get(artifact_id) else {
            continue;
        };
        mapping.insert(
            yaml_key("managed_source"),
            serde_yaml::to_value(json!({
                "connection_id": artifact.connection_id,
                "repository": artifact.repository,
                "requested_ref": artifact.requested_ref,
                "resolved_commit": artifact.resolved_commit,
            }))
            .map_err(|error| {
                ApiError::internal(format!("serializing managed package source: {error}"))
            })?,
        );
    }
    Ok(value)
}

async fn export_blue_config(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<BlueConfigExportResponse>, ApiError> {
    let row = current_config(&state.pool, who.organization_id).await?;
    let config: gh_service::GovernanceConfig =
        serde_json::from_value(row.document).map_err(|error| {
            ApiError::internal(format!("decoding stored governance config: {error}"))
        })?;
    let mut governance = export_governance_value(&state.pool, who.organization_id, &config).await?;
    if let Some(governance) = governance.as_mapping_mut() {
        governance.remove(yaml_key("gateway"));
    }
    let mut document = read_blue_config_with_overlays(
        &state.config.blue_config_file,
        &state.config.blue_config_overlay_files,
    )?;
    let root = document
        .as_mapping_mut()
        .ok_or_else(|| ApiError::internal("Blue config must be a YAML mapping"))?;
    root.insert(yaml_key("governance"), governance);

    let mut redactions = Vec::new();
    for (path, variable) in [
        (&["control_api", "database_url"][..], "HARNESS_DATABASE_URL"),
        (
            &["control_api", "identity", "scim_bearer_token"][..],
            "HARNESS_SCIM_BEARER_TOKEN",
        ),
        (
            &["gateway", "secret_encryption", "key"][..],
            "HARNESS_GATEWAY_ENCRYPTION_KEY",
        ),
    ] {
        redact_literal_secret(&mut document, path, variable.to_owned(), &mut redactions);
    }
    if let Some(connections) = document
        .get_mut(yaml_key("control_api"))
        .and_then(|value| value.get_mut(yaml_key("package_sources")))
        .and_then(|value| value.get_mut(yaml_key("connections")))
        .and_then(serde_yaml::Value::as_sequence_mut)
    {
        for connection in connections {
            let Some(mapping) = connection.as_mapping_mut() else {
                continue;
            };
            let id = mapping
                .get(yaml_key("id"))
                .and_then(serde_yaml::Value::as_str)
                .unwrap_or("CONNECTION")
                .to_owned();
            let environment_id = connection_environment_id(&id);
            for (field, suffix) in [("token", "TOKEN"), ("private_key", "PRIVATE_KEY")] {
                let Some(value) = mapping.get_mut(yaml_key(field)) else {
                    continue;
                };
                let Some(raw) = value.as_str() else { continue };
                if raw.starts_with("os.environ/")
                    || raw.starts_with("env://")
                    || raw.starts_with("file://")
                {
                    continue;
                }
                let variable = format!("BLUE_PACKAGE_SOURCE_{environment_id}_{suffix}");
                *value = serde_yaml::Value::String(format!("os.environ/{variable}"));
                redactions.push(BlueConfigRedaction {
                    path: format!("control_api.package_sources.connections[id={id}].{field}"),
                    environment_variable: variable,
                });
            }
        }
    }
    let yaml = serde_yaml::to_string(&document)
        .map_err(|error| ApiError::internal(format!("serializing Blue config export: {error}")))?;
    Ok(Json(BlueConfigExportResponse { yaml, redactions }))
}

fn supported_harness(harness: &str) -> bool {
    gh_config::implementations::HARNESS_DEFINITIONS
        .iter()
        .any(|definition| definition.metadata.key == harness)
}

fn extension_free_yaml(config: &gh_service::GovernanceConfig) -> Result<String, ApiError> {
    let mut value = serde_yaml::to_value(config)
        .map_err(|error| ApiError::internal(format!("serializing managed config: {error}")))?;
    let root = value
        .as_mapping_mut()
        .ok_or_else(|| ApiError::internal("governance config did not serialize as a mapping"))?;
    root.remove(yaml_key("packages"));
    if let Some(harnesses) = root
        .get_mut(yaml_key("harnesses"))
        .and_then(serde_yaml::Value::as_mapping_mut)
    {
        for policy in harnesses.values_mut() {
            if let Some(policy) = policy.as_mapping_mut() {
                for key in ["mcp", "package_overrides", "skills", "plugins"] {
                    policy.remove(yaml_key(key));
                }
            }
        }
    }
    serde_yaml::to_string(&value)
        .map_err(|error| ApiError::internal(format!("serializing managed config: {error}")))
}

fn reject_extension_owned_yaml(yaml: &str) -> Result<(), ApiError> {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|error| ApiError::bad_request(format!("invalid governance config: {error}")))?;
    let root = value
        .as_mapping()
        .ok_or_else(|| ApiError::bad_request("governance config must be a YAML mapping"))?;
    if root.contains_key(yaml_key("packages")) {
        return Err(ApiError::bad_request(
            "`packages` is managed from the Extensions page",
        ));
    }
    if root
        .get(yaml_key("gateway"))
        .and_then(serde_yaml::Value::as_mapping)
        .is_some_and(|gateway| gateway.contains_key(yaml_key("model")))
    {
        return Err(ApiError::bad_request(
            "`gateway.model` is invalid; models belong in each harness's `managed_config`",
        ));
    }
    if let Some(harnesses) = root
        .get(yaml_key("harnesses"))
        .and_then(serde_yaml::Value::as_mapping)
    {
        for (harness, policy) in harnesses {
            let Some(policy) = policy.as_mapping() else {
                continue;
            };
            if policy.contains_key(yaml_key("session_upload")) {
                return Err(ApiError::bad_request(format!(
                    "`harnesses.{}.session_upload` is invalid; `session_upload` is a global policy",
                    harness.as_str().unwrap_or("<unknown>")
                )));
            }
            if policy.contains_key(yaml_key("gateway")) {
                return Err(ApiError::bad_request(format!(
                    "`harnesses.{}.gateway` is invalid; `gateway` is a global policy",
                    harness.as_str().unwrap_or("<unknown>")
                )));
            }
            for key in ["mcp", "package_overrides", "skills", "plugins"] {
                if policy.contains_key(yaml_key(key)) {
                    return Err(ApiError::bad_request(format!(
                        "`harnesses.{}.{key}` is managed from the Extensions page",
                        harness.as_str().unwrap_or("<unknown>")
                    )));
                }
            }
        }
    }
    Ok(())
}

async fn admin_config_response(
    pool: &PgPool,
    row: ConfigRow,
) -> Result<AdminConfigResponse, ApiError> {
    let document: gh_service::GovernanceConfig = serde_json::from_value(row.document)
        .map_err(|error| ApiError::internal(format!("decoding stored config: {error}")))?;
    let package_audiences =
        package_audiences_for_revision(pool, &row.revision, &document.packages).await?;
    Ok(AdminConfigResponse {
        revision: row.revision,
        yaml: row.yaml,
        managed_yaml: extension_free_yaml(&document)?,
        document,
        package_audiences,
        created_at: now_text(row.created_at),
    })
}

fn merge_current_extensions(
    next: &mut gh_service::GovernanceConfig,
    current: gh_service::GovernanceConfig,
) {
    next.packages = current.packages;
    for (harness, current_policy) in current.harnesses {
        if !supported_harness(&harness) {
            continue;
        }
        if current_policy.mcp.is_empty() && current_policy.package_overrides.is_empty() {
            continue;
        }
        let next_policy = next.harnesses.entry(harness).or_default();
        next_policy.mcp = current_policy.mcp;
        next_policy.package_overrides = current_policy.package_overrides;
    }
}

async fn admin_governance_config(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<AdminConfigResponse>, ApiError> {
    let row = current_config(&state.pool, who.organization_id).await?;
    Ok(Json(admin_config_response(&state.pool, row).await?))
}

#[derive(Deserialize)]
struct UpdateConfigRequest {
    base_revision: String,
    managed_yaml: String,
}

async fn update_governance_config(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Json(input): Json<UpdateConfigRequest>,
) -> Result<Json<AdminConfigResponse>, ApiError> {
    let current = current_config(&state.pool, who.organization_id).await?;
    if current.revision != input.base_revision {
        return Err(ApiError::conflict(
            "governance configuration changed; reload before saving",
        ));
    }
    reject_extension_owned_yaml(&input.managed_yaml)?;
    let mut next =
        gh_service::source::parse_config(std::path::Path::new("config.yaml"), &input.managed_yaml)
            .map_err(|error| {
                ApiError::bad_request(format!("invalid governance config: {error}"))
            })?;
    let current_document: gh_service::GovernanceConfig = serde_json::from_value(current.document)
        .map_err(|error| {
        ApiError::internal(format!("decoding stored governance config: {error}"))
    })?;
    merge_current_extensions(&mut next, current_document);
    apply_deployment_gateway_policy(&mut next, state.config.gateway_kind.as_deref());
    let merged_yaml = serde_yaml::to_string(&next)
        .map_err(|error| ApiError::internal(format!("serializing governance config: {error}")))?;
    let row = insert_governance_revision(
        &state.pool,
        who.organization_id,
        Some(who.user_id),
        &merged_yaml,
        "dashboard",
        Some(&input.base_revision),
        None,
    )
    .await?;
    Ok(Json(admin_config_response(&state.pool, row).await?))
}

#[derive(Deserialize)]
struct PackageCatalogDocument {
    #[serde(default)]
    packages: Vec<gh_service::ManagedPackage>,
}

fn read_package_catalog(config: &AppConfig) -> Result<Vec<gh_service::ManagedPackage>, ApiError> {
    let document = read_blue_config_with_overlays(
        &config.blue_config_file,
        &config.blue_config_overlay_files,
    )?;
    let Some(catalog) = document.get("package_catalog") else {
        return Ok(Vec::new());
    };
    let document: PackageCatalogDocument = serde_yaml::from_value(catalog.clone())
        .map_err(|error| ApiError::internal(format!("parsing package catalog: {error}")))?;
    validate_packages(&document.packages).map_err(ApiError::internal)?;
    Ok(document.packages)
}

fn validate_packages(packages: &[gh_service::ManagedPackage]) -> Result<(), String> {
    let mut ids = std::collections::BTreeSet::new();
    for package in packages {
        if package.id.is_empty()
            || !package
                .id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(format!("invalid package id `{}`", package.id));
        }
        if !ids.insert(package.id.as_str()) {
            return Err(format!("duplicate package id `{}`", package.id));
        }
        if package.version.trim().is_empty() {
            return Err(format!("package `{}` has no version", package.id));
        }
        if package.sha256.len() != 64
            || !package.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!(
                "package `{}` must have a 64-character sha256",
                package.id
            ));
        }
        for (platform, source) in &package.platform_sources {
            if platform.is_empty()
                || !platform.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_')
                })
            {
                return Err(format!(
                    "package `{}` has invalid platform source `{platform}`",
                    package.id
                ));
            }
            if source.sha256.len() != 64
                || !source.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(format!(
                    "package `{}` platform `{platform}` must have a 64-character sha256",
                    package.id
                ));
            }
            if source.artifact_id.is_none()
                && !(source.source_ref.starts_with("https://")
                    || source.source_ref.starts_with("file://")
                    || !source.source_ref.contains("://"))
            {
                return Err(format!(
                    "package `{}` platform `{platform}` source must use HTTPS",
                    package.id
                ));
            }
        }
        if package.artifact_id.is_none()
            && !(package.source_ref.starts_with("https://")
                || package.source_ref.starts_with("file://")
                || !package.source_ref.contains("://"))
        {
            return Err(format!("package `{}` source must use HTTPS", package.id));
        }
        for harness in package.adapters.keys() {
            if !supported_harness(harness) {
                return Err(format!(
                    "package `{}` has unsupported harness adapter `{harness}`",
                    package.id
                ));
            }
        }
        for (harness, adapter) in &package.adapters {
            let availability = adapter.availability().map_err(|error| {
                format!(
                    "package `{}` {harness} has invalid adapter availability: {error}",
                    package.id
                )
            })?;
            let mut intervals = Vec::new();
            for variant in &adapter.variants {
                let interval = variant.interval().map_err(|error| {
                    format!(
                        "package `{}` {harness} has invalid adapter interval: {error}",
                        package.id
                    )
                })?;
                if interval.introduced < availability.introduced
                    || match (&interval.before, &availability.before) {
                        (Some(before), Some(available_before)) => before > available_before,
                        (None, Some(_)) => true,
                        _ => false,
                    }
                {
                    return Err(format!(
                        "package `{}` {harness} adapter variant at `{}` escapes adapter availability",
                        package.id, interval.introduced
                    ));
                }
                if intervals
                    .iter()
                    .any(|existing: &gh_service::PackageAdapterInterval| {
                        existing.overlaps(&interval)
                    })
                {
                    return Err(format!(
                        "package `{}` {harness} has overlapping adapter intervals at `{}`",
                        package.id, interval.introduced
                    ));
                }
                intervals.push(interval);
                let variant_adapter = variant.as_adapter();
                validate_package_adapter_shape(&package.id, harness, &variant_adapter)?;
            }
            if intervals
                .windows(2)
                .any(|pair| pair[0].introduced >= pair[1].introduced)
            {
                return Err(format!(
                    "package `{}` {harness} adapter intervals must be ordered by introduced version",
                    package.id
                ));
            }
            let has_fallback = adapter.plugin_dir.is_some()
                || adapter.skills_dir.is_some()
                || adapter.agents_dir.is_some()
                || adapter.hooks_file.is_some()
                || !adapter.plugins.is_empty()
                || !adapter.helpers.is_empty();
            if !has_fallback && !intervals.is_empty() {
                if intervals[0].introduced != availability.introduced {
                    return Err(format!("package `{}` {harness} adapter intervals have a gap before {} and no fallback", package.id, intervals[0].introduced));
                }
                for pair in intervals.windows(2) {
                    if pair[0].before.as_ref() != Some(&pair[1].introduced) {
                        return Err(format!("package `{}` {harness} adapter intervals contain a gap before {} and no fallback", package.id, pair[1].introduced));
                    }
                }
                if intervals
                    .last()
                    .and_then(|interval| interval.before.as_ref())
                    != availability.before.as_ref()
                {
                    return Err(format!("package `{}` {harness} adapter intervals have an open-ended gap and no fallback", package.id));
                }
            }
            validate_package_adapter_shape(&package.id, harness, adapter)?;
        }
    }
    Ok(())
}

fn validate_package_adapter_shape(
    package_id: &str,
    harness: &str,
    adapter: &gh_service::PackageAdapter,
) -> Result<(), String> {
    let definition = gh_config::implementations::HARNESS_DEFINITIONS
        .iter()
        .find(|definition| definition.metadata.key == harness)
        .ok_or_else(|| format!("unsupported package harness `{harness}`"))?;
    // Catalog acceptance requires a usable production implementation. Older
    // clients still validate against their exact selected interval at apply.
    if !definition.implementations.iter().any(|registration| {
        registration
            .implementation
            .validate_components(adapter)
            .is_ok()
    }) {
        return Err(format!("package `{package_id}` {harness}: no supported implementation accepts these components"));
    }
    let paths = adapter
        .plugin_dir
        .iter()
        .chain(adapter.skills_dir.iter())
        .chain(adapter.agents_dir.iter())
        .chain(adapter.hooks_file.iter())
        .chain(adapter.plugins.iter())
        .chain(
            adapter
                .helpers
                .values()
                .flat_map(|asset| asset.paths.values()),
        );
    for path in paths {
        let candidate = std::path::Path::new(path);
        if candidate.is_absolute()
            || candidate.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            return Err(format!(
                "package `{package_id}` contains unsafe adapter path `{path}`"
            ));
        }
    }
    Ok(())
}

fn validate_governance_packages(config: &gh_service::GovernanceConfig) -> Result<(), String> {
    validate_packages(&config.packages)?;
    let package_ids = config
        .packages
        .iter()
        .map(|package| package.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for (harness, policy) in &config.harnesses {
        for package_id in policy.package_overrides.keys() {
            if !package_ids.contains(package_id.as_str()) {
                return Err(format!(
                    "harness `{harness}` override references unselected package `{package_id}`"
                ));
            }
        }
        if policy.allow_unverified_versions
            && policy
                .version_requirement
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            return Err(format!(
                "harness `{harness}` enables allow_unverified_versions without version_requirement"
            ));
        }
    }
    for harness in gh_config::implementations::HARNESS_DEFINITIONS
        .iter()
        .map(|definition| definition.metadata.key)
    {
        let mut adapters = Vec::new();
        for package in &config.packages {
            if !config
                .allowed_harnesses
                .iter()
                .any(|allowed| allowed == harness)
            {
                continue;
            }
            if config
                .harnesses
                .get(harness)
                .and_then(|policy| policy.package_overrides.get(&package.id))
                .and_then(|item| item.enabled)
                == Some(false)
            {
                continue;
            }
            let Some(adapter) = package.adapters.get(harness) else {
                continue;
            };
            adapters.push((package.id.as_str(), adapter));
        }
        let policy = config.harnesses.get(harness).cloned().unwrap_or_default();
        gh_config::validate_package_adapters_for_policy(
            harness.parse().map_err(|error| format!("{error}"))?,
            &policy,
            &adapters,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// A harness range that matches no Blue-certified release is a mistake worth
/// blocking where an admin makes it, but *not* worth refusing to boot over.
/// A deployment can already hold one — it saved cleanly before this check
/// existed — and the API that would fix it is the thing that would be down.
/// So this is kept out of `validate_complete_governance`, which runs on the
/// startup reconcile path, and is enforced only on admin writes.
fn uncertified_harness_range(config: &gh_service::GovernanceConfig) -> Option<String> {
    config.harnesses.iter().find_map(|(harness, policy)| {
        let requirement = policy.version_requirement.as_ref()?;
        let parsed_harness = harness.parse().ok()?;
        if gh_config::supported_install(parsed_harness, policy).is_ok() {
            return None;
        }
        let action = if policy.allow_unverified_versions {
            "adjust the allowed range so it includes a supported harness release"
        } else {
            "widen the allowed range or enable `Allow unverified versions` to accept releases beyond Blue's certified ceiling"
        };
        Some(format!(
            "harness `{harness}` version requirement `{requirement}` includes no Blue-certified release; {action}"
        ))
    })
}

fn validate_complete_governance(config: &gh_service::GovernanceConfig) -> Result<(), String> {
    if config.contract_version == 0
        || config.contract_version > gh_service::GovernanceConfig::CONTRACT_VERSION
    {
        return Err(format!(
            "unsupported governance contract_version {}",
            config.contract_version
        ));
    }
    for capability in &config.required_capabilities {
        if !gh_service::GovernanceConfig::CAPABILITIES.contains(&capability.as_str()) {
            return Err(format!("unknown required capability `{capability}`"));
        }
    }
    if config.gateway.is_some()
        && !config
            .required_capabilities
            .iter()
            .any(|capability| capability == "gateway_inference_jwt")
    {
        return Err("gateway mode requires capability `gateway_inference_jwt`".into());
    }
    if let Some(gateway) = config.gateway.as_ref() {
        if gateway.proxy_url.is_some() || gateway.token.is_some() {
            return Err("gateway proxy_url and token are runtime-only fields".into());
        }
        if gateway.auth_style != "bearer" {
            return Err("gateway auth_style must be `bearer`".into());
        }
    }
    if let Some(minimum) = &config.minimum_client_version {
        semver::Version::parse(minimum)
            .map_err(|error| format!("invalid minimum_client_version `{minimum}`: {error}"))?;
    }
    for harness in &config.allowed_harnesses {
        if !supported_harness(harness) {
            return Err(format!("unsupported allowed harness `{harness}`"));
        }
    }
    for (harness, policy) in &config.harnesses {
        if !supported_harness(harness) {
            return Err(format!("unsupported harness policy `{harness}`"));
        }
        if let Some(requirement) = &policy.version_requirement {
            semver::VersionReq::parse(requirement).map_err(|error| {
                format!(
                    "harness `{harness}` has invalid version requirement `{requirement}`: {error}"
                )
            })?;
        }
    }
    let uses_version_aware_features = config
        .harnesses
        .values()
        .any(|policy| policy.version_requirement.is_some() || policy.allow_unverified_versions)
        || config.packages.iter().any(|package| {
            package
                .adapters
                .values()
                .any(|adapter| !adapter.variants.is_empty())
        });
    if uses_version_aware_features && config.minimum_client_version.is_none() {
        return Err(
            "version requirements and package adapter variants require minimum_client_version"
                .into(),
        );
    }
    if uses_version_aware_features {
        for capability in [
            "adapter_intervals",
            "compiled_harness_registry",
            "transactional_reconcile",
            "versioned_state",
        ] {
            if !config
                .required_capabilities
                .iter()
                .any(|value| value == capability)
            {
                return Err(format!(
                    "version-aware governance requires capability `{capability}`"
                ));
            }
        }
        if config
            .harnesses
            .values()
            .any(|policy| policy.allow_unverified_versions)
            && !config
                .required_capabilities
                .iter()
                .any(|value| value == "unverified_harness_versions")
        {
            return Err(
                "unverified harness versions require capability `unverified_harness_versions`"
                    .into(),
            );
        }
    }
    validate_governance_packages(config)?;
    let mcp = config
        .harnesses
        .iter()
        .filter(|(_, policy)| !policy.mcp.is_empty())
        .map(|(harness, policy)| (harness.clone(), policy.mcp.clone()))
        .collect::<BTreeMap<_, _>>();
    validate_mcp_servers(&mcp)
}

async fn package_catalog(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<gh_service::ManagedPackage>>, ApiError> {
    Ok(Json(read_package_catalog(&state.config)?))
}

#[derive(Deserialize)]
struct InspectPackageSourceRequest {
    #[serde(default)]
    source_ref: Option<String>,
    #[serde(default)]
    connection_id: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default, rename = "ref")]
    requested_ref: Option<String>,
}

#[derive(Serialize)]
struct InspectPackageSourceResponse {
    source_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_commit: Option<String>,
    sha256: String,
    size_bytes: usize,
}

#[derive(Serialize)]
struct PackageSourceConnectionResponse {
    id: String,
    name: String,
    provider: String,
    namespaces: Vec<String>,
}

async fn organization_slug(pool: &PgPool, id: Uuid) -> Result<String, ApiError> {
    sqlx::query_scalar!("SELECT slug FROM organizations WHERE id=$1", id)
        .fetch_one(pool)
        .await
        .map_err(Into::into)
}

async fn package_source_connections_api(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<Vec<PackageSourceConnectionResponse>>, ApiError> {
    let slug = organization_slug(&state.pool, who.organization_id).await?;
    Ok(Json(
        state
            .config
            .package_source_connections
            .iter()
            .filter_map(|connection| {
                connection.organizations.get(&slug).map(|namespaces| {
                    PackageSourceConnectionResponse {
                        id: connection.id.clone(),
                        name: connection
                            .name
                            .clone()
                            .unwrap_or_else(|| connection.id.clone()),
                        provider: connection.provider.clone(),
                        namespaces: namespaces.clone(),
                    }
                })
            })
            .collect(),
    ))
}

fn safe_repository(value: &str) -> Result<(&str, &str), ApiError> {
    let (namespace, repository) = value
        .split_once('/')
        .ok_or_else(|| ApiError::bad_request("repository must use namespace/repository"))?;
    if [namespace, repository].iter().any(|part| {
        let part = *part;
        part.is_empty()
            || matches!(part, "." | "..")
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    }) {
        return Err(ApiError::bad_request(
            "repository contains unsafe characters",
        ));
    }
    Ok((namespace, repository))
}

fn safe_ref(value: &str) -> Result<&str, ApiError> {
    if value.is_empty()
        || value
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        Err(ApiError::bad_request(
            "repository ref contains unsafe characters",
        ))
    } else {
        Ok(value)
    }
}

fn api_url(base: &str, segments: &[&str]) -> Result<reqwest::Url, ApiError> {
    let mut url = reqwest::Url::parse(base)
        .map_err(|error| ApiError::internal(format!("invalid provider API URL: {error}")))?;
    if url.scheme() != "https" && !cfg!(test) {
        return Err(ApiError::internal("provider API URLs must use HTTPS"));
    }
    let trailing_slash = url.path().ends_with('/');
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| ApiError::internal("provider API URL cannot be a base URL"))?;
        if trailing_slash {
            path.pop_if_empty();
        }
        path.extend(segments);
    }
    Ok(url)
}

fn provider_client(connection: &PackageSourceConnection) -> Result<reqwest::Client, ApiError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("governance-harness-control-api");
    if let Some(bundle) = connection.ca_bundle.as_deref() {
        let pem = resolve_secret_reference(bundle)?;
        let certificates = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
            .map_err(|error| ApiError::internal(format!("invalid provider CA bundle: {error}")))?;
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    builder
        .build()
        .map_err(|error| ApiError::internal(format!("building provider client: {error}")))
}

#[derive(Serialize)]
struct GitHubAppClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

async fn github_token(
    connection: &PackageSourceConnection,
    client: &reqwest::Client,
    owner: &str,
    repository: &str,
) -> Result<String, ApiError> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let key = EncodingKey::from_rsa_pem(
        connection
            .private_key
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
    )
    .map_err(|error| ApiError::internal(format!("invalid GitHub App private key: {error}")))?;
    let jwt = encode(
        &Header::new(Algorithm::RS256),
        &GitHubAppClaims {
            iat: now - 60,
            exp: now + 540,
            iss: connection.app_id.clone().unwrap_or_default(),
        },
        &key,
    )
    .map_err(|error| ApiError::internal(format!("creating GitHub App JWT: {error}")))?;
    let base = connection
        .api_base_url
        .as_deref()
        .unwrap_or("https://api.github.com");
    let installation_url = api_url(base, &["repos", owner, repository, "installation"])?;
    let installation = client
        .get(installation_url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| ApiError::bad_request(format!("contacting GitHub: {error}")))?;
    if !installation.status().is_success() {
        return Err(ApiError::bad_request(format!(
            "GitHub App cannot access repository (HTTP {})",
            installation.status()
        )));
    }
    let installation: serde_json::Value = installation
        .json()
        .await
        .map_err(|error| ApiError::bad_request(format!("decoding GitHub installation: {error}")))?;
    let id = installation
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| ApiError::bad_request("GitHub returned no installation id"))?;
    let token_url = api_url(
        base,
        &["app", "installations", &id.to_string(), "access_tokens"],
    )?;
    let response = client
        .post(token_url)
        .bearer_auth(jwt)
        .header("Accept", "application/vnd.github+json")
        .json(&json!({"repositories":[repository],"permissions":{"contents":"read"}}))
        .send()
        .await
        .map_err(|error| {
            ApiError::bad_request(format!("creating GitHub installation token: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(ApiError::bad_request(format!(
            "GitHub installation token request returned HTTP {}",
            response.status()
        )));
    }
    response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| ApiError::bad_request(format!("decoding GitHub token: {error}")))?
        .get("token")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| ApiError::bad_request("GitHub returned no installation token"))
}

struct ResolvedProviderSource {
    source_ref: String,
    commit: String,
    archive_url: reqwest::Url,
    authorization: String,
}

async fn resolve_provider_source(
    connection: &PackageSourceConnection,
    repository: &str,
    requested_ref: &str,
) -> Result<(reqwest::Client, ResolvedProviderSource), ApiError> {
    let (namespace, repository_name) = safe_repository(repository)?;
    let requested_ref = safe_ref(requested_ref)?;
    let client = provider_client(connection)?;
    let (commit, archive_url, authorization, scheme) = match connection.provider.as_str() {
        "github" => {
            let base = connection
                .api_base_url
                .as_deref()
                .unwrap_or("https://api.github.com");
            let token = github_token(connection, &client, namespace, repository_name).await?;
            let response = client
                .get(api_url(
                    base,
                    &[
                        "repos",
                        namespace,
                        repository_name,
                        "commits",
                        requested_ref,
                    ],
                )?)
                .bearer_auth(&token)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|error| ApiError::bad_request(format!("resolving GitHub ref: {error}")))?;
            if !response.status().is_success() {
                return Err(ApiError::bad_request(format!(
                    "GitHub ref resolution returned HTTP {}",
                    response.status()
                )));
            }
            let value: serde_json::Value = response
                .json()
                .await
                .map_err(|error| ApiError::bad_request(format!("decoding GitHub ref: {error}")))?;
            let commit = value
                .get("sha")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ApiError::bad_request("GitHub returned no commit SHA"))?
                .to_owned();
            let archive = api_url(
                base,
                &["repos", namespace, repository_name, "tarball", &commit],
            )?;
            (commit, archive, format!("Bearer {token}"), "github")
        }
        "bitbucket_cloud" => {
            let base = connection
                .api_base_url
                .as_deref()
                .unwrap_or("https://api.bitbucket.org/2.0");
            let token = connection.token.as_deref().unwrap_or_default();
            let response = client
                .get(api_url(
                    base,
                    &[
                        "repositories",
                        namespace,
                        repository_name,
                        "commit",
                        requested_ref,
                    ],
                )?)
                .bearer_auth(token)
                .send()
                .await
                .map_err(|error| {
                    ApiError::bad_request(format!("resolving Bitbucket ref: {error}"))
                })?;
            if !response.status().is_success() {
                return Err(ApiError::bad_request(format!(
                    "Bitbucket ref resolution returned HTTP {}",
                    response.status()
                )));
            }
            let value: serde_json::Value = response.json().await.map_err(|error| {
                ApiError::bad_request(format!("decoding Bitbucket ref: {error}"))
            })?;
            let commit = value
                .get("hash")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ApiError::bad_request("Bitbucket returned no commit hash"))?
                .to_owned();
            let web = connection
                .web_base_url
                .as_deref()
                .unwrap_or("https://bitbucket.org");
            let archive = api_url(
                web,
                &[
                    namespace,
                    repository_name,
                    "get",
                    &format!("{commit}.tar.gz"),
                ],
            )?;
            (commit, archive, format!("Bearer {token}"), "bitbucket")
        }
        "bitbucket_data_center" => {
            let base = connection
                .api_base_url
                .as_deref()
                .ok_or_else(|| ApiError::internal("Bitbucket Data Center requires api_base_url"))?;
            let token = connection.token.as_deref().unwrap_or_default();
            let response = client
                .get(api_url(
                    base,
                    &[
                        "rest",
                        "api",
                        "latest",
                        "projects",
                        namespace,
                        "repos",
                        repository_name,
                        "commits",
                        requested_ref,
                    ],
                )?)
                .bearer_auth(token)
                .send()
                .await
                .map_err(|error| {
                    ApiError::bad_request(format!("resolving Bitbucket ref: {error}"))
                })?;
            if !response.status().is_success() {
                return Err(ApiError::bad_request(format!(
                    "Bitbucket ref resolution returned HTTP {}",
                    response.status()
                )));
            }
            let value: serde_json::Value = response.json().await.map_err(|error| {
                ApiError::bad_request(format!("decoding Bitbucket ref: {error}"))
            })?;
            let commit = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ApiError::bad_request("Bitbucket returned no commit id"))?
                .to_owned();
            let mut archive = api_url(
                base,
                &[
                    "rest",
                    "api",
                    "latest",
                    "projects",
                    namespace,
                    "repos",
                    repository_name,
                    "archive",
                ],
            )?;
            archive
                .query_pairs_mut()
                .append_pair("at", &commit)
                .append_pair("format", "tgz");
            (commit, archive, format!("Bearer {token}"), "bitbucket")
        }
        _ => return Err(ApiError::internal("unsupported package source provider")),
    };
    Ok((
        client,
        ResolvedProviderSource {
            source_ref: format!("{scheme}://{}/{repository}@{requested_ref}", connection.id),
            commit,
            archive_url,
            authorization,
        },
    ))
}

async fn fetch_provider_archive(
    client: &reqwest::Client,
    connection: &PackageSourceConnection,
    repository: &str,
    source: &ResolvedProviderSource,
) -> Result<Vec<u8>, ApiError> {
    const MAX_PACKAGE_BYTES: usize = 100 * 1024 * 1024;
    if connection.provider == "bitbucket_cloud" {
        return fetch_bitbucket_cloud_git_archive(connection, repository, &source.commit).await;
    }
    let original_host = source.archive_url.host_str().unwrap_or_default().to_owned();
    let mut url = source.archive_url.clone();
    for redirect in 0..=5 {
        let mut request = client.get(url.clone());
        if same_url_origin(&url, &source.archive_url) {
            request = request.header("Authorization", &source.authorization);
        }
        let mut response = request.send().await.map_err(|error| {
            ApiError::bad_request(format!("fetching repository archive: {error}"))
        })?;
        if response.status().is_redirection() {
            if redirect == 5 {
                return Err(ApiError::bad_request(
                    "repository archive redirected too many times",
                ));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    ApiError::bad_request("repository archive redirect had no location")
                })?;
            let next = url.join(location).map_err(|error| {
                ApiError::bad_request(format!("invalid archive redirect: {error}"))
            })?;
            // `cfg!(test)` keeps the plaintext test servers usable; the rule
            // itself is covered by `redirect_scheme_allowed` unit tests.
            if !redirect_scheme_allowed(&next, cfg!(test)) {
                return Err(ApiError::bad_request(
                    "repository archive redirect must use HTTPS",
                ));
            }
            let host = next.host_str().unwrap_or_default();
            if !managed_redirect_host_allowed(connection, &original_host, host) {
                return Err(ApiError::bad_request(
                    "repository archive redirected to an untrusted host",
                ));
            }
            url = next;
            continue;
        }
        if !response.status().is_success() {
            return Err(ApiError::bad_request(format!(
                "repository archive returned HTTP {}",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PACKAGE_BYTES as u64)
        {
            return Err(ApiError::bad_request(
                "package archive exceeds the 100 MiB limit",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            ApiError::bad_request(format!("reading repository archive: {error}"))
        })? {
            if bytes.len() + chunk.len() > MAX_PACKAGE_BYTES {
                return Err(ApiError::bad_request(
                    "package archive exceeds the 100 MiB limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(bytes);
    }
    Err(ApiError::bad_request(
        "repository archive could not be downloaded",
    ))
}

fn same_url_origin(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

/// A managed archive redirect may only stay on HTTPS. Test builds pass
/// `allow_plaintext` so local `http://` fixtures keep working.
fn redirect_scheme_allowed(next: &reqwest::Url, allow_plaintext: bool) -> bool {
    next.scheme() == "https" || allow_plaintext
}

fn managed_redirect_host_allowed(
    connection: &PackageSourceConnection,
    original_host: &str,
    host: &str,
) -> bool {
    host.eq_ignore_ascii_case(original_host)
        || matches!(
            (connection.provider.as_str(), host),
            (
                "github",
                "codeload.github.com" | "objects.githubusercontent.com"
            ) | ("bitbucket_cloud", "bitbucket.org")
        )
        || connection
            .download_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
}

async fn fetch_bitbucket_cloud_git_archive(
    connection: &PackageSourceConnection,
    repository: &str,
    commit: &str,
) -> Result<Vec<u8>, ApiError> {
    const MAX_PACKAGE_BYTES: usize = 100 * 1024 * 1024;
    let (namespace, repository_name) = safe_repository(repository)?;
    let token = connection
        .token
        .as_deref()
        .ok_or_else(|| ApiError::internal("Bitbucket Cloud connection requires token"))?;
    let web = connection
        .web_base_url
        .as_deref()
        .unwrap_or("https://bitbucket.org");
    let clone_url = api_url(web, &[namespace, &format!("{repository_name}.git")])?;
    let temp_root = std::env::temp_dir().join(format!("harness-bitbucket-{}", Uuid::new_v4()));
    let checkout = temp_root.join("checkout");
    gh_common::create_owner_only_dir(&temp_root).map_err(|error| {
        ApiError::internal(format!("creating Bitbucket checkout directory: {error}"))
    })?;

    let result = async {
        let init = tokio::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(&checkout)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .map_err(|error| ApiError::internal(format!("starting git: {error}")))?;
        if !init.status.success() {
            return Err(ApiError::internal("initializing Bitbucket checkout failed"));
        }

        use base64::Engine as _;
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("x-token-auth:{token}"));
        let mut origin = clone_url.clone();
        origin.set_path("/");
        origin.set_query(None);
        origin.set_fragment(None);
        let mut git_config = vec![
            ("http.followRedirects".to_owned(), "false".to_owned()),
            ("http.extraHeader".to_owned(), String::new()),
            ("credential.helper".to_owned(), String::new()),
            (
                format!("http.{origin}.extraHeader"),
                format!("Authorization: Basic {credentials}"),
            ),
        ];
        if let Some(pem) = connection.ca_bundle.as_deref() {
            let ca_path = temp_root.join("provider-ca.pem");
            gh_common::write_atomic(&ca_path, pem.as_bytes()).map_err(|error| {
                ApiError::internal(format!("writing Bitbucket CA bundle: {error}"))
            })?;
            git_config.push((
                format!("http.{origin}.sslCAInfo"),
                ca_path.to_string_lossy().into_owned(),
            ));
        }
        let mut fetch_command = tokio::process::Command::new("git");
        fetch_command
            .arg("-C")
            .arg(&checkout)
            .args(["fetch", "--quiet", "--depth=1", "--no-tags"])
            .arg(clone_url.as_str())
            .arg(commit)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", git_config.len().to_string());
        for (index, (key, value)) in git_config.iter().enumerate() {
            fetch_command
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
        let fetch = fetch_command
            .output()
            .await
            .map_err(|error| ApiError::internal(format!("starting git fetch: {error}")))?;
        if !fetch.status.success() {
            return Err(ApiError::bad_request(format!(
                "fetching Bitbucket repository returned {}",
                fetch.status
            )));
        }

        let checkout_result = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["checkout", "--quiet", "--detach", "FETCH_HEAD"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .map_err(|error| ApiError::internal(format!("starting git checkout: {error}")))?;
        if !checkout_result.status.success() {
            return Err(ApiError::internal("checking out Bitbucket commit failed"));
        }
        tokio::fs::remove_dir_all(checkout.join(".git"))
            .await
            .map_err(|error| ApiError::internal(format!("removing Git metadata: {error}")))?;

        let archive_root = checkout.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            archive
                .append_dir_all(".", archive_root)
                .map_err(|error| format!("packing Bitbucket checkout: {error}"))?;
            let encoder = archive
                .into_inner()
                .map_err(|error| format!("finishing Bitbucket archive: {error}"))?;
            encoder
                .finish()
                .map_err(|error| format!("compressing Bitbucket archive: {error}"))
        })
        .await
        .map_err(|error| ApiError::internal(format!("joining archive task: {error}")))?
        .map_err(ApiError::internal)?;
        if bytes.len() > MAX_PACKAGE_BYTES {
            return Err(ApiError::bad_request(
                "package archive exceeds the 100 MiB limit",
            ));
        }
        Ok(bytes)
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&temp_root).await;
    result
}

fn public_https_url(value: &str) -> Result<reqwest::Url, ApiError> {
    let url = reqwest::Url::parse(value)
        .map_err(|error| ApiError::bad_request(format!("invalid package URL: {error}")))?;
    if url.scheme() != "https" {
        return Err(ApiError::bad_request(
            "custom package sources must use HTTPS",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ApiError::bad_request(
            "custom package URLs must not contain credentials",
        ));
    }
    if url.fragment().is_some() {
        return Err(ApiError::bad_request(
            "custom package URLs must not contain fragments",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ApiError::bad_request("package URL has no host"))?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        return Err(ApiError::bad_request("package URL must use a public host"));
    }
    let address_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(address) = address_host.parse::<std::net::IpAddr>() {
        if !gh_common::network::is_public_ip(address) {
            return Err(ApiError::bad_request("package URL must use a public host"));
        }
    }
    Ok(url)
}

fn public_request_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_body() {
        "response body failed"
    } else if error.is_decode() {
        "response decoding failed"
    } else {
        "request failed"
    }
}

async fn public_https_client(url: &reqwest::Url) -> Result<reqwest::Client, ApiError> {
    let host = url
        .host_str()
        .ok_or_else(|| ApiError::bad_request("package URL has no host"))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let address_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let addresses = if let Ok(address) = address_host.parse::<std::net::IpAddr>() {
        vec![std::net::SocketAddr::new(address, port)]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|error| ApiError::bad_request(format!("resolving package host: {error}")))?
            .collect::<Vec<_>>()
    };
    if !gh_common::network::all_addresses_are_public(&addresses) {
        return Err(ApiError::bad_request(
            "package URL must resolve only to public addresses",
        ));
    }
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("blue-control-api")
        .no_proxy()
        .https_only(true)
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|error| ApiError::internal(format!("building package inspector: {error}")))
}

async fn resolve_package_source(source_ref: &str) -> Result<reqwest::Url, ApiError> {
    if let Some(github) = source_ref.strip_prefix("github:") {
        let (repository, requested_ref) = github
            .rsplit_once('@')
            .ok_or_else(|| ApiError::bad_request("GitHub sources use github:owner/repo@ref"))?;
        let (owner, repo) = repository
            .split_once('/')
            .ok_or_else(|| ApiError::bad_request("GitHub sources use github:owner/repo@ref"))?;
        if [owner, repo, requested_ref].iter().any(|value| {
            value.is_empty()
                || !value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/')
                })
        }) {
            return Err(ApiError::bad_request(
                "GitHub package source contains unsafe characters",
            ));
        }
        let commit_url =
            format!("https://api.github.com/repos/{owner}/{repo}/commits/{requested_ref}");
        let commit_url = public_https_url(&commit_url)?;
        let client = public_https_client(&commit_url).await?;
        let response = client.get(commit_url).send().await.map_err(|error| {
            ApiError::bad_request(format!(
                "resolving GitHub ref: {}",
                public_request_error_kind(&error)
            ))
        })?;
        if !response.status().is_success() {
            return Err(ApiError::bad_request(format!(
                "GitHub ref resolution returned HTTP {}",
                response.status()
            )));
        }
        let document: serde_json::Value = response
            .json()
            .await
            .map_err(|error| ApiError::bad_request(format!("decoding GitHub ref: {error}")))?;
        let commit = document
            .get("sha")
            .and_then(serde_json::Value::as_str)
            .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| ApiError::bad_request("GitHub did not return an immutable commit"))?;
        return reqwest::Url::parse(&format!(
            "https://codeload.github.com/{owner}/{repo}/tar.gz/{commit}"
        ))
        .map_err(|error| ApiError::internal(format!("building GitHub archive URL: {error}")));
    }
    public_https_url(source_ref)
}

async fn inspect_package_source(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Json(input): Json<InspectPackageSourceRequest>,
) -> Result<Json<InspectPackageSourceResponse>, ApiError> {
    if let Some(connection_id) = input.connection_id.as_deref() {
        let repository = input
            .repository
            .as_deref()
            .ok_or_else(|| ApiError::bad_request("repository is required"))?;
        let requested_ref = input
            .requested_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::bad_request("ref is required"))?;
        let (namespace, _) = safe_repository(repository)?;
        let org_slug = organization_slug(&state.pool, who.organization_id).await?;
        let connection = state
            .config
            .package_source_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .ok_or_else(|| ApiError::not_found("package source connection not found"))?;
        let allowed = connection
            .organizations
            .get(&org_slug)
            .is_some_and(|namespaces| {
                namespaces
                    .iter()
                    .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(namespace))
            });
        if !allowed {
            return Err(ApiError::forbidden_message(
                "repository namespace is not allowed for this organization",
            ));
        }
        let (client, resolved) =
            resolve_provider_source(connection, repository, requested_ref).await?;
        #[derive(FromRow)]
        struct ExistingArtifact {
            id: Uuid,
            source_ref: String,
            resolved_commit: String,
            sha256: String,
            size_bytes: i64,
        }
        if let Some(existing) = sqlx::query_as!(ExistingArtifact,
        "SELECT id,source_ref,resolved_commit,sha256,size_bytes FROM package_artifacts \
             WHERE organization_id=$1 AND connection_id=$2 AND repository=$3 AND resolved_commit=$4",
        who.organization_id,
        connection_id,
        repository,
        &resolved.commit)
        .fetch_optional(&state.pool)
        .await?
        {
            return Ok(Json(InspectPackageSourceResponse {
                source_ref: existing.source_ref,
                artifact_id: Some(existing.id),
                resolved_commit: Some(existing.resolved_commit),
                sha256: existing.sha256,
                size_bytes: existing.size_bytes as usize,
            }));
        }
        let bytes = fetch_provider_archive(&client, connection, repository, &resolved).await?;
        let size_bytes = bytes.len();
        use sha2::Digest as _;
        let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
        let object_key = format!("{}/{}.tar.gz", who.organization_id, sha256);
        state
            .package_blob
            .put(&object_key, "application/gzip", &sha256, bytes)
            .await?;
        let id = Uuid::new_v4();
        let inserted = sqlx::query_as!(ExistingArtifact,
        "INSERT INTO package_artifacts \
             (id,organization_id,connection_id,provider,repository,requested_ref,resolved_commit,source_ref,object_key,sha256,size_bytes,created_by) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
             ON CONFLICT (organization_id,connection_id,repository,resolved_commit) DO UPDATE SET resolved_commit=EXCLUDED.resolved_commit \
             RETURNING id,source_ref,resolved_commit,sha256,size_bytes",
        id,
        who.organization_id,
        connection_id,
        &connection.provider,
        repository,
        requested_ref,
        &resolved.commit,
        &resolved.source_ref,
        object_key,
        &sha256,
        size_bytes as i64,
        who.user_id)
        .fetch_one(&state.pool)
        .await?;
        return Ok(Json(InspectPackageSourceResponse {
            source_ref: inserted.source_ref,
            artifact_id: Some(inserted.id),
            resolved_commit: Some(inserted.resolved_commit),
            sha256: inserted.sha256,
            size_bytes: inserted.size_bytes as usize,
        }));
    }

    let source_ref = input
        .source_ref
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("source_ref or connection_id is required"))?;
    let url = resolve_package_source(source_ref).await?;
    public_https_url(url.as_str())?;
    let client = public_https_client(&url).await?;
    let mut response = client.get(url.clone()).send().await.map_err(|error| {
        ApiError::bad_request(format!(
            "fetching package source: {}",
            public_request_error_kind(&error)
        ))
    })?;
    if !response.status().is_success() {
        return Err(ApiError::bad_request(format!(
            "package source returned HTTP {}",
            response.status()
        )));
    }
    const MAX_PACKAGE_BYTES: usize = 100 * 1024 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PACKAGE_BYTES as u64)
    {
        return Err(ApiError::bad_request(
            "package archive exceeds the 100 MiB limit",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ApiError::bad_request(format!("reading package source: {error}")))?
    {
        if bytes.len() + chunk.len() > MAX_PACKAGE_BYTES {
            return Err(ApiError::bad_request(
                "package archive exceeds the 100 MiB limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    use sha2::Digest as _;
    Ok(Json(InspectPackageSourceResponse {
        source_ref: url.to_string(),
        artifact_id: None,
        resolved_commit: None,
        sha256: hex::encode(sha2::Sha256::digest(&bytes)),
        size_bytes: bytes.len(),
    }))
}

async fn download_package_artifact(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row = current_config(&state.pool, who.organization_id).await?;
    let config: gh_service::GovernanceConfig = serde_json::from_value(row.document)
        .map_err(|error| ApiError::internal(format!("decoding stored config: {error}")))?;
    let audiences =
        package_audiences_for_revision(&state.pool, &row.revision, &config.packages).await?;
    let artifact_id = id.to_string();
    let visible = config.packages.iter().any(|package| {
        let references_artifact = package.artifact_id.as_deref() == Some(artifact_id.as_str())
            || package
                .platform_sources
                .values()
                .any(|source| source.artifact_id.as_deref() == Some(artifact_id.as_str()));
        let audience_allows = audiences.get(&package.id).is_none_or(|audience| {
            audience.scope == PackageAudienceScope::Organization
                || audience.user_ids.contains(&who.user_id)
        });
        references_artifact && audience_allows
    });
    if !visible {
        return Err(ApiError::not_found("package artifact not found"));
    }
    let object_key: String = sqlx::query_scalar!(
        "SELECT object_key FROM package_artifacts WHERE id=$1 AND organization_id=$2",
        id,
        who.organization_id
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("package artifact not found"))?;
    let signed = state.package_blob.presign_get(&object_key).await?;
    Ok(Json(json!({
        "url": signed.url,
        "method": signed.method,
        "headers": signed.headers,
        "expires_in_seconds": state.config.blob_presign_ttl_seconds,
    })))
}

#[derive(Deserialize)]
struct UpdateExtensionsRequest {
    base_revision: String,
    packages: Vec<gh_service::ManagedPackage>,
    #[serde(default)]
    package_audiences: BTreeMap<String, PackageAudience>,
    #[serde(default)]
    package_overrides: BTreeMap<String, BTreeMap<String, gh_service::PackageOverride>>,
    #[serde(default)]
    mcp: BTreeMap<String, Vec<gh_service::McpServer>>,
}

#[derive(Deserialize)]
struct UpdateHarnessManagedConfigRequest {
    base_revision: String,
    managed_config_yaml: String,
    #[serde(default)]
    version_requirement: Option<String>,
    #[serde(default)]
    allow_unverified_versions: bool,
}

#[derive(Serialize)]
struct HarnessManagedConfigResponse {
    revision: String,
    harness: String,
    managed_config_yaml: String,
    version_requirement: Option<String>,
    allow_unverified_versions: bool,
}

#[derive(Serialize)]
struct HarnessManagedConfigsResponse {
    revision: String,
    configurations: BTreeMap<String, String>,
    version_requirements: BTreeMap<String, Option<String>>,
    allow_unverified_versions: BTreeMap<String, bool>,
}

fn parse_harness_managed_config_yaml(yaml: &str) -> Result<gh_service::ManagedConfig, ApiError> {
    if yaml.trim().is_empty() {
        return Ok(gh_service::ManagedConfig::default());
    }
    let value: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|error| ApiError::bad_request(format!("invalid managed config YAML: {error}")))?;
    if !value.is_mapping() {
        return Err(ApiError::bad_request(
            "managed config YAML must be a mapping of setting names to values",
        ));
    }
    serde_yaml::from_value(value)
        .map_err(|error| ApiError::bad_request(format!("invalid managed config: {error}")))
}

fn harness_managed_config_response(
    row: ConfigRow,
    harness: &str,
) -> Result<HarnessManagedConfigResponse, ApiError> {
    if !supported_harness(harness) {
        return Err(ApiError::bad_request(format!(
            "unsupported managed-config harness `{harness}`"
        )));
    }
    let config: gh_service::GovernanceConfig = serde_json::from_value(row.document)
        .map_err(|error| ApiError::internal(format!("decoding stored config: {error}")))?;
    let managed_config = config
        .harnesses
        .get(harness)
        .map(|policy| &policy.managed_config)
        .cloned()
        .unwrap_or_default();
    let version_requirement = config
        .harnesses
        .get(harness)
        .and_then(|policy| policy.version_requirement.clone());
    let allow_unverified_versions = config
        .harnesses
        .get(harness)
        .is_some_and(|policy| policy.allow_unverified_versions);
    let managed_config_yaml = serde_yaml::to_string(&managed_config).map_err(|error| {
        ApiError::internal(format!("serializing harness managed config: {error}"))
    })?;
    Ok(HarnessManagedConfigResponse {
        revision: row.revision,
        harness: harness.to_owned(),
        managed_config_yaml,
        version_requirement,
        allow_unverified_versions,
    })
}

fn replace_harness_version_requirement(
    config: &mut gh_service::GovernanceConfig,
    harness: &str,
    requirement: Option<String>,
    allow_unverified_versions: bool,
) -> Result<(), ApiError> {
    let requirement = requirement.filter(|value| !value.trim().is_empty());
    if let Some(value) = requirement.as_deref() {
        semver::VersionReq::parse(value).map_err(|error| {
            ApiError::bad_request(format!(
                "invalid harness version requirement `{value}`: {error}"
            ))
        })?;
        config
            .minimum_client_version
            .get_or_insert_with(|| env!("CARGO_PKG_VERSION").to_owned());
    }
    if allow_unverified_versions && requirement.is_none() {
        return Err(ApiError::bad_request(
            "allow_unverified_versions requires an explicit version_requirement",
        ));
    }
    config
        .harnesses
        .entry(harness.to_owned())
        .or_default()
        .version_requirement = requirement;
    config
        .harnesses
        .entry(harness.to_owned())
        .or_default()
        .allow_unverified_versions = allow_unverified_versions;
    Ok(())
}

fn replace_harness_managed_config(
    config: &mut gh_service::GovernanceConfig,
    harness: &str,
    managed_config: gh_service::ManagedConfig,
) -> Result<(), ApiError> {
    if !supported_harness(harness) {
        return Err(ApiError::bad_request(format!(
            "unsupported managed-config harness `{harness}`"
        )));
    }
    config
        .harnesses
        .entry(harness.to_owned())
        .or_default()
        .managed_config = managed_config;
    Ok(())
}

async fn update_harness_managed_config(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(harness): Path<String>,
    Json(input): Json<UpdateHarnessManagedConfigRequest>,
) -> Result<Json<HarnessManagedConfigResponse>, ApiError> {
    let managed_config = parse_harness_managed_config_yaml(&input.managed_config_yaml)?;
    let current = current_config(&state.pool, who.organization_id).await?;
    if current.revision != input.base_revision {
        return Err(ApiError::conflict(
            "governance configuration changed; reload before saving",
        ));
    }
    let mut config: gh_service::GovernanceConfig = serde_json::from_value(current.document)
        .map_err(|error| ApiError::internal(format!("decoding stored config: {error}")))?;
    replace_harness_managed_config(&mut config, &harness, managed_config)?;
    replace_harness_version_requirement(
        &mut config,
        &harness,
        input.version_requirement,
        input.allow_unverified_versions,
    )?;
    stamp_version_aware_client_floor(&mut config);
    let yaml = serde_yaml::to_string(&config).map_err(|error| {
        ApiError::internal(format!("serializing harness configuration: {error}"))
    })?;
    let row = insert_governance_revision(
        &state.pool,
        who.organization_id,
        Some(who.user_id),
        &yaml,
        "dashboard",
        Some(&input.base_revision),
        None,
    )
    .await?;
    Ok(Json(harness_managed_config_response(row, &harness)?))
}

async fn get_harness_managed_config(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(harness): Path<String>,
) -> Result<Json<HarnessManagedConfigResponse>, ApiError> {
    let row = current_config(&state.pool, who.organization_id).await?;
    Ok(Json(harness_managed_config_response(row, &harness)?))
}

async fn list_harness_managed_configs(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<HarnessManagedConfigsResponse>, ApiError> {
    let row = current_config(&state.pool, who.organization_id).await?;
    let config: gh_service::GovernanceConfig = serde_json::from_value(row.document)
        .map_err(|error| ApiError::internal(format!("decoding stored config: {error}")))?;
    let configurations = gh_config::implementations::HARNESS_DEFINITIONS
        .iter()
        .map(|definition| definition.metadata.key)
        .map(|harness| {
            let managed_config = config
                .harnesses
                .get(harness)
                .map(|policy| &policy.managed_config)
                .cloned()
                .unwrap_or_default();
            serde_yaml::to_string(&managed_config)
                .map(|yaml| (harness.to_owned(), yaml))
                .map_err(|error| {
                    ApiError::internal(format!("serializing harness managed config: {error}"))
                })
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let version_requirements = gh_config::implementations::HARNESS_DEFINITIONS
        .iter()
        .map(|definition| definition.metadata.key)
        .map(|harness| {
            (
                harness.to_owned(),
                config
                    .harnesses
                    .get(harness)
                    .and_then(|policy| policy.version_requirement.clone()),
            )
        })
        .collect();
    let allow_unverified_versions = gh_config::implementations::HARNESS_DEFINITIONS
        .iter()
        .map(|definition| definition.metadata.key)
        .map(|harness| {
            (
                harness.to_owned(),
                config
                    .harnesses
                    .get(harness)
                    .is_some_and(|policy| policy.allow_unverified_versions),
            )
        })
        .collect();
    Ok(Json(HarnessManagedConfigsResponse {
        revision: row.revision,
        configurations,
        version_requirements,
        allow_unverified_versions,
    }))
}

fn validate_mcp_servers(mcp: &BTreeMap<String, Vec<gh_service::McpServer>>) -> Result<(), String> {
    for (harness, servers) in mcp {
        if !supported_harness(harness) {
            return Err(format!("unsupported MCP harness `{harness}`"));
        }
        let mut names = std::collections::BTreeSet::new();
        for server in servers {
            let name = server.name.trim();
            if name.is_empty() {
                return Err(format!("MCP server name is required for {harness}"));
            }
            if !names.insert(name) {
                return Err(format!(
                    "MCP server `{name}` is configured more than once for {harness}"
                ));
            }
            match (&server.command, &server.url) {
                (Some(command), None) if !command.trim().is_empty() => {
                    if server.transport.is_some() {
                        return Err(format!(
                            "local MCP server `{name}` cannot declare a transport"
                        ));
                    }
                }
                (None, Some(url)) if !url.trim().is_empty() => {
                    if !server.args.is_empty() || !server.env.is_empty() {
                        return Err(format!(
                            "remote MCP server `{name}` cannot declare arguments or environment variables"
                        ));
                    }
                    let parsed = reqwest::Url::parse(url)
                        .map_err(|_| format!("MCP server `{name}` has an invalid URL"))?;
                    if !matches!(parsed.scheme(), "http" | "https") {
                        return Err(format!("MCP server `{name}` URL must use http or https"));
                    }
                }
                _ => {
                    return Err(format!(
                        "MCP server `{name}` must declare exactly one non-empty command or URL"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn replace_extensions(
    config: &mut gh_service::GovernanceConfig,
    packages: Vec<gh_service::ManagedPackage>,
    package_overrides: &BTreeMap<String, BTreeMap<String, gh_service::PackageOverride>>,
    mcp: &BTreeMap<String, Vec<gh_service::McpServer>>,
) {
    config.packages = packages;
    for policy in config.harnesses.values_mut() {
        policy.package_overrides.clear();
        policy.mcp.clear();
    }
    for harness in gh_config::implementations::HARNESS_DEFINITIONS
        .iter()
        .map(|definition| definition.metadata.key)
    {
        let overrides = package_overrides.get(harness).cloned().unwrap_or_default();
        let servers = mcp.get(harness).cloned().unwrap_or_default();
        if let Some(policy) = config.harnesses.get_mut(harness) {
            policy.package_overrides = overrides;
            policy.mcp = servers;
        } else if !overrides.is_empty() || !servers.is_empty() {
            let policy = config.harnesses.entry(harness.to_string()).or_default();
            policy.package_overrides = overrides;
            policy.mcp = servers;
        }
    }
}

async fn validate_package_audiences(
    pool: &PgPool,
    organization_id: Uuid,
    packages: &[gh_service::ManagedPackage],
    audiences: &BTreeMap<String, PackageAudience>,
    current_revision: &str,
    current_packages: &[gh_service::ManagedPackage],
) -> Result<(), ApiError> {
    let package_ids = packages
        .iter()
        .map(|package| package.id.as_str())
        .collect::<BTreeSet<_>>();
    let current = package_audiences_for_revision(pool, current_revision, current_packages).await?;
    for (package_id, audience) in audiences {
        if !package_ids.contains(package_id.as_str()) {
            return Err(ApiError::bad_request(format!(
                "audience references unselected package `{package_id}`"
            )));
        }
        match audience.scope {
            PackageAudienceScope::Organization if !audience.user_ids.is_empty() => {
                return Err(ApiError::bad_request(format!(
                    "organization audience for package `{package_id}` cannot include users"
                )));
            }
            PackageAudienceScope::Organization => continue,
            PackageAudienceScope::Users if audience.user_ids.is_empty() => {
                return Err(ApiError::bad_request(format!(
                    "user audience for package `{package_id}` must include at least one user"
                )));
            }
            PackageAudienceScope::Users => {}
        }
        let unique = audience.user_ids.iter().copied().collect::<BTreeSet<_>>();
        if unique.len() != audience.user_ids.len() {
            return Err(ApiError::bad_request(format!(
                "audience for package `{package_id}` contains duplicate users"
            )));
        }
        let rows = sqlx::query!(
            "SELECT id,status FROM users WHERE organization_id=$1 AND id=ANY($2)",
            organization_id,
            &audience.user_ids
        )
        .fetch_all(pool)
        .await?;
        if rows.len() != audience.user_ids.len() {
            return Err(ApiError::bad_request(format!(
                "audience for package `{package_id}` contains a user outside this organization"
            )));
        }
        let retained = current
            .get(package_id)
            .filter(|item| item.scope == PackageAudienceScope::Users)
            .map(|item| item.user_ids.iter().copied().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        if let Some(row) = rows
            .iter()
            .find(|row| row.status != "active" && !retained.contains(&row.id))
        {
            let user_id = row.id;
            return Err(ApiError::bad_request(format!(
                "inactive user `{user_id}` cannot be newly added to package `{package_id}`"
            )));
        }
    }
    Ok(())
}

fn stamp_version_aware_client_floor(config: &mut gh_service::GovernanceConfig) {
    let required = config
        .harnesses
        .values()
        .any(|policy| policy.version_requirement.is_some() || policy.allow_unverified_versions)
        || config.packages.iter().any(|package| {
            package
                .adapters
                .values()
                .any(|adapter| !adapter.variants.is_empty())
        });
    if required {
        config.contract_version = gh_service::GovernanceConfig::CONTRACT_VERSION;
        for capability in [
            "adapter_intervals",
            "compiled_harness_registry",
            "transactional_reconcile",
            "versioned_state",
        ] {
            if !config
                .required_capabilities
                .iter()
                .any(|value| value == capability)
            {
                config.required_capabilities.push(capability.to_owned());
            }
        }
        if config
            .harnesses
            .values()
            .any(|policy| policy.allow_unverified_versions)
            && !config
                .required_capabilities
                .iter()
                .any(|value| value == "unverified_harness_versions")
        {
            config
                .required_capabilities
                .push("unverified_harness_versions".to_owned());
        }
        config
            .minimum_client_version
            .get_or_insert_with(|| env!("CARGO_PKG_VERSION").to_owned());
    }
    if config.gateway.is_some() {
        config.contract_version = gh_service::GovernanceConfig::CONTRACT_VERSION;
        if !config
            .required_capabilities
            .iter()
            .any(|capability| capability == "gateway_inference_jwt")
        {
            config
                .required_capabilities
                .push("gateway_inference_jwt".to_owned());
        }
    }
}

async fn update_governance_extensions(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Json(input): Json<UpdateExtensionsRequest>,
) -> Result<Json<AdminConfigResponse>, ApiError> {
    validate_packages(&input.packages).map_err(ApiError::bad_request)?;
    validate_package_artifacts(&state.pool, who.organization_id, &input.packages).await?;
    validate_mcp_servers(&input.mcp).map_err(ApiError::bad_request)?;
    let package_ids = input
        .packages
        .iter()
        .map(|package| package.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for (harness, overrides) in &input.package_overrides {
        if !supported_harness(harness) {
            return Err(ApiError::bad_request(format!(
                "unsupported package override harness `{harness}`"
            )));
        }
        for package_id in overrides.keys() {
            if !package_ids.contains(package_id.as_str()) {
                return Err(ApiError::bad_request(format!(
                    "override references unselected package `{package_id}`"
                )));
            }
        }
    }
    let current = current_config(&state.pool, who.organization_id).await?;
    if current.revision != input.base_revision {
        return Err(ApiError::conflict(
            "governance configuration changed; reload before saving",
        ));
    }
    let mut config: gh_service::GovernanceConfig = serde_json::from_value(current.document)
        .map_err(|error| ApiError::internal(format!("decoding stored config: {error}")))?;
    validate_package_audiences(
        &state.pool,
        who.organization_id,
        &input.packages,
        &input.package_audiences,
        &current.revision,
        &config.packages,
    )
    .await?;
    replace_extensions(
        &mut config,
        input.packages,
        &input.package_overrides,
        &input.mcp,
    );
    stamp_version_aware_client_floor(&mut config);
    let yaml = serde_yaml::to_string(&config).map_err(|error| {
        ApiError::internal(format!("serializing extension configuration: {error}"))
    })?;
    let row = insert_governance_revision(
        &state.pool,
        who.organization_id,
        Some(who.user_id),
        &yaml,
        "dashboard",
        Some(&input.base_revision),
        Some(&input.package_audiences),
    )
    .await?;
    Ok(Json(admin_config_response(&state.pool, row).await?))
}

async fn validate_package_artifacts(
    pool: &PgPool,
    organization_id: Uuid,
    packages: &[gh_service::ManagedPackage],
) -> Result<(), ApiError> {
    for package in packages {
        let sources = std::iter::once((package.artifact_id.as_deref(), package.sha256.as_str()))
            .chain(
                package
                    .platform_sources
                    .values()
                    .map(|source| (source.artifact_id.as_deref(), source.sha256.as_str())),
            );
        for (artifact_id, sha256) in sources {
            let Some(artifact_id) = artifact_id else {
                continue;
            };
            let id = Uuid::parse_str(artifact_id)
                .map_err(|_| ApiError::bad_request("package artifact id is invalid"))?;
            let matches: bool = sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM package_artifacts WHERE id=$1 AND organization_id=$2 AND sha256=$3) as \"exists!\"",
        id,
        organization_id,
        sha256)
            .fetch_one(pool)
            .await?;
            if !matches {
                return Err(ApiError::bad_request(format!(
                    "package `{}` references an unavailable artifact or mismatched digest",
                    package.id
                )));
            }
        }
    }
    Ok(())
}

#[derive(FromRow)]
struct RevisionRow {
    revision: String,
    created_at: OffsetDateTime,
    created_by_email: Option<String>,
    origin: String,
}

async fn governance_revisions(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let rows = sqlx::query_as!(
        RevisionRow,
        "SELECT r.revision,r.created_at,u.email AS \"created_by_email?\",r.origin \
         FROM governance_config_revisions r LEFT JOIN users u ON u.id=r.created_by \
         WHERE r.organization_id=$1 ORDER BY r.id DESC LIMIT 100",
        who.organization_id,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| {
                json!({"revision":row.revision,"created_at":now_text(row.created_at),"created_by_email":row.created_by_email,"origin":row.origin})
            })
            .collect(),
    ))
}

#[derive(FromRow, Clone)]
struct UserRow {
    id: Uuid,
    subject: String,
    email: String,
    role: String,
    active: bool,
    status: String,
    protected: bool,
    provisioning_source: String,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}
fn user_json(row: UserRow) -> serde_json::Value {
    json!({
        "id":row.id,"subject":row.subject,"email":row.email,"role":row.role,
        "active":row.active,"status":row.status,"protected":row.protected,
        "provisioning_source":row.provisioning_source,"managed":row.provisioning_source == "scim",
        "created_at":now_text(row.created_at),"updated_at":now_text(row.updated_at)
    })
}

#[derive(Default, Deserialize)]
struct UserListQuery {
    page: Option<i64>,
    per_page: Option<i64>,
    q: Option<String>,
    role: Option<String>,
    status: Option<String>,
    provisioning_source: Option<String>,
}

async fn list_users(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Query(query): Query<UserListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_role_filter(query.role.as_deref())?;
    validate_status_filter(query.status.as_deref())?;
    validate_provisioning_source_filter(query.provisioning_source.as_deref())?;
    let (page, per_page, offset) = validate_page(query.page, query.per_page)?;
    let search = like_filter("q", query.q.as_deref())?;
    let total = sqlx::query_scalar!(
        "SELECT count(*) as \"count!\" FROM users WHERE organization_id=$1 \
         AND ($2::text IS NULL OR lower(email) LIKE $2 ESCAPE E'\\\\' OR lower(subject) LIKE $2 ESCAPE E'\\\\') \
         AND ($3::text IS NULL OR role=$3) AND ($4::text IS NULL OR status=$4) \
         AND ($5::text IS NULL OR provisioning_source=$5)",
        who.organization_id,
        search.as_deref(),
        query.role.as_deref(),
        query.status.as_deref(),
        query.provisioning_source.as_deref()
    )
    .fetch_one(&state.pool)
    .await?;
    let rows = sqlx::query_as!(UserRow,
        "SELECT id,subject,email,role,active,status,protected,provisioning_source,created_at,updated_at FROM users \
         WHERE organization_id=$1 AND ($2::text IS NULL OR lower(email) LIKE $2 ESCAPE E'\\\\' OR lower(subject) LIKE $2 ESCAPE E'\\\\') \
         AND ($3::text IS NULL OR role=$3) AND ($4::text IS NULL OR status=$4) \
         AND ($5::text IS NULL OR provisioning_source=$5) \
         ORDER BY lower(email) LIMIT $6 OFFSET $7",
        who.organization_id,
        search.as_deref(),
        query.role.as_deref(),
        query.status.as_deref(),
        query.provisioning_source.as_deref(),
        per_page,
        offset)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.into_iter().map(user_json).collect::<Vec<_>>(),
        "page": page,
        "per_page": per_page,
        "total": total,
        "total_pages": page_count(total, per_page),
    })))
}

async fn list_user_options(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let rows = sqlx::query_as!(UserRow,
        "SELECT id,subject,email,role,active,status,protected,provisioning_source,created_at,updated_at \
         FROM users WHERE organization_id=$1 AND status != 'removed' ORDER BY lower(email)",
        who.organization_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows.into_iter().map(user_json).collect()))
}

#[derive(Default, Deserialize)]
struct UserOptionSearchRequest {
    #[serde(default)]
    q: String,
    #[serde(default)]
    selected_user_ids: Vec<Uuid>,
}

async fn search_user_options(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Json(input): Json<UserOptionSearchRequest>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let query = input.q.trim().to_lowercase();
    let rows = sqlx::query_as!(UserRow,
        "SELECT id,subject,email,role,active,status,protected,provisioning_source,created_at,updated_at \
         FROM users WHERE organization_id=$1 AND status != 'removed' \
         AND (status='active' OR id=ANY($2)) \
         AND ($3='' OR strpos(lower(email),$3)>0) \
         ORDER BY lower(email) LIMIT 10",
        who.organization_id,
        &input.selected_user_ids,
        query)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows.into_iter().map(user_json).collect()))
}

fn validate_page(
    requested_page: Option<i64>,
    requested_per_page: Option<i64>,
) -> Result<(i64, i64, i64), ApiError> {
    let page = requested_page.unwrap_or(1);
    let per_page = requested_per_page.unwrap_or(25);
    if page < 1 {
        return Err(ApiError::bad_request("page must be at least 1"));
    }
    if !(1..=100).contains(&per_page) {
        return Err(ApiError::bad_request("per_page must be between 1 and 100"));
    }
    let offset = (page - 1)
        .checked_mul(per_page)
        .ok_or_else(|| ApiError::bad_request("page is too large"))?;
    Ok((page, per_page, offset))
}

fn validate_role_filter(role: Option<&str>) -> Result<(), ApiError> {
    if role.is_some_and(|value| value != "admin" && value != "member") {
        Err(ApiError::bad_request("role must be admin or member"))
    } else {
        Ok(())
    }
}

fn validate_status_filter(status: Option<&str>) -> Result<(), ApiError> {
    if status.is_some_and(|value| !["active", "suspended", "removed"].contains(&value)) {
        Err(ApiError::bad_request(
            "status must be active, suspended, or removed",
        ))
    } else {
        Ok(())
    }
}

fn validate_provisioning_source_filter(source: Option<&str>) -> Result<(), ApiError> {
    if source.is_some_and(|value| value != "local" && value != "scim") {
        Err(ApiError::bad_request(
            "provisioning_source must be local or scim",
        ))
    } else {
        Ok(())
    }
}

async fn scoped_user(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<UserRow, ApiError> {
    sqlx::query_as!(UserRow,
        "SELECT id,subject,email,role,active,status,protected,provisioning_source,created_at,updated_at \
         FROM users WHERE organization_id=$1 AND id=$2",
        org_id,
        id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::not_found("user not found"))
}

async fn get_user(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(user_json(
        scoped_user(&state.pool, who.organization_id, id).await?,
    )))
}

#[derive(Deserialize)]
struct UpdateUserRequest {
    role: Option<String>,
    status: Option<String>,
}

async fn revoke_credentials(
    connection: &mut PgConnection,
    user_id: Uuid,
    subject: &str,
) -> Result<(), ApiError> {
    sqlx::query!("delete from auth.\"session\" where \"userId\"=$1", subject)
        .execute(&mut *connection)
        .await?;
    sqlx::query!(
        "delete from auth.\"oauthAccessToken\" where \"userId\"=$1",
        subject
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query!(
        "delete from auth.\"oauthRefreshToken\" where \"userId\"=$1",
        subject
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query!(
        "delete from auth.\"deviceCode\" where \"userId\"=$1",
        subject
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query!(
        "update login_credentials set revoked_at=coalesce(revoked_at,now()) where user_id=$1",
        user_id
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query!(
        "update api_sessions set revoked_at=coalesce(revoked_at,now()) where user_id=$1",
        user_id
    )
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn revoke_gateway_key(state: &AppState, key: Option<String>) {
    let Some(key) = key else { return };
    let _ = sqlx::query!("insert into public.gateway_key_revocations(id,external_id) values($1,$2) on conflict(external_id) do nothing",
        Uuid::new_v4(),
        &key).execute(&state.pool).await;
    process_gateway_revocation(state, &key).await;
}

async fn process_gateway_revocation(state: &AppState, key: &str) {
    let Ok(Some((_, _provisioner))) = managed_gateway_settings(&state.config) else {
        return;
    };
    let request = gh_gateway_provisioner::RevokeRequest {
        identity: gh_gateway_provisioner::UserIdentity {
            id: String::new(),
            email: String::new(),
            organization_id: String::new(),
            groups: Vec::new(),
        },
        external_id: key.to_owned(),
    };
    let Some(implementation) = &state.gateway_provisioner else {
        return;
    };
    match implementation.revoke(request).await {
        Ok(_) | Err(ProvisionerError::CredentialInvalid(_)) => {
            let _ = sqlx::query!("update public.gateway_key_revocations set completed_at=now(),last_error=null where external_id=$1",
        key).execute(&state.pool).await;
        }
        Err(provisioner_error) => {
            let error = provisioner_api_error(provisioner_error);
            tracing::warn!(%error, "failed to revoke removed user's governance proxy key");
            let _ = sqlx::query!("update public.gateway_key_revocations set attempts=attempts+1,last_error=$2,next_attempt_at=now() + make_interval(secs => least(3600,30 * (1 << least(attempts,7)))) where external_id=$1",
        key,
        error.to_string()).execute(&state.pool).await;
        }
    }
}

async fn process_gateway_revocations(state: Arc<AppState>) {
    loop {
        match sqlx::query_scalar!("select external_id from public.gateway_key_revocations where completed_at is null and next_attempt_at<=now() order by next_attempt_at limit 100").fetch_all(&state.pool).await {
            Ok(keys) => {
                for key in keys { process_gateway_revocation(&state, &key).await; }
                record_worker_success();
            },
            Err(error) => tracing::warn!(%error, "failed to load gateway revocation outbox"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

async fn update_user(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateUserRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if input.role.is_none() && input.status.is_none() {
        return Err(ApiError::bad_request("role or status is required"));
    }
    validate_role_filter(input.role.as_deref())?;
    if input
        .status
        .as_deref()
        .is_some_and(|value| value != "active" && value != "suspended")
    {
        return Err(ApiError::bad_request("status must be active or suspended"));
    }
    let target = scoped_user(&state.pool, who.organization_id, id).await?;
    if target.status == "removed" {
        return Err(ApiError::conflict("removed users must be invited again"));
    }
    if target.id == who.user_id {
        return Err(ApiError::conflict(
            "administrators cannot change their own role or status",
        ));
    }
    if target.protected {
        return Err(ApiError::conflict("bootstrap administrator is protected"));
    }
    if target.provisioning_source == "scim" {
        return Err(ApiError::conflict(
            "SCIM-managed users must be changed through the identity provider",
        ));
    }
    let next_role = input.role.as_deref().unwrap_or(&target.role);
    let next_status = input.status.as_deref().unwrap_or(&target.status);
    if target.role == "admin"
        && target.status == "active"
        && (next_role != "admin" || next_status != "active")
    {
        let active_admins: i64 = sqlx::query_scalar!("select count(*) as \"count!\" from users where organization_id=$1 and role='admin' and status='active'",
        who.organization_id)
        .fetch_one(&state.pool)
        .await?;
        if active_admins <= 1 {
            return Err(ApiError::conflict(
                "organization must retain an active administrator",
            ));
        }
    }
    let gateway_key = if next_status == "suspended" {
        sqlx::query_scalar!(
            "select source_key_hash from public.gateway_key_selections where user_id=$1",
            id
        )
        .fetch_optional(&state.pool)
        .await?
        .flatten()
    } else {
        None
    };
    let mut transaction = state.pool.begin().await?;
    revoke_credentials(&mut transaction, id, &target.subject).await?;
    sqlx::query!("update users set role=$1,status=$2,active=($2='active'),tokens_valid_after=now(),updated_at=now() where id=$3",
        next_role,
        next_status,
        id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query!("update auth.\"user\" set \"governanceRole\"=$1,banned=$2,\"banReason\"=$3,\"banExpires\"=null,\"updatedAt\"=now() where id=$4",
        next_role,
        next_status == "suspended",
        if next_status == "suspended" {
        Some("Suspended by organization administrator")
    } else {
        None
    },
        &target.subject)
    .execute(&mut *transaction)
    .await?;
    sqlx::query!(
        "update auth.\"member\" m set role=$1 from auth.\"organization\" ao,organizations po \
         where m.\"organizationId\"=ao.id and ao.slug=po.slug and po.id=$2 and m.\"userId\"=$3",
        next_role,
        who.organization_id,
        &target.subject
    )
    .execute(&mut *transaction)
    .await?;
    if next_status == "suspended" {
        sqlx::query!(
            "delete from public.gateway_key_selections where user_id=$1",
            id
        )
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    revoke_gateway_key(&state, gateway_key).await;
    Ok(Json(user_json(
        scoped_user(&state.pool, who.organization_id, id).await?,
    )))
}

async fn revoke_user_sessions(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let target = scoped_user(&state.pool, who.organization_id, id).await?;
    if target.status == "removed" {
        return Err(ApiError::conflict("user has no active credentials"));
    }
    let mut transaction = state.pool.begin().await?;
    revoke_credentials(&mut transaction, id, &target.subject).await?;
    sqlx::query!(
        "update users set tokens_valid_after=now(),updated_at=now() where id=$1",
        id
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_user(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let target = scoped_user(&state.pool, who.organization_id, id).await?;
    if target.status == "removed" {
        return Ok(StatusCode::NO_CONTENT);
    }
    if target.id == who.user_id {
        return Err(ApiError::conflict(
            "administrators cannot delete their own account",
        ));
    }
    if target.protected {
        return Err(ApiError::conflict("bootstrap administrator is protected"));
    }
    if target.provisioning_source == "scim" {
        return Err(ApiError::conflict(
            "SCIM-managed users must be changed through the identity provider",
        ));
    }
    if target.role == "admin" && target.status == "active" {
        let active_admins: i64 = sqlx::query_scalar!("select count(*) as \"count!\" from users where organization_id=$1 and role='admin' and status='active'",
        who.organization_id)
        .fetch_one(&state.pool)
        .await?;
        if active_admins <= 1 {
            return Err(ApiError::conflict(
                "organization must retain an active administrator",
            ));
        }
    }
    let gateway_key = sqlx::query_scalar!(
        "select source_key_hash from public.gateway_key_selections where user_id=$1",
        id
    )
    .fetch_optional(&state.pool)
    .await?
    .flatten();
    let mut transaction = state.pool.begin().await?;
    revoke_credentials(&mut transaction, id, &target.subject).await?;
    sqlx::query!("update auth.\"invitation\" set \"inviterId\"=$1 where \"inviterId\"=$2 and status='pending'",
        &who.subject,
        &target.subject)
    .execute(&mut *transaction)
    .await?;
    sqlx::query!("delete from auth.\"user\" where id=$1", &target.subject)
        .execute(&mut *transaction)
        .await?;
    sqlx::query!(
        "delete from public.gateway_key_selections where user_id=$1",
        id
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query!("update users set status='removed',active=false,tokens_valid_after=now(),updated_at=now() where id=$1",
        id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    revoke_gateway_key(&state, gateway_key).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(FromRow)]
struct InvitationRow {
    id: String,
    email: String,
    role: Option<String>,
    status: String,
    expires_at: OffsetDateTime,
    created_at: OffsetDateTime,
    inviter_email: Option<String>,
}

fn invitation_json(row: InvitationRow) -> serde_json::Value {
    let status = if row.status == "pending" && row.expires_at <= OffsetDateTime::now_utc() {
        "expired"
    } else {
        row.status.as_str()
    };
    json!({
        "id":row.id,"email":row.email,"role":row.role.unwrap_or_else(|| "member".into()),
        "status":status,"expires_at":now_text(row.expires_at),"created_at":now_text(row.created_at),
        "inviter_email":row.inviter_email
    })
}

async fn auth_org_id(pool: &PgPool, org_id: Uuid) -> Result<String, ApiError> {
    sqlx::query_scalar!("select ao.id from auth.\"organization\" ao join organizations po on po.slug=ao.slug where po.id=$1",
        org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::internal("organization is not initialized in Better Auth"))
}

async fn invitation_row(pool: &PgPool, org_id: Uuid, id: &str) -> Result<InvitationRow, ApiError> {
    let auth_org = auth_org_id(pool, org_id).await?;
    sqlx::query_as!(InvitationRow,
        "select i.id,i.email,i.role,i.status,i.\"expiresAt\" as expires_at,i.\"createdAt\" as created_at,u.email as inviter_email \
         from auth.\"invitation\" i left join auth.\"user\" u on u.id=i.\"inviterId\" \
         where i.\"organizationId\"=$1 and i.id=$2",
        auth_org,
        id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::not_found("invitation not found"))
}

#[derive(Default, Deserialize)]
struct InvitationListQuery {
    page: Option<i64>,
    per_page: Option<i64>,
    status: Option<String>,
    q: Option<String>,
    role: Option<String>,
}

async fn list_invitations(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Query(query): Query<InvitationListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if query
        .status
        .as_deref()
        .is_some_and(|value| value != "outstanding")
    {
        return Err(ApiError::bad_request("status must be outstanding"));
    }
    validate_role_filter(query.role.as_deref())?;
    let search = like_filter("q", query.q.as_deref())?;
    let (page, per_page, offset) = validate_page(query.page, query.per_page)?;
    let auth_org = auth_org_id(&state.pool, who.organization_id).await?;
    let outstanding = query.status.as_deref() == Some("outstanding");
    let total = sqlx::query_scalar!(
        "select count(*) as \"count!\" from auth.\"invitation\" i \
         where i.\"organizationId\"=$1 and ($2::bool=false or i.status='pending') \
         and ($3::text is null or lower(i.email) like $3 escape E'\\\\') \
         and ($4::text is null or i.role=$4)",
        &auth_org,
        outstanding,
        search.as_deref(),
        query.role.as_deref()
    )
    .fetch_one(&state.pool)
    .await?;
    let rows = sqlx::query_as!(InvitationRow,
        "select i.id,i.email,i.role,i.status,i.\"expiresAt\" as expires_at,i.\"createdAt\" as created_at,u.email as inviter_email \
         from auth.\"invitation\" i left join auth.\"user\" u on u.id=i.\"inviterId\" \
         where i.\"organizationId\"=$1 and ($2::bool=false or i.status='pending') \
         and ($3::text is null or lower(i.email) like $3 escape E'\\\\') \
         and ($4::text is null or i.role=$4) \
         order by i.\"createdAt\" desc limit $5 offset $6",
        auth_org,
        outstanding,
        search.as_deref(),
        query.role.as_deref(),
        per_page,
        offset)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.into_iter().map(invitation_json).collect::<Vec<_>>(),
        "page": page,
        "per_page": per_page,
        "total": total,
        "total_pages": page_count(total, per_page),
    })))
}

async fn get_invitation(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(invitation_json(
        invitation_row(&state.pool, who.organization_id, &id).await?,
    )))
}

#[derive(Deserialize)]
struct CreateInvitationRequest {
    email: String,
    role: String,
}

fn normalize_email(email: &str) -> Result<String, ApiError> {
    let email = email.trim().to_lowercase();
    let valid = !email.is_empty()
        && !email.chars().any(char::is_whitespace)
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    if valid {
        Ok(email)
    } else {
        Err(ApiError::bad_request("a valid email is required"))
    }
}

fn log_invitation(state: &AppState, id: &str, email: &str) {
    let url = format!(
        "{}/accept-invitation?id={id}",
        state.config.auth_public_url.trim_end_matches('/')
    );
    tracing::info!(%email, %url, "Blue invitation");
}

async fn create_invitation(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Json(input): Json<CreateInvitationRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if state.config.auth_mode == "oidc" {
        return Err(ApiError::conflict(
            "invitations are disabled for identity-provider-managed workspaces",
        ));
    }
    validate_role_filter(Some(&input.role))?;
    let email = normalize_email(&input.email)?;
    let auth_org = auth_org_id(&state.pool, who.organization_id).await?;
    let member_exists: bool = sqlx::query_scalar!(
        "select exists(select 1 from auth.\"member\" m join auth.\"user\" u on u.id=m.\"userId\" \
         where m.\"organizationId\"=$1 and lower(u.email)=lower($2)) as \"exists!\"",
        &auth_org,
        &email
    )
    .fetch_one(&state.pool)
    .await?;
    if member_exists {
        return Err(ApiError::conflict(
            "email is already an organization member",
        ));
    }
    let mut transaction = state.pool.begin().await?;
    sqlx::query!("update auth.\"invitation\" set status='canceled' where \"organizationId\"=$1 and lower(email)=lower($2) \
         and status='pending' and \"expiresAt\" <= now()",
        &auth_org,
        &email)
    .execute(&mut *transaction)
    .await?;
    let pending: bool = sqlx::query_scalar!("select exists(select 1 from auth.\"invitation\" where \"organizationId\"=$1 and lower(email)=lower($2) and status='pending') as \"exists!\"",
        &auth_org,
        &email)
    .fetch_one(&mut *transaction)
    .await?;
    if pending {
        return Err(ApiError::conflict("email already has a pending invitation"));
    }
    let id = Uuid::new_v4().to_string();
    sqlx::query!("insert into auth.\"invitation\" (id,\"organizationId\",email,role,status,\"expiresAt\",\"createdAt\",\"inviterId\") \
         values ($1,$2,$3,$4,'pending',now()+interval '24 hours',now(),$5)",
        &id,
        &auth_org,
        &email,
        &input.role,
        &who.subject)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    log_invitation(&state, &id, &email);
    Ok((
        StatusCode::CREATED,
        Json(invitation_json(
            invitation_row(&state.pool, who.organization_id, &id).await?,
        )),
    ))
}

async fn resend_invitation(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let invitation = invitation_row(&state.pool, who.organization_id, &id).await?;
    if invitation.status != "pending" {
        return Err(ApiError::conflict(
            "only pending or expired invitations can be resent",
        ));
    }
    sqlx::query!(
        "update auth.\"invitation\" set \"expiresAt\"=now()+interval '24 hours' where id=$1",
        &id
    )
    .execute(&state.pool)
    .await?;
    log_invitation(&state, &id, &invitation.email);
    Ok(Json(invitation_json(
        invitation_row(&state.pool, who.organization_id, &id).await?,
    )))
}

async fn cancel_invitation(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let invitation = invitation_row(&state.pool, who.organization_id, &id).await?;
    if invitation.status == "canceled" {
        return Ok(StatusCode::NO_CONTENT);
    }
    if invitation.status != "pending" {
        return Err(ApiError::conflict(
            "only pending invitations can be canceled",
        ));
    }
    sqlx::query!(
        "update auth.\"invitation\" set status='canceled' where id=$1",
        id
    )
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PresignRequest {
    harness: String,
    #[serde(default = "legacy_compatibility_profile")]
    compatibility_profile: String,
    session_id: String,
    sha256: String,
    size_bytes: i64,
    content_type: String,
    cwd: Option<String>,
    #[serde(default = "legacy_artifact_format")]
    artifact_format: String,
    #[serde(default)]
    resumable: bool,
    title: Option<String>,
    summary: Option<String>,
    #[serde(default)]
    captured_at_unix_ms: u128,
    repository_root: Option<String>,
    repository_remote: Option<String>,
}
fn legacy_artifact_format() -> String {
    "legacy-raw".into()
}
fn legacy_compatibility_profile() -> String {
    "legacy".to_owned()
}
fn upload_profile_is_known(harness: &str, profile: &str) -> bool {
    if profile == "legacy" {
        return supported_harness(harness);
    }
    harness
        .parse::<gh_common::Harness>()
        .ok()
        .is_some_and(|harness| {
            gh_config::implementations::implementation_for_profile(harness, profile).is_some()
        })
}
#[derive(Serialize)]
struct PresignResponse {
    upload_id: Uuid,
    status: String,
    upload_url: Option<String>,
    method: Option<String>,
    headers: BTreeMap<String, String>,
    complete_url: Option<String>,
}

async fn presign_upload(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Json(input): Json<PresignRequest>,
) -> Result<Json<PresignResponse>, ApiError> {
    if !supported_harness(&input.harness) {
        return Err(ApiError::bad_request("unknown harness"));
    }
    if !upload_profile_is_known(&input.harness, &input.compatibility_profile) {
        return Err(ApiError::bad_request(
            "unknown compatibility profile for harness",
        ));
    }
    if input.size_bytes < 0
        || input.size_bytes > gh_config::session_bundle::MAX_BUNDLE_BYTES as i64
        || input.sha256.len() != 64
        || !input.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ApiError::bad_request("invalid size or sha256"));
    }
    let bounded = |name: &str, value: Option<&str>, max: usize| -> Result<(), ApiError> {
        if value.is_some_and(|value| value.chars().count() > max) {
            Err(ApiError::bad_request(format!("{name} is too long")))
        } else {
            Ok(())
        }
    };
    bounded("session_id", Some(&input.session_id), 512)?;
    if input.session_id.is_empty() {
        return Err(ApiError::bad_request("session_id must not be empty"));
    }
    bounded("content_type", Some(&input.content_type), 256)?;
    bounded("cwd", input.cwd.as_deref(), 4096)?;
    bounded("title", input.title.as_deref(), 256)?;
    bounded("summary", input.summary.as_deref(), 1024)?;
    bounded("repository_root", input.repository_root.as_deref(), 4096)?;
    bounded(
        "repository_remote",
        input.repository_remote.as_deref(),
        4096,
    )?;
    if input.artifact_format != "legacy-raw"
        && input.artifact_format != gh_config::session_bundle::ARTIFACT_FORMAT
    {
        return Err(ApiError::bad_request("unsupported artifact_format"));
    }
    if input.resumable
        && (input.artifact_format != gh_config::session_bundle::ARTIFACT_FORMAT
            || input.compatibility_profile == "legacy")
    {
        return Err(ApiError::bad_request(
            "resumable uploads require a supported bundle format and compatibility profile",
        ));
    }
    if input.resumable {
        let harness: gh_common::Harness = input
            .harness
            .parse()
            .map_err(|_| ApiError::bad_request("unknown harness"))?;
        let supported = gh_config::implementations::implementation_for_profile(
            harness,
            &input.compatibility_profile,
        )
        .is_some_and(|registration| {
            matches!(
                registration.implementation.session_resume_capability(),
                gh_config::implementations::Feature::Supported(())
            )
        });
        if !supported {
            return Err(ApiError::bad_request(
                "compatibility profile does not support session resume",
            ));
        }
    }
    let source_captured_at = (input.captured_at_unix_ms > 0)
        .then(|| {
            i128::try_from(input.captured_at_unix_ms)
                .ok()
                .and_then(|value| value.checked_mul(1_000_000))
                .and_then(|value| OffsetDateTime::from_unix_timestamp_nanos(value).ok())
        })
        .flatten();
    let captured_id: Uuid = sqlx::query_scalar!("INSERT INTO captured_sessions (id,organization_id,user_id,harness,compatibility_profile,native_session_id,cwd,artifact_format,resumable,title,summary,source_captured_at,repository_root,repository_remote) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) ON CONFLICT (user_id,harness,compatibility_profile,native_session_id) \
         DO UPDATE SET cwd=EXCLUDED.cwd,artifact_format=EXCLUDED.artifact_format,resumable=EXCLUDED.resumable,title=EXCLUDED.title,summary=EXCLUDED.summary,source_captured_at=EXCLUDED.source_captured_at,repository_root=EXCLUDED.repository_root,repository_remote=EXCLUDED.repository_remote,updated_at=now() RETURNING id AS \"id!\"",
        Uuid::new_v4(), who.organization_id, who.user_id,
        &input.harness, &input.compatibility_profile, &input.session_id,
        input.cwd.as_deref(), &input.artifact_format, input.resumable,
        input.title.as_deref(), input.summary.as_deref(), source_captured_at,
        input.repository_root.as_deref(), input.repository_remote.as_deref())
    .fetch_one(&state.pool)
    .await?;
    #[derive(FromRow)]
    struct ExistingArtifact {
        id: Uuid,
        status: String,
        object_key: String,
    }
    if let Some(existing) = sqlx::query_as!(ExistingArtifact,
        "SELECT id,status,object_key FROM session_artifacts WHERE captured_session_id=$1 AND sha256=$2",
        captured_id,
        &input.sha256)
    .fetch_optional(&state.pool)
    .await?
    {
        if existing.status == "complete" {
            return Ok(Json(PresignResponse {
                upload_id: existing.id,
                status: existing.status,
                upload_url: None,
                method: None,
                headers: BTreeMap::new(),
                complete_url: None,
            }));
        }
        sqlx::query!("UPDATE session_artifacts SET status='pending', size_bytes=$2, content_type=$3, \
             upload_expires_at=now()+interval '15 minutes', failure_reason=NULL WHERE id=$1",
        existing.id,
        input.size_bytes,
        &input.content_type)
        .execute(&state.pool)
        .await?;
        let signed = state
            .blob
            .presign_put(&existing.object_key, &input.content_type, &input.sha256)
            .await?;
        return Ok(Json(PresignResponse {
            upload_id: existing.id,
            status: "pending".into(),
            upload_url: Some(signed.url),
            method: Some(signed.method),
            headers: signed.headers,
            complete_url: Some(format!("/session-uploads/{}/complete", existing.id)),
        }));
    }
    let upload_id = Uuid::new_v4();
    let extension = if input.artifact_format == gh_config::session_bundle::ARTIFACT_FORMAT {
        "tgz"
    } else if input.content_type.contains("ndjson") {
        "jsonl"
    } else {
        "json"
    };
    let object_key = format!(
        "{}/{}/{upload_id}.{extension}",
        who.organization_id, captured_id
    );
    sqlx::query!("INSERT INTO session_artifacts \
         (id,captured_session_id,object_key,sha256,size_bytes,content_type,status,upload_expires_at) \
         VALUES ($1,$2,$3,$4,$5,$6,'pending',now()+interval '15 minutes')",
        upload_id,
        captured_id,
        &object_key,
        &input.sha256,
        input.size_bytes,
        &input.content_type)
    .execute(&state.pool)
    .await?;
    let signed = state
        .blob
        .presign_put(&object_key, &input.content_type, &input.sha256)
        .await?;
    Ok(Json(PresignResponse {
        upload_id,
        status: "pending".into(),
        upload_url: Some(signed.url),
        method: Some(signed.method),
        headers: signed.headers,
        complete_url: Some(format!("/session-uploads/{upload_id}/complete")),
    }))
}

#[derive(FromRow)]
struct ArtifactOwner {
    captured_session_id: Uuid,
    object_key: String,
    sha256: String,
    size_bytes: i64,
    status: String,
    upload_expires_at: OffsetDateTime,
    user_id: Uuid,
    organization_id: Uuid,
}
async fn artifact_owner(state: &AppState, id: Uuid) -> Result<ArtifactOwner, ApiError> {
    sqlx::query_as!(ArtifactOwner,
        "SELECT a.captured_session_id,a.object_key,a.sha256,a.size_bytes,a.status,a.upload_expires_at,s.user_id,s.organization_id \
         FROM session_artifacts a JOIN captured_sessions s ON s.id=a.captured_session_id WHERE a.id=$1",
        id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("upload not found"))
}
fn may_access(who: &Principal, user_id: Uuid, org_id: Uuid) -> Result<(), ApiError> {
    if who.organization_id != org_id || (who.role != "admin" && who.user_id != user_id) {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "session access denied",
        ))
    } else {
        Ok(())
    }
}

async fn may_view_session(
    state: &AppState,
    who: &Principal,
    session_id: Uuid,
    user_id: Uuid,
    org_id: Uuid,
) -> Result<(), ApiError> {
    if who.organization_id != org_id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "session access denied",
        ));
    }
    if who.role == "admin" || who.user_id == user_id {
        return Ok(());
    }
    let shared: bool = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM captured_sessions s WHERE s.id=$1 AND s.organization_id=$2 AND \
         (s.sharing_mode='workspace' OR EXISTS (SELECT 1 FROM captured_session_grants g JOIN users u ON u.id=g.user_id \
          WHERE g.captured_session_id=s.id AND g.user_id=$3 AND u.active=true))) AS \"exists!\"",
        session_id, who.organization_id, who.user_id
    ).fetch_one(&state.pool).await?;
    if shared {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "session access denied",
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionSharingInput {
    mode: String,
    #[serde(default)]
    user_ids: Vec<Uuid>,
}

async fn session_sharing(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let session: Option<(Uuid, Uuid, String)> = sqlx::query!(
        "SELECT user_id,organization_id,sharing_mode FROM captured_sessions WHERE id=$1",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .map(|r| (r.user_id, r.organization_id, r.sharing_mode));
    let (owner_id, org_id, mode) =
        session.ok_or_else(|| ApiError::not_found("captured session not found"))?;
    may_view_session(&state, &who, id, owner_id, org_id).await?;
    let recipients: Vec<(Uuid, String)> = match mode.as_str() {
        "workspace" => sqlx::query!(
            "SELECT id,email FROM users WHERE organization_id=$1 AND active=true AND id<>$2 ORDER BY lower(email),id",
            org_id,
            owner_id,
        )
        .fetch_all(&state.pool)
        .await?
        .into_iter()
        .map(|r| (r.id, r.email))
        .collect(),
        "selected" => sqlx::query!(
            "SELECT u.id,u.email FROM captured_session_grants g JOIN users u ON u.id=g.user_id WHERE g.captured_session_id=$1 AND u.active=true ORDER BY lower(u.email),u.id",
            id,
        )
        .fetch_all(&state.pool)
        .await?
        .into_iter()
        .map(|r| (r.id, r.email))
        .collect(),
        _ => Vec::new(),
    };
    Ok(Json(json!({
        "mode": mode,
        "can_edit": owner_id == who.user_id,
        "recipient_count": recipients.len(),
        "recipients": recipients.into_iter().map(|(id,email)| json!({"id":id,"email":email})).collect::<Vec<_>>(),
    })))
}

async fn update_session_sharing(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<SessionSharingInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !["private", "workspace", "selected"].contains(&input.mode.as_str()) {
        return Err(ApiError::bad_request(
            "sharing mode must be private, workspace, or selected",
        ));
    }
    if input.mode != "selected" && !input.user_ids.is_empty() {
        return Err(ApiError::bad_request(
            "user_ids are only valid for selected sharing",
        ));
    }
    let owner: Option<(Uuid, Uuid)> = sqlx::query!(
        "SELECT user_id,organization_id FROM captured_sessions WHERE id=$1",
        id
    )
    .fetch_optional(&state.pool)
    .await?
    .map(|r| (r.user_id, r.organization_id));
    let (owner_id, org_id) =
        owner.ok_or_else(|| ApiError::not_found("captured session not found"))?;
    if owner_id != who.user_id || org_id != who.organization_id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only the session owner may change sharing",
        ));
    }
    let unique: BTreeSet<_> = input
        .user_ids
        .into_iter()
        .filter(|value| *value != owner_id)
        .collect();
    if unique.len() > 500 {
        return Err(ApiError::bad_request(
            "selected sharing supports at most 500 recipients",
        ));
    }
    if input.mode == "selected" && unique.is_empty() {
        return Err(ApiError::bad_request(
            "selected sharing requires at least one recipient",
        ));
    }
    if !unique.is_empty() {
        let active: i64 = sqlx::query_scalar!(
            "SELECT count(*) AS \"count!\" FROM users WHERE organization_id=$1 AND active=true AND id=ANY($2)",
            org_id,
            &unique.iter().copied().collect::<Vec<_>>(),
        )
        .fetch_one(&state.pool)
        .await?;
        if active as usize != unique.len() {
            return Err(ApiError::bad_request(
                "all recipients must be active workspace members",
            ));
        }
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query!(
        "UPDATE captured_sessions SET sharing_mode=$2 WHERE id=$1",
        id,
        &input.mode
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM captured_session_grants WHERE captured_session_id=$1",
        id
    )
    .execute(&mut *tx)
    .await?;
    if input.mode == "selected" {
        for user_id in &unique {
            sqlx::query!("INSERT INTO captured_session_grants(captured_session_id,user_id) VALUES($1,$2) ON CONFLICT DO NOTHING", id, user_id)
                .execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(Json(json!({"id":id,"mode":input.mode,"user_ids":unique})))
}

#[derive(Deserialize, Default)]
struct SessionShareMemberQuery {
    q: Option<String>,
}

async fn session_share_members(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Query(query): Query<SessionShareMemberQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let search = like_filter("q", query.q.as_deref())?;
    let members: Vec<(Uuid, String)> = sqlx::query!(
        "SELECT id,email FROM users WHERE organization_id=$1 AND active=true AND id<>$2 AND ($3::text IS NULL OR lower(email) LIKE $3 ESCAPE E'\\\\') ORDER BY lower(email),id LIMIT 10",
        who.organization_id, who.user_id, search
    ).fetch_all(&state.pool).await?
    .into_iter().map(|r| (r.id, r.email)).collect();
    Ok(Json(
        json!({"items":members.into_iter().map(|(id,email)|json!({"id":id,"email":email})).collect::<Vec<_>>()}),
    ))
}

async fn complete_upload(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let artifact = artifact_owner(&state, id).await?;
    may_access(&who, artifact.user_id, artifact.organization_id)?;
    if artifact.status == "complete" {
        return Ok(Json(json!({"id":id,"status":"complete"})));
    }
    if artifact.upload_expires_at <= OffsetDateTime::now_utc() {
        return Err(ApiError::bad_request("upload request has expired"));
    }
    let metadata = state.blob.head(&artifact.object_key).await?;
    if metadata.size_bytes != artifact.size_bytes
        || metadata.sha256.as_deref() != Some(artifact.sha256.as_str())
    {
        sqlx::query!("UPDATE session_artifacts SET status='failed',failure_reason='blob metadata mismatch' WHERE id=$1",
        id)
        .execute(&state.pool)
        .await?;
        return Err(ApiError::bad_request(
            "uploaded blob size or checksum does not match",
        ));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query!(
        "UPDATE session_artifacts SET status='superseded' \
         WHERE captured_session_id=$1 AND status='complete' AND id<>$2",
        artifact.captured_session_id,
        id
    )
    .execute(&mut *tx)
    .await?;
    let retention_expires_at =
        OffsetDateTime::now_utc() + Duration::days(state.config.session_retention_days);
    sqlx::query!(
        "UPDATE session_artifacts SET status='complete',completed_at=now(), \
         retention_expires_at=$2 WHERE id=$1",
        id,
        retention_expires_at
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE captured_sessions SET current_artifact_id=$1,updated_at=now() WHERE id=$2",
        id,
        artifact.captured_session_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"id":id,"status":"complete"})))
}

#[derive(Clone, Deserialize, Default)]
struct SessionListQuery {
    limit: Option<i64>,
    cursor: Option<Uuid>,
    page: Option<i64>,
    per_page: Option<i64>,
    q: Option<String>,
    harness: Option<String>,
    status: Option<String>,
    user_id: Option<Uuid>,
    updated_from: Option<String>,
    updated_to: Option<String>,
    sort: Option<String>,
    resumable: Option<bool>,
}
#[derive(FromRow)]
struct SessionListRow {
    id: Uuid,
    user_id: Uuid,
    user_email: String,
    harness: String,
    compatibility_profile: String,
    native_session_id: String,
    cwd: Option<String>,
    artifact_format: String,
    resumable: bool,
    title: Option<String>,
    summary: Option<String>,
    source_captured_at: Option<OffsetDateTime>,
    repository_root: Option<String>,
    repository_remote: Option<String>,
    sharing_mode: String,
    updated_at: OffsetDateTime,
    artifact_status: Option<String>,
    size_bytes: Option<i64>,
    content_type: Option<String>,
    retention_expires_at: Option<OffsetDateTime>,
}
fn session_json(row: SessionListRow) -> serde_json::Value {
    json!({"id":row.id,"user_id":row.user_id,"user_email":row.user_email,"harness":row.harness,"compatibility_profile":row.compatibility_profile,"native_session_id":row.native_session_id,"cwd":row.cwd,"artifact_format":row.artifact_format,"resumable":row.resumable,"title":row.title,"summary":row.summary,"source_captured_at":row.source_captured_at.map(now_text),"repository":{"root":row.repository_root,"remote":row.repository_remote},"sharing_mode":row.sharing_mode,"updated_at":now_text(row.updated_at),"status":row.artifact_status,"size_bytes":row.size_bytes,"content_type":row.content_type,"retention_expires_at":row.retention_expires_at.map(now_text)})
}

async fn add_session_sharing_summaries(
    pool: &PgPool,
    items: &mut [serde_json::Value],
) -> Result<(), ApiError> {
    #[derive(FromRow)]
    struct SharingCountRow {
        session_id: Uuid,
        recipient_count: i64,
    }
    let ids = items
        .iter()
        .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
        .filter_map(|id| Uuid::parse_str(id).ok())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Ok(());
    }
    let counts: Vec<SharingCountRow> = sqlx::query_as!(
        SharingCountRow,
        "SELECT s.id AS session_id, CASE s.sharing_mode \
         WHEN 'workspace' THEN (SELECT count(*) FROM users u WHERE u.organization_id=s.organization_id AND u.active=true AND u.id<>s.user_id) \
         WHEN 'selected' THEN (SELECT count(*) FROM captured_session_grants g JOIN users u ON u.id=g.user_id WHERE g.captured_session_id=s.id AND u.active=true) \
         ELSE 0 END::bigint AS \"recipient_count!\" FROM captured_sessions s WHERE s.id=ANY($1)",
        &ids,
    )
    .fetch_all(pool)
    .await?;
    let counts = counts
        .into_iter()
        .map(|row| (row.session_id, row.recipient_count))
        .collect::<BTreeMap<_, _>>();
    for item in items {
        let count = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .and_then(|id| counts.get(&id).copied())
            .unwrap_or(0);
        item["share_recipient_count"] = json!(count);
    }
    Ok(())
}

struct SessionPageFilters {
    user_id: Option<Uuid>,
    harness: Option<String>,
    status: Option<String>,
    search: Option<String>,
    updated_from: Option<OffsetDateTime>,
    updated_to_exclusive: Option<OffsetDateTime>,
    sort: String,
}

fn parse_session_date(value: &str, field: &str) -> Result<OffsetDateTime, ApiError> {
    let parts: Vec<_> = value.split('-').collect();
    if parts.len() != 3 {
        return Err(ApiError::bad_request(format!(
            "{field} must use YYYY-MM-DD"
        )));
    }
    let year = parts[0]
        .parse::<i32>()
        .map_err(|_| ApiError::bad_request(format!("{field} must use YYYY-MM-DD")))?;
    let month = parts[1]
        .parse::<u8>()
        .ok()
        .and_then(|value| Month::try_from(value).ok())
        .ok_or_else(|| ApiError::bad_request(format!("{field} must use YYYY-MM-DD")))?;
    let day = parts[2]
        .parse::<u8>()
        .map_err(|_| ApiError::bad_request(format!("{field} must use YYYY-MM-DD")))?;
    Date::from_calendar_date(year, month, day)
        .map_err(|_| ApiError::bad_request(format!("{field} must be a valid date")))?
        .with_hms(0, 0, 0)
        .map(|value| value.assume_utc())
        .map_err(|_| ApiError::bad_request(format!("{field} must be a valid date")))
}

/// Builds the bounded, case-insensitive `LIKE` pattern used by every free-text
/// filter. `field` names the query parameter so rejections stay actionable.
fn like_filter(field: &str, value: Option<&str>) -> Result<Option<String>, ApiError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.chars().count() > 200 {
                Err(ApiError::bad_request(format!(
                    "{field} must be 200 characters or fewer"
                )))
            } else {
                Ok(format!("%{}%", escape_like(&value.to_lowercase())))
            }
        })
        .transpose()
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn page_count(total: i64, per_page: i64) -> i64 {
    if total == 0 {
        0
    } else {
        (total + per_page - 1) / per_page
    }
}

fn validate_session_page_query(
    query: &SessionListQuery,
    requested_user: Option<Uuid>,
) -> Result<(i64, i64, i64, SessionPageFilters), ApiError> {
    if query.cursor.is_some() || query.limit.is_some() {
        return Err(ApiError::bad_request(
            "page/per_page cannot be combined with cursor/limit",
        ));
    }
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(25);
    if page < 1 {
        return Err(ApiError::bad_request("page must be at least 1"));
    }
    if !(1..=100).contains(&per_page) {
        return Err(ApiError::bad_request("per_page must be between 1 and 100"));
    }
    let offset = (page - 1)
        .checked_mul(per_page)
        .ok_or_else(|| ApiError::bad_request("page is too large"))?;
    if query
        .status
        .as_deref()
        .is_some_and(|value| !["pending", "complete", "superseded", "failed"].contains(&value))
    {
        return Err(ApiError::bad_request("invalid session status"));
    }
    let sort = query.sort.as_deref().unwrap_or("updated_desc");
    if !["updated_desc", "updated_asc"].contains(&sort) {
        return Err(ApiError::bad_request(
            "sort must be updated_desc or updated_asc",
        ));
    }
    let search = like_filter("q", query.q.as_deref())?;
    let updated_from = query
        .updated_from
        .as_deref()
        .map(|value| parse_session_date(value, "updated_from"))
        .transpose()?;
    let updated_to_exclusive = query
        .updated_to
        .as_deref()
        .map(|value| {
            parse_session_date(value, "updated_to")?
                .checked_add(Duration::days(1))
                .ok_or_else(|| ApiError::bad_request("updated_to is outside the supported range"))
        })
        .transpose()?;
    if updated_from
        .zip(updated_to_exclusive)
        .is_some_and(|(from, to)| from >= to)
    {
        return Err(ApiError::bad_request(
            "updated_from must be on or before updated_to",
        ));
    }
    Ok((
        page,
        per_page,
        offset,
        SessionPageFilters {
            user_id: requested_user,
            harness: query.harness.clone(),
            status: query.status.clone(),
            search,
            updated_from,
            updated_to_exclusive,
            sort: sort.to_owned(),
        },
    ))
}

async fn list_captured_sessions(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if query.resumable == Some(true) {
        return list_resumable_sessions(&state, &who, &query).await;
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let requested_user = if who.role == "admin" {
        query.user_id
    } else {
        Some(who.user_id)
    };
    let numbered_mode = query.page.is_some()
        || query.per_page.is_some()
        || query.q.is_some()
        || query.updated_from.is_some()
        || query.updated_to.is_some()
        || query.sort.is_some();
    if numbered_mode {
        let (page, per_page, offset, filters) =
            validate_session_page_query(&query, requested_user)?;
        let total: i64 = sqlx::query_scalar!(
            "SELECT count(*) as \"count!\" FROM captured_sessions s \
             LEFT JOIN session_artifacts a ON a.id=s.current_artifact_id \
             WHERE s.organization_id=$1 AND s.current_artifact_id IS NOT NULL \
             AND ($2::uuid IS NULL OR s.user_id=$2 OR s.sharing_mode='workspace' OR EXISTS (SELECT 1 FROM captured_session_grants g JOIN users recipient ON recipient.id=g.user_id WHERE g.captured_session_id=s.id AND g.user_id=$2 AND recipient.active=true)) AND ($3::text IS NULL OR s.harness=$3) \
             AND ($4::text IS NULL OR a.status=$4) \
             AND ($5::text IS NULL OR lower(s.native_session_id) LIKE $5 ESCAPE E'\\\\' \
                  OR lower(coalesce(s.cwd,'')) LIKE $5 ESCAPE E'\\\\' \
                  OR lower(coalesce(s.title,'')) LIKE $5 ESCAPE E'\\\\' \
                  OR lower(coalesce(s.summary,'')) LIKE $5 ESCAPE E'\\\\') \
             AND ($6::timestamptz IS NULL OR s.updated_at >= $6) \
             AND ($7::timestamptz IS NULL OR s.updated_at < $7)",
            who.organization_id, filters.user_id, filters.harness.clone(),
            filters.status.clone(), filters.search.clone(), filters.updated_from,
            filters.updated_to_exclusive,
        )
        .fetch_one(&state.pool)
        .await?;
        let rows = sqlx::query_as!(
        SessionListRow,
        "SELECT s.id,s.user_id,u.email AS user_email,s.harness,s.compatibility_profile,s.native_session_id,s.cwd,s.artifact_format,s.resumable,s.title,s.summary,s.source_captured_at,s.repository_root,s.repository_remote,s.sharing_mode,s.updated_at, \
             a.status AS \"artifact_status?\",a.size_bytes AS \"size_bytes?\",a.content_type AS \"content_type?\",a.retention_expires_at AS \"retention_expires_at?\" \
             FROM captured_sessions s JOIN users u ON u.id=s.user_id \
             LEFT JOIN session_artifacts a ON a.id=s.current_artifact_id \
             WHERE s.organization_id=$1 AND s.current_artifact_id IS NOT NULL \
             AND ($2::uuid IS NULL OR s.user_id=$2 OR s.sharing_mode='workspace' OR EXISTS (SELECT 1 FROM captured_session_grants g JOIN users recipient ON recipient.id=g.user_id WHERE g.captured_session_id=s.id AND g.user_id=$2 AND recipient.active=true)) AND ($3::text IS NULL OR s.harness=$3) \
             AND ($4::text IS NULL OR a.status=$4) \
             AND ($5::text IS NULL OR lower(s.native_session_id) LIKE $5 ESCAPE E'\\\\' \
                  OR lower(coalesce(s.cwd,'')) LIKE $5 ESCAPE E'\\\\' \
                  OR lower(coalesce(s.title,'')) LIKE $5 ESCAPE E'\\\\' \
                  OR lower(coalesce(s.summary,'')) LIKE $5 ESCAPE E'\\\\') \
             AND ($6::timestamptz IS NULL OR s.updated_at >= $6) \
             AND ($7::timestamptz IS NULL OR s.updated_at < $7) \
             ORDER BY CASE WHEN $8='updated_asc' THEN s.updated_at END ASC, \
                      CASE WHEN $8='updated_asc' THEN s.id END ASC, \
                      CASE WHEN $8='updated_desc' THEN s.updated_at END DESC, \
                      CASE WHEN $8='updated_desc' THEN s.id END DESC \
             OFFSET $9 LIMIT $10",
        who.organization_id, filters.user_id, filters.harness,
        filters.status, filters.search, filters.updated_from,
        filters.updated_to_exclusive, filters.sort, offset, per_page,
        )
        .fetch_all(&state.pool)
        .await?;
        let mut items = rows.into_iter().map(session_json).collect::<Vec<_>>();
        add_session_sharing_summaries(&state.pool, &mut items).await?;
        return Ok(Json(json!({
            "items": items,
            "page": page,
            "per_page": per_page,
            "total": total,
            "total_pages": page_count(total, per_page),
        })));
    }
    let rows = sqlx::query_as!(
        SessionListRow,
        "SELECT s.id,s.user_id,u.email AS user_email,s.harness,s.compatibility_profile,s.native_session_id,s.cwd,s.artifact_format,s.resumable,s.title,s.summary,s.source_captured_at,s.repository_root,s.repository_remote,s.sharing_mode,s.updated_at, \
         a.status AS \"artifact_status?\",a.size_bytes AS \"size_bytes?\",a.content_type AS \"content_type?\",a.retention_expires_at AS \"retention_expires_at?\" \
         FROM captured_sessions s JOIN users u ON u.id=s.user_id LEFT JOIN session_artifacts a ON a.id=s.current_artifact_id \
         WHERE s.organization_id=$1 AND s.current_artifact_id IS NOT NULL \
         AND ($2::uuid IS NULL OR s.user_id=$2 OR s.sharing_mode='workspace' OR EXISTS (SELECT 1 FROM captured_session_grants g JOIN users recipient ON recipient.id=g.user_id WHERE g.captured_session_id=s.id AND g.user_id=$2 AND recipient.active=true)) AND ($3::text IS NULL OR s.harness=$3) \
         AND ($4::text IS NULL OR a.status=$4) AND ($5::uuid IS NULL OR (s.updated_at,s.id)< \
         (SELECT updated_at,id FROM captured_sessions WHERE id=$5)) ORDER BY s.updated_at DESC,s.id DESC LIMIT $6",
        who.organization_id, requested_user, query.harness,
        query.status, query.cursor, limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let mut items: Vec<_> = rows
        .into_iter()
        .take(limit as usize)
        .map(session_json)
        .collect();
    add_session_sharing_summaries(&state.pool, &mut items).await?;
    let next_cursor = if has_more {
        items.last().and_then(|value| value.get("id")).cloned()
    } else {
        None
    };
    Ok(Json(json!({"items":items,"next_cursor":next_cursor})))
}

/// Picker view: even administrators see only sessions they own or that were
/// deliberately shared with them. This keeps audit authority out of the
/// day-to-day native resume workflow.
async fn list_resumable_sessions(
    state: &AppState,
    who: &Principal,
    query: &SessionListQuery,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let search = like_filter("q", query.q.as_deref())?;
    let rows = sqlx::query_as!(
        SessionListRow,
        "SELECT s.id,s.user_id,u.email AS user_email,s.harness,s.compatibility_profile,s.native_session_id,s.cwd,\
         s.artifact_format,s.resumable,s.title,s.summary,s.source_captured_at,s.repository_root,s.repository_remote,s.sharing_mode,s.updated_at,\
         a.status AS \"artifact_status?\",a.size_bytes AS \"size_bytes?\",a.content_type AS \"content_type?\",a.retention_expires_at AS \"retention_expires_at?\" \
         FROM captured_sessions s JOIN users u ON u.id=s.user_id JOIN session_artifacts a ON a.id=s.current_artifact_id \
         WHERE s.organization_id=$1 AND s.resumable=true AND a.status='complete' \
         AND (s.user_id=$2 OR s.sharing_mode='workspace' OR EXISTS \
             (SELECT 1 FROM captured_session_grants g JOIN users recipient ON recipient.id=g.user_id \
              WHERE g.captured_session_id=s.id AND g.user_id=$2 AND recipient.active=true)) \
         AND ($3::text IS NULL OR s.harness=$3) \
         AND ($4::text IS NULL OR lower(s.native_session_id) LIKE $4 ESCAPE E'\\\\' \
              OR lower(coalesce(s.cwd,'')) LIKE $4 ESCAPE E'\\\\' \
              OR lower(coalesce(s.title,'')) LIKE $4 ESCAPE E'\\\\' \
              OR lower(coalesce(s.summary,'')) LIKE $4 ESCAPE E'\\\\') \
         AND ($5::uuid IS NULL OR (s.updated_at,s.id)<(SELECT updated_at,id FROM captured_sessions WHERE id=$5)) \
         ORDER BY s.updated_at DESC,s.id DESC LIMIT $6",
        who.organization_id,
        who.user_id,
        query.harness.as_deref(),
        search.as_deref(),
        query.cursor,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let mut items: Vec<_> = rows
        .into_iter()
        .take(limit as usize)
        .map(|row| {
            let shared = row.user_id != who.user_id;
            let mut value = session_json(row);
            value["shared"] = json!(shared);
            value
        })
        .collect();
    add_session_sharing_summaries(&state.pool, &mut items).await?;
    let next_cursor = has_more
        .then(|| items.last().and_then(|value| value.get("id")).cloned())
        .flatten();
    Ok(Json(json!({"items":items,"next_cursor":next_cursor})))
}

#[derive(FromRow)]
struct SessionFacetUser {
    id: Uuid,
    email: String,
}

#[derive(Default, Deserialize)]
struct FacetUserQuery {
    user_q: Option<String>,
    selected_user_id: Option<Uuid>,
}

async fn captured_session_facets(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Query(query): Query<FacetUserQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let requested_user = (who.role != "admin").then_some(who.user_id);
    let user_search = like_filter("user_q", query.user_q.as_deref())?;
    let users = sqlx::query_as!(
        SessionFacetUser,
        "SELECT u.id,u.email FROM captured_sessions s JOIN users u ON u.id=s.user_id \
         WHERE s.organization_id=$1 AND s.current_artifact_id IS NOT NULL \
         AND ($2::uuid IS NULL OR s.user_id=$2 OR s.sharing_mode='workspace' OR EXISTS (SELECT 1 FROM captured_session_grants g JOIN users recipient ON recipient.id=g.user_id WHERE g.captured_session_id=s.id AND g.user_id=$2 AND recipient.active=true)) \
         AND ($3::text IS NULL OR lower(u.email) LIKE $3 ESCAPE E'\\\\') \
         GROUP BY u.id,u.email ORDER BY (u.id=$4) DESC,u.email LIMIT 10",
        who.organization_id, requested_user, user_search.as_deref(), query.selected_user_id,
    )
    .fetch_all(&state.pool)
    .await?;
    let harnesses: Vec<String> = sqlx::query_scalar!(
        "SELECT DISTINCT s.harness FROM captured_sessions s \
         WHERE s.organization_id=$1 AND s.current_artifact_id IS NOT NULL \
         AND ($2::uuid IS NULL OR s.user_id=$2 OR s.sharing_mode='workspace' OR EXISTS (SELECT 1 FROM captured_session_grants g JOIN users recipient ON recipient.id=g.user_id WHERE g.captured_session_id=s.id AND g.user_id=$2 AND recipient.active=true)) ORDER BY s.harness",
        who.organization_id, requested_user,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "users": users.into_iter().map(|user| json!({"id":user.id,"email":user.email})).collect::<Vec<_>>(),
        "harnesses": harnesses,
    })))
}

async fn captured_session_detail(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row = sqlx::query_as!(
        SessionListRow,
        "SELECT s.id,s.user_id,u.email AS user_email,s.harness,s.compatibility_profile,s.native_session_id,s.cwd,s.artifact_format,s.resumable,s.title,s.summary,s.source_captured_at,s.repository_root,s.repository_remote,s.sharing_mode,s.updated_at, \
         a.status AS \"artifact_status?\",a.size_bytes AS \"size_bytes?\",a.content_type AS \"content_type?\",a.retention_expires_at AS \"retention_expires_at?\" \
         FROM captured_sessions s JOIN users u ON u.id=s.user_id LEFT JOIN session_artifacts a ON a.id=s.current_artifact_id WHERE s.id=$1",
        id,
    ).fetch_optional(&state.pool).await?.ok_or_else(||ApiError::not_found("captured session not found"))?;
    let org_id: Uuid = sqlx::query_scalar!(
        "SELECT organization_id FROM captured_sessions WHERE id=$1",
        id
    )
    .fetch_one(&state.pool)
    .await?;
    may_view_session(&state, &who, id, row.user_id, org_id).await?;
    #[derive(FromRow)]
    struct ArtifactRow {
        id: Uuid,
        sha256: String,
        size_bytes: i64,
        content_type: String,
        status: String,
        created_at: OffsetDateTime,
        completed_at: Option<OffsetDateTime>,
        retention_expires_at: Option<OffsetDateTime>,
    }
    let artifacts=sqlx::query_as!(ArtifactRow,
        "SELECT id,sha256,size_bytes,content_type,status,created_at,completed_at,retention_expires_at FROM session_artifacts WHERE captured_session_id=$1 ORDER BY created_at DESC",
        id).fetch_all(&state.pool).await?;
    let grants: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT user_id FROM captured_session_grants WHERE captured_session_id=$1 ORDER BY user_id",
        id,
    )
    .fetch_all(&state.pool)
    .await?;
    let mut sessions = vec![session_json(row)];
    add_session_sharing_summaries(&state.pool, &mut sessions).await?;
    Ok(Json(
        json!({"session":sessions.pop().expect("one session"),"sharing":{"user_ids":grants},"artifacts":artifacts.into_iter().map(|a|json!({"id":a.id,"sha256":a.sha256,"size_bytes":a.size_bytes,"content_type":a.content_type,"status":a.status,"created_at":now_text(a.created_at),"completed_at":a.completed_at.map(now_text),"retention_expires_at":a.retention_expires_at.map(now_text)})).collect::<Vec<_>>() }),
    ))
}

async fn download_session(
    State(state): State<Arc<AppState>>,
    Extension(who): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    #[derive(FromRow)]
    struct DownloadRow {
        user_id: Uuid,
        organization_id: Uuid,
        object_key: String,
        retention_expires_at: Option<OffsetDateTime>,
    }
    let row=sqlx::query_as!(DownloadRow,
        "SELECT s.user_id,s.organization_id,a.object_key,a.retention_expires_at FROM captured_sessions s JOIN session_artifacts a ON a.id=s.current_artifact_id WHERE s.id=$1 AND a.status='complete'",
        id).fetch_optional(&state.pool).await?.ok_or_else(||ApiError::not_found("completed session artifact not found"))?;
    may_view_session(&state, &who, id, row.user_id, row.organization_id).await?;
    if row
        .retention_expires_at
        .is_some_and(|expiry| expiry <= OffsetDateTime::now_utc())
    {
        return Err(ApiError::new(
            StatusCode::GONE,
            "session artifact has expired",
        ));
    }
    let signed = state.blob.presign_get(&row.object_key).await?;
    Ok(Json(
        json!({"download_url":signed.url,"expires_in_seconds":state.config.blob_presign_ttl_seconds}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid_report(
        reason: Option<gh_gateway::InvalidCredentialReason>,
        classification: Option<&str>,
    ) -> InvalidGatewayCredentialReport {
        InvalidGatewayCredentialReport {
            user_id: Uuid::nil(),
            credential_version: Uuid::nil(),
            reason,
            classification: classification.map(str::to_owned),
        }
    }

    #[test]
    fn invalid_credential_reports_support_semantic_and_legacy_wires() {
        use gh_gateway::InvalidCredentialReason::{Blocked, NotFound, Revoked};

        let semantic: InvalidGatewayCredentialReport = serde_json::from_value(json!({
            "user_id": Uuid::nil(),
            "credential_version": Uuid::nil(),
            "reason": "revoked"
        }))
        .unwrap();
        assert_eq!(invalid_credential_reason(&semantic).unwrap(), Revoked);

        assert_eq!(
            invalid_credential_reason(&invalid_report(Some(NotFound), None)).unwrap(),
            NotFound
        );
        assert_eq!(
            invalid_credential_reason(&invalid_report(None, Some("key_blocked"))).unwrap(),
            Blocked
        );
        assert_eq!(
            invalid_credential_reason(&invalid_report(
                Some(NotFound),
                Some("token_not_found_in_db"),
            ))
            .unwrap(),
            NotFound
        );
        assert_eq!(
            invalid_credential_reason(&invalid_report(Some(Revoked), None)).unwrap(),
            Revoked
        );
        assert!(invalid_credential_reason(&invalid_report(
            Some(Blocked),
            Some("token_not_found_in_db"),
        ))
        .is_err());
        assert!(invalid_credential_reason(&invalid_report(None, Some("provider_error"))).is_err());
        assert!(invalid_credential_reason(&invalid_report(None, None)).is_err());
        assert_eq!(
            Revoked.invalidation_message(),
            "upstream gateway credential was revoked"
        );
    }

    struct TestProvisioner;

    #[test]
    fn identity_status_contains_only_safe_configuration() {
        let response = IdentityStatusResponse {
            auth_mode: "oidc".into(),
            scim: ScimIdentityStatus {
                configured: true,
                group_role_mappings: BTreeMap::from([
                    ("Blue Admins".into(), "admin".into()),
                    ("Blue Developers".into(), "member".into()),
                ]),
            },
        };
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(
            value,
            json!({
                "auth_mode": "oidc",
                "scim": {
                    "configured": true,
                    "group_role_mappings": {
                        "Blue Admins": "admin",
                        "Blue Developers": "member",
                    },
                },
            })
        );
        let serialized = value.to_string();
        assert!(!serialized.contains("token"));
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn identity_status_represents_unmanaged_configuration() {
        let response = IdentityStatusResponse {
            auth_mode: "password".into(),
            scim: ScimIdentityStatus {
                configured: false,
                group_role_mappings: BTreeMap::new(),
            },
        };

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "auth_mode": "password",
                "scim": { "configured": false, "group_role_mappings": {} },
            })
        );
    }

    #[async_trait::async_trait]
    impl GatewayProvisioner for TestProvisioner {
        fn kind(&self) -> &'static str {
            "company-litellm"
        }

        async fn ensure(
            &self,
            _request: gh_gateway_provisioner::EnsureRequest,
        ) -> Result<gh_gateway_provisioner::ProvisionedCredential, ProvisionerError> {
            unreachable!()
        }

        async fn revoke(
            &self,
            _request: gh_gateway_provisioner::RevokeRequest,
        ) -> Result<gh_gateway_provisioner::RevokeResponse, ProvisionerError> {
            unreachable!()
        }
    }

    #[test]
    fn linked_provisioner_is_optional_until_selected() {
        let linked: Option<Arc<dyn GatewayProvisioner>> = Some(Arc::new(TestProvisioner));
        assert!(validate_linked_provisioner(None, &linked).is_ok());

        let matching = GatewayProvisionerConfig {
            kind: "company-litellm".into(),
            reconcile_ttl_seconds: 3600,
            executable_path: None,
            executable_sha256: None,
            policy_revision: None,
            timeout_seconds: default_provisioner_timeout(),
            max_concurrency: default_provisioner_max_concurrency(),
        };
        assert!(validate_linked_provisioner(Some(&matching), &linked).is_ok());

        let mismatched = GatewayProvisionerConfig {
            kind: "other".into(),
            reconcile_ttl_seconds: 3600,
            executable_path: None,
            executable_sha256: None,
            policy_revision: None,
            timeout_seconds: default_provisioner_timeout(),
            max_concurrency: default_provisioner_max_concurrency(),
        };
        assert!(validate_linked_provisioner(Some(&mismatched), &linked).is_err());

        let unavailable = None;
        assert!(validate_linked_provisioner(Some(&matching), &unavailable).is_err());
    }

    #[test]
    fn manual_gateway_retry_bypasses_cooldown_and_attempt_limit() {
        let now = OffsetDateTime::now_utc();

        assert!(defer_gateway_retry(
            false,
            1,
            Some(now + Duration::minutes(1)),
            now
        ));
        assert!(defer_gateway_retry(false, 5, None, now));
        assert!(!defer_gateway_retry(
            true,
            5,
            Some(now + Duration::hours(1)),
            now
        ));
        assert!(!defer_gateway_retry(
            false,
            1,
            Some(now - Duration::seconds(1)),
            now
        ));
    }

    #[test]
    fn provisioner_yaml_selects_code_without_gateway_policy() {
        let selected: serde_yaml::Value = serde_yaml::from_str(
            "gateway:\n  provisioner:\n    type: company-litellm\n    reconcile_ttl_seconds: 3600\n",
        )
        .unwrap();
        let provisioner = gateway_provisioner(&Some(selected)).unwrap().unwrap();
        assert_eq!(provisioner.kind, "company-litellm");
        assert_eq!(provisioner.reconcile_ttl_seconds, 3600);

        let runtime_module: serde_yaml::Value = serde_yaml::from_str(
            "gateway:\n  provisioner:\n    type: company-runtime\n    executable_path: /etc/blue/provisioner\n    executable_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n    policy_revision: v1\n    timeout_seconds: 7\n",
        )
        .unwrap();
        let provisioner = gateway_provisioner(&Some(runtime_module)).unwrap().unwrap();
        assert_eq!(
            provisioner.executable_path.as_deref(),
            Some(std::path::Path::new("/etc/blue/provisioner"))
        );
        assert_eq!(provisioner.timeout_seconds, 7);
        assert_eq!(provisioner.policy_revision.as_deref(), Some("v1"));

        let invalid_ttl: serde_yaml::Value = serde_yaml::from_str(
            "gateway:\n  provisioner:\n    type: company-runtime\n    reconcile_ttl_seconds: 0\n",
        )
        .unwrap();
        assert!(gateway_provisioner(&Some(invalid_ttl)).is_err());

        let gateway_policy: serde_yaml::Value = serde_yaml::from_str(
            "gateway:\n  provisioner:\n    type: company-litellm\n    config:\n      max_budget: 100\n",
        )
        .unwrap();
        assert!(gateway_provisioner(&Some(gateway_policy)).is_err());
    }

    #[test]
    fn gateway_key_response_reports_disabled_when_gateway_off() {
        // Governance-only deployments have no top-level `gateway:` block, so both
        // GET /gateway/key and the ensure endpoint's None branch build their body
        // from gateway_key_response(false, .., None). It must be a benign disabled
        // signal (HTTP 200), not the old 400 "gateway mode is not enabled": the
        // CLI's ensure-first step no-ops on enabled:false, so `blue setup`/`status`
        // stop failing on governance-only while gateway mode is untouched.
        let response = gateway_key_response(false, "user@example.com".into(), None);
        assert!(!response.enabled);
        assert_eq!(response.status, "disabled");
        assert_eq!(response.email, "user@example.com");
        assert!(response.alias.is_none());
        assert!(response.external_id.is_none());
        assert!(response.error.is_none());
        assert!(response.invalidated_at.is_none());
        assert!(response.next_retry_at.is_none());

        // The serialized shape is what the CLI's GatewayKeyResponse deserializes;
        // enabled/status are the only fields it branches on.
        let body = serde_json::to_value(&response).unwrap();
        assert_eq!(body["enabled"], serde_json::json!(false));
        assert_eq!(body["status"], serde_json::json!("disabled"));
    }

    fn config_with_gateway_runtime(kind: Option<&str>) -> AppConfig {
        AppConfig {
            database_url: "postgres://localhost/blue".into(),
            listen: "127.0.0.1:8080".into(),
            internal_listen: "127.0.0.1:8081".into(),
            internal_transport_mode: "plaintext".into(),
            internal_tls_cert_file: None,
            internal_tls_key_file: None,
            internal_tls_client_ca_file: None,
            database_max_connections: 5,
            gateway_log_database_max_connections: 5,
            gateway_kms_max_concurrency: 4,
            gateway_kms_timeout_seconds: 5,
            run_background_jobs: false,
            blue_config_file: PathBuf::from("/etc/blue/blue.yaml"),
            blue_config_overlay_files: Vec::new(),
            bootstrap_org: "org".into(),
            bootstrap_org_name: "Org".into(),
            bootstrap_admin_sub: "admin".into(),
            bootstrap_admin_email: "admin@example.com".into(),
            session_retention_days: 30,
            gateway_request_log_retention_days: 30,
            s3_bucket: "bucket".into(),
            s3_package_bucket: "packages".into(),
            s3_region: "us-east-1".into(),
            s3_endpoint: None,
            s3_public_endpoint: None,
            s3_force_path_style: false,
            blob_presign_ttl_seconds: 900,
            auth_session_url: "https://auth.example.com/session".into(),
            auth_jwks_url: "https://auth.example.com/jwks".into(),
            auth_issuer: "https://auth.example.com".into(),
            auth_audience: "blue".into(),
            auth_client_id: "blue".into(),
            auth_public_url: "https://auth.example.com".into(),
            auth_mode: "oidc".into(),
            scim_bearer_token: None,
            scim_group_role_mappings: BTreeMap::new(),
            gateway_kind: kind.map(str::to_string),
            gateway_upstream_url: Some("https://litellm.example.com".into()),
            gateway_inference_proxy_url: Some("https://proxy.example.com".into()),
            gateway_inference_proxy_health_url: Some("https://proxy.example.com/health".into()),
            gateway_jwt_issuer: "https://api.example.com".into(),
            gateway_jwt_audience: "blue-inference-proxy".into(),
            gateway_inference_token_ttl_seconds: 43_200,
            gateway_jwt_active_kid: Some("test-key".into()),
            gateway_jwt_private_key_file: Some(PathBuf::from("/run/secrets/gateway-jwt.pem")),
            gateway_jwt_jwks_file: Some(PathBuf::from("/run/config/gateway-jwks.json")),
            gateway_jwt_private_key_pem: None,
            gateway_jwt_jwks_json: None,
            internal_allowed_client_id: "blue-inference-proxy".into(),
            gateway_provisioner: gateway_provisioner(&Some(
                serde_yaml::from_str(
                    "gateway:\n  provisioner:\n    type: company-litellm\n    reconcile_ttl_seconds: 3600\n",
                )
                .unwrap(),
            ))
            .unwrap(),
            gateway_encryption: gateway_encryption(&Some(
                serde_yaml::from_str("gateway:\n  secret_encryption:\n    provider: static\n    key: k\n").unwrap(),
            ))
            .unwrap(),
            package_source_connections: Vec::new(),
        }
    }

    #[test]
    fn gateway_gating_clears_runtime_when_type_unset() {
        // Env/compose can inject proxy URLs unconditionally; with no gateway.type
        // every runtime field must be dropped so the deployment is governance-only.
        let config = config_with_gateway_runtime(None).with_gateway_gated_on_type();
        assert!(config.gateway_kind.is_none());
        assert!(config.gateway_upstream_url.is_none());
        assert!(config.gateway_inference_proxy_url.is_none());
        assert!(config.gateway_inference_proxy_health_url.is_none());
        assert!(config.gateway_provisioner.is_none());
        assert!(config.gateway_encryption.is_none());
        // The cleared config resolves to governance-only, not a partial-config error.
        assert!(managed_gateway_settings(&config).unwrap().is_none());
    }

    #[test]
    fn gateway_gating_preserves_runtime_when_type_set() {
        let config = config_with_gateway_runtime(Some("litellm")).with_gateway_gated_on_type();
        assert_eq!(config.gateway_kind.as_deref(), Some("litellm"));
        assert_eq!(
            config.gateway_upstream_url.as_deref(),
            Some("https://litellm.example.com")
        );
        assert_eq!(
            config.gateway_inference_proxy_url.as_deref(),
            Some("https://proxy.example.com")
        );
        assert_eq!(
            config.gateway_inference_proxy_health_url.as_deref(),
            Some("https://proxy.example.com/health")
        );
        assert!(config.gateway_provisioner.is_some());
        assert!(config.gateway_encryption.is_some());
    }

    #[test]
    fn gateway_runtime_validation_aggregates_missing_names() {
        let mut config = config_with_gateway_runtime(Some("litellm"));
        config.gateway_upstream_url = None;
        config.gateway_inference_proxy_url = None;
        config.gateway_provisioner = None;
        config.gateway_encryption = None;
        config.internal_allowed_client_id.clear();

        let error = config.validate_gateway_runtime().unwrap_err().to_string();
        for name in [
            "gateway.url",
            "gateway.inference_proxy_url",
            "gateway.internal_allowed_client_id",
            "gateway.provisioner",
            "gateway.secret_encryption",
        ] {
            assert!(error.contains(name), "missing {name} in {error}");
        }
        assert!(!error.contains("litellm.example.com"));
    }

    #[test]
    fn missing_gateway_environment_references_defer_to_aggregate_validation() {
        let settings = Some(
            serde_yaml::from_str(
                "gateway:\n  url: os.environ/BLUE_TEST_MISSING_GATEWAY_URL_7C2A\n",
            )
            .unwrap(),
        );
        let value = optional_env_reference_setting(
            "BLUE_TEST_MISSING_GATEWAY_OVERRIDE_7C2A",
            &settings,
            &["gateway", "url"],
        )
        .unwrap();
        assert!(value.is_none());
    }

    #[test]
    fn branding_urls_are_trimmed_and_must_be_absolute_http_urls() {
        assert_eq!(
            normalize_branding_url(
                Some("  https://assets.example.com/logo.svg  ".into()),
                "logo_url"
            )
            .unwrap(),
            Some("https://assets.example.com/logo.svg".into())
        );
        assert_eq!(
            normalize_branding_url(Some("   ".into()), "logo_url").unwrap(),
            None
        );
        assert!(normalize_branding_url(Some("/logo.svg".into()), "logo_url").is_err());
        assert!(
            normalize_branding_url(Some("data:image/svg+xml,test".into()), "logo_url").is_err()
        );
        assert!(normalize_branding_url(
            Some(format!("https://example.com/{}", "a".repeat(2049))),
            "logo_url"
        )
        .is_err());
    }

    fn merged_json(
        baseline: serde_json::Value,
        current: serde_json::Value,
        incoming: serde_json::Value,
    ) -> (serde_json::Value, Vec<String>) {
        let mut conflicts = Vec::new();
        let merged = merge_governance_value(
            Some(&baseline),
            Some(&current),
            Some(&incoming),
            "",
            &mut conflicts,
        )
        .unwrap();
        (merged, conflicts)
    }

    #[test]
    fn deployment_merge_applies_non_conflicting_changes_and_preserves_dashboard_conflicts() {
        let baseline = json!({"ttl_seconds":300,"required":false,"harnesses":{"codex":{"managed_config":{"model":"old"}}}});
        let current = json!({"ttl_seconds":600,"required":false,"harnesses":{"codex":{"managed_config":{"model":"dashboard"}}}});
        let incoming = json!({"ttl_seconds":300,"required":true,"harnesses":{"codex":{"managed_config":{"model":"deployment"}}}});
        let (merged, conflicts) = merged_json(baseline, current, incoming);
        assert_eq!(merged["ttl_seconds"], 600);
        assert_eq!(merged["required"], true);
        assert_eq!(
            merged["harnesses"]["codex"]["managed_config"]["model"],
            "dashboard"
        );
        assert_eq!(conflicts, vec!["harnesses.codex.managed_config.model"]);
    }

    #[test]
    fn deployment_merge_keys_packages_and_mcp_servers() {
        let baseline = json!({"packages":[],"harnesses":{"codex":{"mcp":[]}}});
        let current = json!({"packages":[{"id":"dashboard","version":"1"}],"harnesses":{"codex":{"mcp":[{"name":"dashboard","command":"one"}]}}});
        let incoming = json!({"packages":[{"id":"deployment","version":"1"}],"harnesses":{"codex":{"mcp":[{"name":"deployment","command":"two"}]}}});
        let (merged, conflicts) = merged_json(baseline, current, incoming);
        assert!(conflicts.is_empty());
        assert_eq!(merged["packages"].as_array().unwrap().len(), 2);
        assert_eq!(
            merged["harnesses"]["codex"]["mcp"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn blue_export_redacts_literals_but_preserves_authored_references() {
        let mut document: serde_yaml::Value = serde_yaml::from_str(
            "control_api:\n  database_url: postgres://secret\n  identity:\n    scim_bearer_token: os.environ/CUSTOM_SCIM\n",
        ).unwrap();
        let mut redactions = Vec::new();
        redact_literal_secret(
            &mut document,
            &["control_api", "database_url"],
            "HARNESS_DATABASE_URL".into(),
            &mut redactions,
        );
        redact_literal_secret(
            &mut document,
            &["control_api", "identity", "scim_bearer_token"],
            "HARNESS_SCIM_BEARER_TOKEN".into(),
            &mut redactions,
        );
        let yaml = serde_yaml::to_string(&document).unwrap();
        assert!(!yaml.contains("postgres://secret"));
        assert!(yaml.contains("os.environ/CUSTOM_SCIM"));
        assert_eq!(redactions.len(), 1);
    }

    #[test]
    fn shipped_blue_configs_have_valid_modes_governance_and_catalog_sections() {
        let document: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../../../deploy/blue.yaml")).unwrap();
        assert!(document.get("gateway").is_none());
        assert!(document["governance"].get("session_upload").is_none());
        assert_eq!(document["governance"]["required"].as_bool(), Some(true));
        assert!(document["governance"].get("gateway").is_none());
        assert!(gateway_kind_setting(&Some(document.clone()))
            .unwrap()
            .is_none());

        let gateway_overlay: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../../../deploy/blue.gateway.yaml")).unwrap();
        assert_eq!(gateway_overlay.as_mapping().unwrap().len(), 1);
        let mut gateway_document = document.clone();
        merge_blue_config(&mut gateway_document, gateway_overlay).unwrap();
        assert_eq!(
            gateway_kind_setting(&Some(gateway_document.clone()))
                .unwrap()
                .as_deref(),
            Some("litellm")
        );
        assert!(gateway_document["governance"]
            .get("session_upload")
            .is_none());
        assert_eq!(gateway_document["governance"], document["governance"]);
        assert_eq!(gateway_document["control_api"], document["control_api"]);
        assert_eq!(
            gateway_document["package_catalog"],
            document["package_catalog"]
        );

        let capture_overlay: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../../../deploy/blue.capture.yaml")).unwrap();
        assert_eq!(capture_overlay.as_mapping().unwrap().len(), 1);
        let mut capture_document = document.clone();
        merge_blue_config(&mut capture_document, capture_overlay.clone()).unwrap();
        assert!(capture_document.get("gateway").is_none());
        assert!(capture_document["governance"]
            .get("session_upload")
            .is_some());
        let mut capture_governance = capture_document["governance"].clone();
        capture_governance
            .as_mapping_mut()
            .unwrap()
            .remove(yaml_key("session_upload"));
        assert_eq!(capture_governance, document["governance"]);
        assert_eq!(capture_document["control_api"], document["control_api"]);
        assert_eq!(
            capture_document["package_catalog"],
            document["package_catalog"]
        );

        let mut combined_document = gateway_document.clone();
        merge_blue_config(&mut combined_document, capture_overlay).unwrap();
        assert!(combined_document.get("gateway").is_some());
        assert!(combined_document["governance"]
            .get("session_upload")
            .is_some());
        assert_eq!(
            combined_document["governance"],
            capture_document["governance"]
        );
        assert_eq!(combined_document["control_api"], document["control_api"]);
        assert_eq!(
            combined_document["package_catalog"],
            document["package_catalog"]
        );

        let mut duplicate = gateway_document.clone();
        duplicate["governance"].as_mapping_mut().unwrap().insert(
            yaml_key("gateway"),
            serde_yaml::from_str("type: litellm").unwrap(),
        );
        assert!(gateway_kind_setting(&Some(duplicate)).is_err());
        let unsupported: serde_yaml::Value =
            serde_yaml::from_str("gateway:\n  type: unknown-gateway\n").unwrap();
        let error = gateway_kind_setting(&Some(unsupported)).unwrap_err();
        assert!(error.to_string().contains("compiled: litellm"));
        let governance: gh_service::GovernanceConfig =
            serde_yaml::from_value(document["governance"].clone()).unwrap();
        assert!(governance.is_allowed("codex"));
        let projected = governance_seed_yaml(&gateway_document, Some("litellm")).unwrap();
        let projected: gh_service::GovernanceConfig = serde_yaml::from_str(&projected).unwrap();
        assert_eq!(
            projected
                .gateway
                .as_ref()
                .map(|gateway| gateway.kind.as_str()),
            Some("litellm")
        );
        assert_eq!(
            projected
                .gateway
                .as_ref()
                .and_then(|gateway| gateway.proxy_url.as_deref()),
            None
        );
        let catalog: PackageCatalogDocument =
            serde_yaml::from_value(document["package_catalog"].clone()).unwrap();
        validate_packages(&catalog.packages).unwrap();
    }

    #[test]
    fn consumer_blue_config_contains_a_valid_governance_seed() {
        let document: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../../../deploy/consumer/blue/blue.yaml")).unwrap();
        assert!(document.get("control_api").is_some());
        assert!(document.get("gateway").is_none());
        let governance: gh_service::GovernanceConfig =
            serde_yaml::from_value(document["governance"].clone()).unwrap();
        assert!(governance.is_allowed("codex"));
        assert_eq!(governance.minimum_client_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn revision_event_payload_is_org_scoped_and_serializable() {
        let event = RevisionEvent {
            organization_id: Uuid::nil(),
            revision: "r2".into(),
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["organization_id"], Uuid::nil().to_string());
        assert_eq!(encoded["revision"], "r2");
    }

    #[test]
    fn boolean_settings_are_strict() {
        assert!(parse_bool_setting("true").unwrap());
        assert!(!parse_bool_setting("0").unwrap());
        assert!(parse_bool_setting("sometimes").is_err());
    }

    #[test]
    fn gateway_request_results_cover_forwarded_outcomes() {
        assert_eq!(request_result(Some(200)), "success");
        assert_eq!(request_result(Some(302)), "redirect");
        assert_eq!(request_result(Some(429)), "client_error");
        assert_eq!(request_result(Some(503)), "server_error");
        assert_eq!(request_result(None), "transport_error");
        assert_eq!(request_result(Some(99)), "transport_error");
    }

    #[test]
    fn gateway_request_log_query_validates_filters_and_scope() {
        let member = Uuid::new_v4();
        let query = GatewayRequestLogQuery {
            page: Some(2),
            per_page: Some(25),
            result: Some("success".into()),
            occurred_from: Some("2026-08-01".into()),
            occurred_to: Some("2026-08-29".into()),
            ..Default::default()
        };
        let (page, per_page, offset, filters) =
            validate_gateway_request_log_query(&query, Some(member)).unwrap();
        assert_eq!((page, per_page, offset), (2, 25, 25));
        assert_eq!(filters.user_id, Some(member));
        assert_eq!(filters.result.as_deref(), Some("success"));

        let invalid = GatewayRequestLogQuery {
            result: Some("unknown".into()),
            ..Default::default()
        };
        assert!(validate_gateway_request_log_query(&invalid, Some(member)).is_err());
    }

    #[test]
    fn catalog_adapter_availability_requires_a_compatible_harness_policy() {
        let document: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../../../deploy/blue.yaml")).unwrap();
        let mut governance: gh_service::GovernanceConfig =
            serde_yaml::from_value(document["governance"].clone()).unwrap();
        let catalog: PackageCatalogDocument =
            serde_yaml::from_value(document["package_catalog"].clone()).unwrap();
        governance.packages.push(
            catalog
                .packages
                .into_iter()
                .find(|package| package.id == "ponytail")
                .unwrap(),
        );

        governance
            .harnesses
            .entry("claude".into())
            .or_default()
            .version_requirement = None;

        let error = validate_complete_governance(&governance).unwrap_err();
        assert!(error.contains("package `ponytail`"));
        assert!(error.contains("requires harness version >=2.0.12"));

        governance
            .harnesses
            .entry("claude".into())
            .or_default()
            .version_requirement = Some(">=2.0.12, <2.1.253-0".into());
        stamp_version_aware_client_floor(&mut governance);
        assert!(validate_complete_governance(&governance).is_ok());
    }

    #[test]
    fn package_validation_rejects_duplicates_and_mutable_digests() {
        let package = gh_service::ManagedPackage {
            id: "review-kit".into(),
            name: None,
            version: "1".into(),
            source_ref: "https://packages.example/review.tar.gz".into(),
            artifact_id: None,
            sha256: "a".repeat(64),
            platform_sources: Default::default(),
            settings: Default::default(),
            adapters: Default::default(),
        };
        assert!(validate_packages(std::slice::from_ref(&package)).is_ok());
        assert!(validate_packages(&[package.clone(), package.clone()]).is_err());
        let mut invalid = package;
        invalid.sha256 = "main".into();
        assert!(validate_packages(&[invalid]).is_err());
    }

    #[test]
    fn version_aware_governance_requires_client_floor_and_nonoverlapping_variants() {
        let mut config: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "r1",
            "allowed_harnesses": ["codex"],
            "harnesses": {
                "codex": { "version_requirement": ">=0.145.0, <0.151.1-0" }
            }
        }))
        .unwrap();
        assert!(validate_complete_governance(&config)
            .unwrap_err()
            .contains("minimum_client_version"));
        config.minimum_client_version = Some("0.1.0".into());
        stamp_version_aware_client_floor(&mut config);
        assert!(validate_complete_governance(&config).is_ok());
        let left = gh_service::legacy_requirement_interval(">=1.0.0, <3.0.0").unwrap();
        let overlapping = gh_service::legacy_requirement_interval(">=2.0.0").unwrap();
        let disjoint = gh_service::legacy_requirement_interval(">=3.0.0").unwrap();
        assert!(left.overlaps(&overlapping));
        assert!(!left.overlaps(&disjoint));
    }

    #[test]
    fn harness_policy_rejects_a_range_above_the_certified_ceiling() {
        let mut config: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "r1",
            "allowed_harnesses": ["codex"],
            "harnesses": {
                "codex": { "version_requirement": ">0.151.0" }
            }
        }))
        .unwrap();
        stamp_version_aware_client_floor(&mut config);

        let error = uncertified_harness_range(&config).unwrap();
        assert!(error.contains("includes no Blue-certified release"));
        assert!(error.contains("enable `Allow unverified versions`"));

        config
            .harnesses
            .get_mut("codex")
            .unwrap()
            .allow_unverified_versions = true;
        stamp_version_aware_client_floor(&mut config);
        assert_eq!(uncertified_harness_range(&config), None);
    }

    /// The deployment that already holds such a range has to keep booting: the
    /// startup reconcile runs `validate_complete_governance`, and the admin API
    /// is the only way to correct the range.
    #[test]
    fn a_range_above_the_certified_ceiling_does_not_block_startup_validation() {
        let mut config: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "r1",
            "allowed_harnesses": ["codex"],
            "harnesses": {
                "codex": { "version_requirement": ">0.151.0" }
            }
        }))
        .unwrap();
        stamp_version_aware_client_floor(&mut config);

        assert!(uncertified_harness_range(&config).is_some());
        assert!(validate_complete_governance(&config).is_ok());
    }

    #[test]
    fn unverified_version_opt_in_requires_range_and_client_capability() {
        let mut config: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "r1",
            "allowed_harnesses": ["codex"],
            "harnesses": {
                "codex": { "allow_unverified_versions": true }
            }
        }))
        .unwrap();
        assert!(validate_complete_governance(&config)
            .unwrap_err()
            .contains("minimum_client_version"));

        stamp_version_aware_client_floor(&mut config);
        assert!(validate_complete_governance(&config)
            .unwrap_err()
            .contains("without version_requirement"));

        config
            .harnesses
            .get_mut("codex")
            .unwrap()
            .version_requirement = Some(">=0.151.1, <0.152.0".into());
        stamp_version_aware_client_floor(&mut config);
        assert!(config
            .required_capabilities
            .iter()
            .any(|capability| capability == "unverified_harness_versions"));
        assert!(validate_complete_governance(&config).is_ok());
    }

    #[test]
    fn persisted_gateway_policy_rejects_runtime_auth_fields() {
        let mut config: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "r1",
            "contract_version": 3,
            "required_capabilities": ["gateway_inference_jwt"],
            "gateway": { "type": "litellm", "token": "must-not-persist" }
        }))
        .unwrap();
        assert!(validate_complete_governance(&config)
            .unwrap_err()
            .contains("runtime-only"));
        config.gateway.as_mut().unwrap().token = None;
        config.gateway.as_mut().unwrap().proxy_url = Some("https://proxy.example".into());
        assert!(validate_complete_governance(&config)
            .unwrap_err()
            .contains("runtime-only"));
    }

    #[test]
    fn managed_yaml_omits_and_rejects_extension_owned_fields() {
        let config: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "r1",
            "allowed_harnesses": ["codex"],
            "session_upload": { "presign_url": "https://control.example/uploads" },
            "packages": [{
                "id": "tools", "version": "1", "source_ref": "https://example.com/tools.tar.gz",
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }],
            "harnesses": {
                "codex": {
                    "managed_config": { "model": "gpt-5" },
                    "mcp": [{ "name": "tools", "command": "npx" }],
                    "package_overrides": { "tools": { "enabled": true } }
                }
            }
        }))
        .unwrap();
        let yaml = extension_free_yaml(&config).unwrap();
        assert!(!yaml.contains("packages:"));
        assert!(!yaml.contains("package_overrides:"));
        assert!(!yaml.contains("mcp:"));
        assert!(yaml.contains("managed_config:"));
        assert!(yaml.contains("session_upload:"));
        assert!(reject_extension_owned_yaml(&yaml).is_ok());
        assert!(reject_extension_owned_yaml("revision: r1\npackages: []\n").is_err());
        assert!(
            reject_extension_owned_yaml("revision: r1\nharnesses:\n  codex:\n    mcp: []\n")
                .is_err()
        );
        assert!(reject_extension_owned_yaml(
            "revision: r1\nharnesses:\n  codex:\n    skills: []\n"
        )
        .is_err());
        assert!(reject_extension_owned_yaml(
            "revision: r1\nharnesses:\n  codex:\n    session_upload:\n      presign_url: https://control.example/uploads\n"
        )
        .is_err());
        assert!(reject_extension_owned_yaml(
            "revision: r1\nharnesses:\n  codex:\n    gateway:\n      type: litellm\n"
        )
        .is_err());
        assert!(reject_extension_owned_yaml(
            "revision: r1\ngateway:\n  type: litellm\n  model: gpt-5\n"
        )
        .is_err());
    }

    #[test]
    fn deployment_gateway_policy_stamps_the_inference_jwt_capability() {
        // A `managed_yaml` round-trip as the dashboard/CLI sends it: the client
        // echoes back a document whose stored revision was written without the
        // gateway capability (a governance-only control-api reconciled it away).
        let managed_yaml =
            "revision: r1\nallowed_harnesses:\n  - codex\ngateway:\n  type: litellm\n";
        let mut config =
            gh_service::source::parse_config(std::path::Path::new("config.yaml"), managed_yaml)
                .unwrap();
        assert!(validate_complete_governance(&config)
            .unwrap_err()
            .contains("gateway_inference_jwt"));

        apply_deployment_gateway_policy(&mut config, Some("litellm"));
        assert_eq!(config.gateway.as_ref().unwrap().kind, "litellm");
        assert!(config
            .required_capabilities
            .iter()
            .any(|capability| capability == "gateway_inference_jwt"));
        assert!(validate_complete_governance(&config).is_ok());

        // Governance-only deployments clear the gateway instead, and stamp nothing.
        let mut governance_only =
            gh_service::source::parse_config(std::path::Path::new("config.yaml"), managed_yaml)
                .unwrap();
        apply_deployment_gateway_policy(&mut governance_only, None);
        assert!(governance_only.gateway.is_none());
        assert!(!governance_only
            .required_capabilities
            .iter()
            .any(|capability| capability == "gateway_inference_jwt"));
        assert!(validate_complete_governance(&governance_only).is_ok());
    }

    #[test]
    fn managed_yaml_merge_preserves_current_extensions() {
        let mut next: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "next",
            "allowed_harnesses": ["codex"],
            "harnesses": { "codex": { "managed_config": { "model": "new" } } }
        }))
        .unwrap();
        let current: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "current",
            "allowed_harnesses": ["codex"],
            "packages": [{
                "id": "tools", "version": "1", "source_ref": "https://example.com/tools.tar.gz",
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }],
            "harnesses": { "codex": { "mcp": [{ "name": "tools", "command": "npx" }] } }
        }))
        .unwrap();
        merge_current_extensions(&mut next, current);
        assert_eq!(next.packages[0].id, "tools");
        assert_eq!(next.harnesses["codex"].mcp[0].name, "tools");
        assert_eq!(
            next.harnesses["codex"].managed_config.model.as_deref(),
            Some("new")
        );
    }

    #[test]
    fn harness_managed_config_update_preserves_other_policy() {
        let mut config: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "current",
            "allowed_harnesses": ["codex", "claude"],
            "packages": [{
                "id": "tools", "version": "1", "source_ref": "https://example.com/tools.tar.gz",
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }],
            "harnesses": {
                "codex": {
                    "managed_config": { "model": "old" },
                    "mcp": [{ "name": "tools", "command": "npx" }]
                },
                "claude": { "managed_config": { "model": "claude-opus" } }
            }
        }))
        .unwrap();
        let managed_config = serde_json::from_value(json!({
            "model": "new",
            "reasoning_effort": "high",
            "future_setting": { "enabled": true }
        }))
        .unwrap();

        replace_harness_managed_config(&mut config, "codex", managed_config).unwrap();

        assert_eq!(config.packages[0].id, "tools");
        assert_eq!(config.harnesses["codex"].mcp[0].name, "tools");
        assert_eq!(
            config.harnesses["codex"].managed_config.model.as_deref(),
            Some("new")
        );
        assert_eq!(
            config.harnesses["codex"].managed_config.extra["future_setting"],
            json!({ "enabled": true })
        );
        assert_eq!(
            config.harnesses["claude"].managed_config.model.as_deref(),
            Some("claude-opus")
        );
        assert!(replace_harness_managed_config(
            &mut config,
            "unsupported",
            gh_service::ManagedConfig::default()
        )
        .is_err());
    }

    #[test]
    fn harness_managed_config_yaml_requires_a_mapping_and_preserves_new_fields() {
        let parsed = parse_harness_managed_config_yaml(
            "model: governed-model\nfuture_setting:\n  enabled: true\n",
        )
        .unwrap();
        assert_eq!(parsed.model.as_deref(), Some("governed-model"));
        assert_eq!(parsed.extra["future_setting"], json!({ "enabled": true }));
        assert!(parse_harness_managed_config_yaml("").is_ok());
        assert!(parse_harness_managed_config_yaml("- model\n- sandbox_mode\n").is_err());
        assert!(parse_harness_managed_config_yaml("model: [\n").is_err());
    }

    #[test]
    fn mcp_validation_enforces_shape_targets_and_unique_names() {
        let local = gh_service::McpServer {
            name: "tools".into(),
            command: Some("npx".into()),
            args: vec!["-y".into()],
            env: Default::default(),
            url: None,
            transport: None,
            disabled: false,
        };
        let valid = BTreeMap::from([("codex".into(), vec![local.clone()])]);
        assert!(validate_mcp_servers(&valid).is_ok());
        assert!(
            validate_mcp_servers(&BTreeMap::from([("unknown".into(), vec![local.clone()])]))
                .is_err()
        );
        assert!(validate_mcp_servers(&BTreeMap::from([(
            "codex".into(),
            vec![local.clone(), local]
        )]))
        .is_err());
        let invalid_remote = gh_service::McpServer {
            name: "remote".into(),
            command: None,
            args: vec!["not-allowed".into()],
            env: Default::default(),
            url: Some("https://mcp.example.com".into()),
            transport: None,
            disabled: false,
        };
        assert!(
            validate_mcp_servers(&BTreeMap::from([("claude".into(), vec![invalid_remote])]))
                .is_err()
        );
    }

    #[test]
    fn extension_replacement_clears_omitted_agent_values() {
        let mut config: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "r1",
            "allowed_harnesses": ["codex"],
            "harnesses": {
                "codex": {
                    "mcp": [{ "name": "old", "command": "old" }],
                    "package_overrides": { "old": { "enabled": false } }
                },
                "unsupported": {
                    "mcp": [{ "name": "hidden", "command": "hidden" }]
                }
            }
        }))
        .unwrap();
        replace_extensions(&mut config, Vec::new(), &BTreeMap::new(), &BTreeMap::new());
        assert!(config.harnesses["codex"].mcp.is_empty());
        assert!(config.harnesses["codex"].package_overrides.is_empty());
        assert!(config.harnesses["unsupported"].mcp.is_empty());
    }

    #[test]
    fn package_validation_accepts_standalone_codex_and_claude_skills() {
        let skill_adapter = gh_service::PackageAdapter {
            skills_dir: Some("loosely/nested/secure-code-review".into()),
            ..Default::default()
        };
        let package = gh_service::ManagedPackage {
            id: "secure-code-review".into(),
            name: None,
            version: "1".into(),
            source_ref: "https://packages.example/review.tar.gz".into(),
            artifact_id: None,
            sha256: "a".repeat(64),
            platform_sources: Default::default(),
            settings: Default::default(),
            adapters: [
                ("codex".into(), skill_adapter.clone()),
                ("claude".into(), skill_adapter),
            ]
            .into_iter()
            .collect(),
        };

        assert!(validate_packages(&[package]).is_ok());
    }

    #[test]
    fn package_validation_accepts_managed_repository_references() {
        let mut package = gh_service::ManagedPackage {
            id: "jsonl-tools".into(),
            name: None,
            version: "1".into(),
            source_ref: "bitbucket://bitbucket-blocksorg/blocksorg/skills@main".into(),
            artifact_id: Some(Uuid::new_v4().to_string()),
            sha256: "a".repeat(64),
            platform_sources: Default::default(),
            settings: Default::default(),
            adapters: Default::default(),
        };

        assert!(validate_packages(std::slice::from_ref(&package)).is_ok());
        package.artifact_id = None;
        assert!(validate_packages(&[package]).is_err());
    }

    #[test]
    fn package_source_inspection_rejects_private_hosts() {
        assert!(public_https_url("http://packages.example/archive.tar.gz").is_err());
        assert!(public_https_url("https://127.0.0.1/archive.tar.gz").is_err());
        assert!(public_https_url("https://[::1]/archive.tar.gz").is_err());
        assert!(public_https_url("https://[::127.0.0.1]/archive.tar.gz").is_err());
        assert!(public_https_url("https://[64:ff9b::7f00:1]/archive.tar.gz").is_err());
        assert!(public_https_url("https://[fec0::1]/archive.tar.gz").is_err());
        assert!(public_https_url("https://[2001:db8::1]/archive.tar.gz").is_err());
        assert!(public_https_url("https://metadata.local/archive.tar.gz").is_err());
        assert!(public_https_url("https://secret@packages.example/archive.tar.gz").is_err());
        assert!(public_https_url("https://packages.example/archive.tar.gz#fragment").is_err());
        assert!(public_https_url("https://packages.example/archive.tar.gz").is_ok());
    }

    #[test]
    fn managed_package_redirects_are_host_allowlisted_and_credentials_are_origin_bound() {
        let source = reqwest::Url::parse("https://api.example.test/archive").unwrap();
        assert!(same_url_origin(
            &source,
            &reqwest::Url::parse("https://api.example.test/other").unwrap()
        ));
        assert!(!same_url_origin(
            &source,
            &reqwest::Url::parse("https://api.example.test:8443/other").unwrap()
        ));
        assert!(!same_url_origin(
            &source,
            &reqwest::Url::parse("https://assets.example.test/archive").unwrap()
        ));

        let connection = PackageSourceConnection {
            id: "test".into(),
            name: None,
            provider: "github".into(),
            api_base_url: None,
            web_base_url: None,
            app_id: None,
            private_key: None,
            token: None,
            ca_bundle: None,
            download_hosts: vec!["assets.example.test".into()],
            organizations: Default::default(),
        };
        assert!(managed_redirect_host_allowed(
            &connection,
            "api.example.test",
            "codeload.github.com"
        ));
        assert!(managed_redirect_host_allowed(
            &connection,
            "api.example.test",
            "assets.example.test"
        ));
        assert!(!managed_redirect_host_allowed(
            &connection,
            "api.example.test",
            "attacker.example"
        ));
    }

    #[test]
    fn managed_archive_redirects_must_stay_on_https() {
        // The call site passes `cfg!(test)` so plaintext fixtures keep working,
        // which is exactly why the rule needs its own coverage here.
        let plaintext = reqwest::Url::parse("http://assets.example.test/archive").unwrap();
        let secure = reqwest::Url::parse("https://assets.example.test/archive").unwrap();
        assert!(!redirect_scheme_allowed(&plaintext, false));
        assert!(redirect_scheme_allowed(&secure, false));
        assert!(redirect_scheme_allowed(&plaintext, true));
    }

    #[tokio::test]
    async fn bitbucket_git_fetch_refuses_initial_redirect_without_forwarding_credentials() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let source = TcpListener::bind("127.0.0.1:0").unwrap();
        let source_address = source.local_addr().unwrap();
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let destination_address = destination.local_addr().unwrap();
        let source_thread = std::thread::spawn(move || {
            let (mut stream, _) = source.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{destination_address}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        let connection = PackageSourceConnection {
            id: "bitbucket-test".into(),
            name: None,
            provider: "bitbucket_cloud".into(),
            api_base_url: None,
            web_base_url: Some(format!("http://{source_address}")),
            app_id: None,
            private_key: None,
            token: Some("redirect-secret".into()),
            ca_bundle: None,
            download_hosts: vec![destination_address.ip().to_string()],
            organizations: Default::default(),
        };
        assert!(fetch_bitbucket_cloud_git_archive(
            &connection,
            "workspace/repository",
            "0123456789012345678901234567890123456789",
        )
        .await
        .is_err());
        let request = source_thread.join().unwrap();
        assert!(request.contains("Authorization: Basic "), "{request}");
        assert!(matches!(
            destination.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[tokio::test]
    async fn managed_http_redirect_strips_credentials_at_a_new_origin() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        fn capture_request(
            listener: TcpListener,
            response: String,
        ) -> std::thread::JoinHandle<String> {
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                stream.write_all(response.as_bytes()).unwrap();
                String::from_utf8_lossy(&request).into_owned()
            })
        }

        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_address = origin.local_addr().unwrap();
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination_address = destination.local_addr().unwrap();
        let origin_thread = capture_request(
            origin,
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{destination_address}/archive\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        let destination_thread = capture_request(
            destination,
            "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\narchive".into(),
        );
        let connection = PackageSourceConnection {
            id: "redirect-test".into(),
            name: None,
            provider: "bitbucket_data_center".into(),
            api_base_url: Some(format!("http://{origin_address}")),
            web_base_url: None,
            app_id: None,
            private_key: None,
            token: Some("secret".into()),
            ca_bundle: None,
            download_hosts: Vec::new(),
            organizations: Default::default(),
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let source = ResolvedProviderSource {
            source_ref: "test".into(),
            commit: "commit".into(),
            archive_url: reqwest::Url::parse(&format!("http://{origin_address}/archive")).unwrap(),
            authorization: "Bearer origin-secret".into(),
        };
        assert_eq!(
            fetch_provider_archive(&client, &connection, "project/repo", &source)
                .await
                .unwrap(),
            b"archive"
        );
        let origin_request = origin_thread.join().unwrap();
        let destination_request = destination_thread.join().unwrap();
        assert!(origin_request
            .to_ascii_lowercase()
            .contains("authorization: bearer origin-secret"));
        assert!(!destination_request
            .to_ascii_lowercase()
            .contains("authorization:"));
    }

    #[tokio::test]
    async fn managed_provider_proxy_child_helper() {
        let Some(marker) = std::env::var_os("BLUE_MANAGED_PROXY_TEST") else {
            return;
        };
        let connection = PackageSourceConnection {
            id: "proxy-test".into(),
            name: None,
            provider: "bitbucket_data_center".into(),
            api_base_url: Some("http://managed-provider.invalid".into()),
            web_base_url: None,
            app_id: None,
            private_key: None,
            token: Some("token".into()),
            ca_bundle: None,
            download_hosts: Vec::new(),
            organizations: Default::default(),
        };
        let response = provider_client(&connection)
            .unwrap()
            .get("http://managed-provider.invalid/proxy-check")
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert_eq!(marker, "1");
    }

    #[test]
    fn managed_provider_clients_honor_environment_proxy_configuration() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let proxy_thread = std::thread::spawn(move || {
            let (mut stream, _) = proxy.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        let proxy_url = format!("http://{proxy_address}");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::managed_provider_proxy_child_helper"])
            .env("BLUE_MANAGED_PROXY_TEST", "1")
            .env("HTTP_PROXY", &proxy_url)
            .env("HTTPS_PROXY", &proxy_url)
            .env("ALL_PROXY", &proxy_url)
            .env("NO_PROXY", "")
            .status()
            .unwrap();
        assert!(status.success());
        let request = proxy_thread.join().unwrap();
        assert!(request.starts_with("GET http://managed-provider.invalid/proxy-check "));
    }

    #[test]
    fn managed_repository_coordinates_are_strict_and_api_paths_are_preserved() {
        assert_eq!(
            safe_repository("acme/review-kit").unwrap(),
            ("acme", "review-kit")
        );
        assert!(safe_repository("acme/review/kit").is_err());
        assert!(safe_repository("../review-kit").is_err());
        assert_eq!(safe_ref("release/1.2").unwrap(), "release/1.2");
        assert!(safe_ref("main?token=secret").is_err());
        let url = api_url(
            "https://github.enterprise.example/api/v3",
            &["repos", "acme", "review-kit"],
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://github.enterprise.example/api/v3/repos/acme/review-kit"
        );
    }

    #[test]
    fn admin_user_filters_accept_only_contract_values() {
        assert!(validate_role_filter(Some("admin")).is_ok());
        assert!(validate_role_filter(Some("owner")).is_err());
        assert!(validate_status_filter(Some("removed")).is_ok());
        assert!(validate_status_filter(Some("disabled")).is_err());
        assert!(validate_provisioning_source_filter(Some("local")).is_ok());
        assert!(validate_provisioning_source_filter(Some("scim")).is_ok());
        assert!(validate_provisioning_source_filter(Some("sso")).is_err());
        assert!(like_filter("user_q", Some(&"a".repeat(201))).is_err());
        assert_eq!(like_filter("q", None).unwrap(), None);
        assert_eq!(like_filter("q", Some("   ")).unwrap(), None);
        assert_eq!(
            like_filter("q", Some(r"  Ada\100%_X  "))
                .unwrap()
                .as_deref(),
            Some(r"%ada\\100\%\_x%")
        );
        assert_eq!(
            like_filter("user_q", Some(&"a".repeat(201)))
                .unwrap_err()
                .message,
            "user_q must be 200 characters or fewer"
        );
    }

    #[test]
    fn admin_list_pagination_is_validated() {
        assert_eq!(validate_page(None, None).unwrap(), (1, 25, 0));
        assert_eq!(validate_page(Some(3), Some(50)).unwrap(), (3, 50, 100));
        assert!(validate_page(Some(0), Some(25)).is_err());
        assert!(validate_page(Some(1), Some(101)).is_err());
    }

    #[test]
    fn session_numbered_pagination_is_validated() {
        let user_id = Uuid::new_v4();
        let query = SessionListQuery {
            page: Some(3),
            per_page: Some(25),
            q: Some(r"  workspace\100%_ready  ".into()),
            updated_from: Some("2026-08-01".into()),
            updated_to: Some("2026-08-27".into()),
            sort: Some("updated_asc".into()),
            ..Default::default()
        };
        let (page, per_page, offset, filters) =
            validate_session_page_query(&query, Some(user_id)).unwrap();
        assert_eq!((page, per_page), (3, 25));
        assert_eq!(offset, 50);
        assert_eq!(filters.user_id, Some(user_id));
        assert_eq!(
            filters.search.as_deref(),
            Some(r"%workspace\\100\%\_ready%")
        );
        assert_eq!(filters.sort, "updated_asc");
        assert_eq!(
            filters.updated_to_exclusive.unwrap() - filters.updated_from.unwrap(),
            Duration::days(27)
        );

        let mixed = SessionListQuery {
            page: Some(1),
            cursor: Some(Uuid::new_v4()),
            ..Default::default()
        };
        assert!(validate_session_page_query(&mixed, None).is_err());
        let invalid_sort = SessionListQuery {
            page: Some(1),
            sort: Some("size_desc".into()),
            ..Default::default()
        };
        assert!(validate_session_page_query(&invalid_sort, None).is_err());
    }

    #[test]
    fn session_page_counts_cover_empty_and_partial_pages() {
        assert_eq!(page_count(0, 25), 0);
        assert_eq!(page_count(1, 25), 1);
        assert_eq!(page_count(25, 25), 1);
        assert_eq!(page_count(26, 25), 2);
        assert_eq!(page_count(200, 25), 8);
    }

    #[test]
    fn client_status_pagination_and_filters_are_validated() {
        let user_id = Uuid::new_v4();
        let query = ClientStatusListQuery {
            page: Some(4),
            per_page: Some(50),
            q: Some(r"  laptop\100%_ready  ".into()),
            user_id: Some(user_id),
            harness: Some("codex".into()),
            health: Some("attention".into()),
            last_seen_from: Some("2026-08-01".into()),
            last_seen_to: Some("2026-08-27".into()),
            sort: Some("last_seen_asc".into()),
        };
        let (page, per_page, offset, filters) = validate_client_status_page_query(&query).unwrap();
        assert_eq!((page, per_page, offset), (4, 50, 150));
        assert_eq!(filters.user_id, Some(user_id));
        assert_eq!(filters.harness.as_deref(), Some("codex"));
        assert_eq!(filters.health.as_deref(), Some("attention"));
        assert_eq!(filters.search.as_deref(), Some(r"%laptop\\100\%\_ready%"));
        assert_eq!(filters.sort, "last_seen_asc");
        assert_eq!(
            filters.last_seen_to_exclusive.unwrap() - filters.last_seen_from.unwrap(),
            Duration::days(27)
        );

        let defaults =
            validate_client_status_page_query(&ClientStatusListQuery::default()).unwrap();
        assert_eq!((defaults.0, defaults.1, defaults.2), (1, 25, 0));
        assert_eq!(defaults.3.sort, "last_seen_desc");

        let outdated = validate_client_status_page_query(&ClientStatusListQuery {
            health: Some("outdated".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(outdated.3.health.as_deref(), Some("outdated"));
    }

    #[test]
    fn client_health_status_separates_revision_lag_from_mismatch() {
        assert_eq!(
            client_health_status(Some("r2"), Some(true), Some("r2")),
            "current"
        );
        assert_eq!(
            client_health_status(Some("r1"), Some(true), Some("r2")),
            "outdated"
        );
        assert_eq!(client_health_status(None, None, Some("r2")), "outdated");
        assert_eq!(
            client_health_status(Some("r1"), Some(false), Some("r2")),
            "attention"
        );
        assert_eq!(
            client_health_status(Some("r2"), Some(false), Some("r2")),
            "attention"
        );
        assert_eq!(
            client_health_status(None, Some(false), Some("r2")),
            "attention"
        );
    }

    #[test]
    fn client_activity_becomes_stale_at_thirty_days() {
        let now = OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap();
        assert_eq!(
            client_activity_status(now - Duration::days(30) + Duration::seconds(1), now),
            "recent"
        );
        assert_eq!(
            client_activity_status(now - Duration::days(30), now),
            "stale"
        );
    }

    #[test]
    fn reconciliation_scope_ignores_untouched_harnesses_per_revision() {
        let input: ClientStatusRequest = serde_json::from_value(json!({
            "instance_id": "client-1",
            "client_version": "0.1.0",
            "platform": "linux",
            "config_revision": "r2",
            "applied": true,
            "files_ok": true,
            "harnesses": [
                {"name":"codex","reconciled":true,"installed":true},
                {"name":"kimi","reconciled":false,"installed":true},
                {"name":"opencode","reconciled":false,"installed":true}
            ]
        }))
        .unwrap();
        let allowed = BTreeSet::from(["codex", "kimi", "opencode"]);

        assert_eq!(
            reconciliation_scope(&input, "r2", &allowed),
            BTreeSet::from(["codex".to_owned()])
        );
        assert!(reconciliation_scope(&input, "r3", &allowed).is_empty());
    }

    #[test]
    fn reconciliation_scope_includes_failed_attempt_and_legacy_reports() {
        let attempted: ClientStatusRequest = serde_json::from_value(json!({
            "instance_id": "client-1",
            "client_version": "0.1.0",
            "platform": "linux",
            "config_revision": null,
            "applied": false,
            "files_ok": false,
            "harnesses": [{"name":"kimi","reconciled":false,"installed":true}],
            "reconciliation_attempt": {"harness":"kimi","revision":"r2"},
            "error": "extension activation failed"
        }))
        .unwrap();
        let allowed = BTreeSet::from(["codex", "kimi"]);
        assert_eq!(
            reconciliation_scope(&attempted, "r2", &allowed),
            BTreeSet::from(["kimi".to_owned()])
        );

        let legacy: ClientStatusRequest = serde_json::from_value(json!({
            "instance_id": "client-2",
            "client_version": "0.1.0",
            "platform": "linux",
            "config_revision": "r2",
            "applied": true,
            "files_ok": true,
            "harnesses": ["codex"]
        }))
        .unwrap();
        assert_eq!(
            reconciliation_scope(&legacy, "r2", &allowed),
            BTreeSet::from(["codex".to_owned()])
        );
    }

    #[test]
    fn client_revision_helpers_normalize_versions_and_platform_digests() {
        let report = json!({
            "raw_version": "codex-cli v1.2.3",
            "version": "1.2.3"
        });
        assert_eq!(
            reported_version(&report).0,
            Some(semver::Version::new(1, 2, 3))
        );

        let mut package = gh_service::ManagedPackage {
            id: "review-kit".into(),
            name: None,
            version: "1".into(),
            source_ref: "https://packages.example/review.tar.gz".into(),
            artifact_id: None,
            sha256: "a".repeat(64),
            platform_sources: Default::default(),
            settings: Default::default(),
            adapters: Default::default(),
        };
        assert_eq!(
            expected_package_digests(&package, "macos", Some("aarch64")),
            BTreeSet::from(["a".repeat(64)])
        );
        package.platform_sources.insert(
            "macos-aarch64".into(),
            gh_service::PackageSource {
                source_ref: "https://packages.example/review-macos.tar.gz".into(),
                artifact_id: None,
                sha256: "b".repeat(64),
            },
        );
        package.platform_sources.insert(
            "linux-x86_64".into(),
            gh_service::PackageSource {
                source_ref: "https://packages.example/review-linux.tar.gz".into(),
                artifact_id: None,
                sha256: "c".repeat(64),
            },
        );
        assert_eq!(
            expected_package_digests(&package, "macos", Some("aarch64")),
            BTreeSet::from(["b".repeat(64)])
        );
        assert_eq!(
            expected_package_digests(&package, "macos", None),
            BTreeSet::from(["b".repeat(64)])
        );
        assert!(expected_package_digests(&package, "windows", Some("x86_64")).is_empty());
    }

    #[test]
    fn client_status_filters_reject_invalid_values() {
        assert!(validate_client_status_page_query(&ClientStatusListQuery {
            health: Some("offline".into()),
            ..Default::default()
        })
        .is_err());
        assert!(validate_client_status_page_query(&ClientStatusListQuery {
            sort: Some("hostname_asc".into()),
            ..Default::default()
        })
        .is_err());
        assert!(validate_client_status_page_query(&ClientStatusListQuery {
            last_seen_from: Some("2026-08-28".into()),
            last_seen_to: Some("2026-08-27".into()),
            ..Default::default()
        })
        .is_err());
        assert!(validate_client_status_page_query(&ClientStatusListQuery {
            per_page: Some(101),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn invitation_email_is_normalized_and_validated() {
        assert_eq!(
            normalize_email(" Developer@Example.COM ").unwrap(),
            "developer@example.com"
        );
        assert!(normalize_email("developer at example.com").is_err());
        assert!(normalize_email("developer@localhost").is_err());
    }

    #[test]
    fn session_upload_profiles_must_belong_to_the_reported_harness() {
        assert!(upload_profile_is_known("claude", "claude-v1"));
        assert!(upload_profile_is_known("claude", "legacy"));
        assert!(!upload_profile_is_known("claude", "codex-v1"));
        assert!(!upload_profile_is_known("unknown", "legacy"));
    }

    #[test]
    fn identity_settings_are_provider_neutral_and_strict() {
        assert_eq!(identity_mode("OIDC").unwrap(), "oidc");
        assert!(identity_mode("saml").is_err());
        let mappings =
            role_mappings(Some(r#"{"Blue Admins":"admin","Everyone":"member"}"#)).unwrap();
        assert_eq!(mappings["Blue Admins"], "admin");
        assert!(role_mappings(Some(r#"{"Owners":"owner"}"#)).is_err());
    }

    #[test]
    fn package_audiences_filter_packages_and_matching_overrides() {
        let selected_user = Uuid::new_v4();
        let other_user = Uuid::new_v4();
        let mut config: gh_service::GovernanceConfig = serde_json::from_value(json!({
            "revision": "r1",
            "packages": [
                {"id":"everyone","version":"1","source_ref":"https://packages.example/everyone.tar.gz","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                {"id":"selected","version":"1","source_ref":"https://packages.example/selected.tar.gz","sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
            ],
            "harnesses": {"codex":{"package_overrides": {
                "everyone":{"enabled":true},
                "selected":{"enabled":true}
            }}}
        }))
        .unwrap();
        let audiences = BTreeMap::from([(
            "selected".into(),
            PackageAudience {
                scope: PackageAudienceScope::Users,
                user_ids: vec![selected_user],
            },
        )]);

        let mut selected_config = config.clone();
        apply_package_audiences(selected_user, &mut selected_config, &audiences);
        assert_eq!(selected_config.packages.len(), 2);

        apply_package_audiences(other_user, &mut config, &audiences);
        assert_eq!(config.packages.len(), 1);
        assert_eq!(config.packages[0].id, "everyone");
        assert!(config.harnesses["codex"]
            .package_overrides
            .contains_key("everyone"));
        assert!(!config.harnesses["codex"]
            .package_overrides
            .contains_key("selected"));
    }

    /// A fresh deployment starts control-api and the worker at the same moment,
    /// and both seed the same rows. These cover the serialization that keeps the
    /// loser of that race from exiting on a unique violation.
    #[cfg(feature = "postgres-tests")]
    mod cold_start {
        use std::time::Duration;

        use sqlx::PgPool;

        use crate::{acquire_bootstrap_lock, bootstrap_identity, AppConfig};

        fn bootstrap_config(org: &str) -> AppConfig {
            let mut config = super::config_with_gateway_runtime(None);
            config.bootstrap_org = org.to_owned();
            config.bootstrap_org_name = "Cold Start".into();
            config.bootstrap_admin_sub = "admin-subject".into();
            config.bootstrap_admin_email = "admin@example.com".into();
            config
        }

        #[sqlx::test(migrations = false)]
        async fn the_bootstrap_lock_excludes_a_second_process(pool: PgPool) {
            crate::db::migrate_pool(&pool).await.unwrap();
            let held = acquire_bootstrap_lock(&pool, "cold-start").await.unwrap();

            assert!(
                tokio::time::timeout(
                    Duration::from_millis(250),
                    acquire_bootstrap_lock(&pool, "cold-start"),
                )
                .await
                .is_err(),
                "a second process entered bootstrap while the first still held the lock"
            );
            // Keyed per deployment: another organization's bootstrap never waits.
            tokio::time::timeout(
                Duration::from_secs(10),
                acquire_bootstrap_lock(&pool, "other-deployment"),
            )
            .await
            .expect("an unrelated organization waited on this lock")
            .unwrap()
            .rollback()
            .await
            .unwrap();

            held.rollback().await.unwrap();
            tokio::time::timeout(
                Duration::from_secs(10),
                acquire_bootstrap_lock(&pool, "cold-start"),
            )
            .await
            .expect("the lock outlived the transaction that held it")
            .unwrap()
            .rollback()
            .await
            .unwrap();
        }

        #[sqlx::test(migrations = false)]
        async fn concurrent_bootstrap_seeds_a_single_identity(pool: PgPool) {
            crate::db::migrate_pool(&pool).await.unwrap();

            let identities = futures_util::future::join_all((0..3).map(|_| {
                let pool = pool.clone();
                let config = bootstrap_config("cold-start");
                async move {
                    let lock = acquire_bootstrap_lock(&pool, &config.bootstrap_org)
                        .await
                        .unwrap();
                    let identity = bootstrap_identity(&pool, &config).await;
                    lock.rollback().await.unwrap();
                    identity.expect("bootstrap failed under concurrency")
                }
            }))
            .await;

            assert!(identities.iter().all(|identity| *identity == identities[0]));
            let organizations = sqlx::query_scalar!(
                "select count(*) as \"count!\" from organizations where slug=$1",
                "cold-start"
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(organizations, 1);
            let admins = sqlx::query_scalar!(
                "select count(*) as \"count!\" from users where organization_id=$1",
                identities[0].0
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(admins, 1);
        }

        #[sqlx::test(migrations = false)]
        async fn the_seeded_governance_baseline_survives_a_second_writer(pool: PgPool) {
            crate::db::migrate_pool(&pool).await.unwrap();
            let config = bootstrap_config("cold-start");
            let (org_id, _) = bootstrap_identity(&pool, &config).await.unwrap();
            let baseline = serde_json::json!({"revision": "seed"});
            let digest = "0".repeat(64);

            crate::upsert_deployment_governance_state(&pool, org_id, &baseline, &digest)
                .await
                .unwrap();
            crate::upsert_deployment_governance_state(&pool, org_id, &baseline, &digest)
                .await
                .expect("re-seeding the deployment baseline must not conflict");
        }
    }
}

#[cfg(test)]
mod service_token_tests {
    use super::*;

    fn user_principal(role: &str, scopes: &[&str]) -> Principal {
        Principal {
            user_id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            subject: "subject".into(),
            email: "user@example.com".into(),
            role: role.into(),
            expires_at: OffsetDateTime::now_utc() + Duration::hours(1),
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            oauth_session_id: None,
            issued_at: None,
            bearer_token: true,
        }
    }

    #[test]
    fn user_guard_policies_match_route_requirements() {
        let member = user_principal("member", &["governance:read"]);
        assert!(authorize_user_policy(&member, UserPolicy::Authenticated).is_ok());
        assert!(authorize_user_policy(&member, UserPolicy::Scope("governance:read")).is_ok());
        assert_eq!(
            authorize_user_policy(&member, UserPolicy::Scope("session:write"))
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            authorize_user_policy(&member, UserPolicy::Admin)
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        assert!(authorize_user_policy(&user_principal("admin", &[]), UserPolicy::Admin).is_ok());
    }

    fn claims(sub: &str, scope: &str) -> ServiceClaims {
        ServiceClaims {
            sub: sub.into(),
            client_id: sub.into(),
            azp: sub.into(),
            exp: 0,
            scope: scope.into(),
        }
    }

    #[test]
    fn only_the_inference_proxy_service_identity_is_authorized() {
        let allowed = "blue-inference-proxy";
        // Correct subject + scope (with or without extra scopes) is authorized.
        assert!(authorize_service_claims(&claims(allowed, "gateway:resolve"), allowed).is_ok());
        assert!(
            authorize_service_claims(&claims(allowed, "openid gateway:resolve foo"), allowed)
                .is_ok()
        );
        // A normal user's bearer token remains unauthorized even if the user
        // somehow obtains the internal scope. Authorization is pinned to the
        // inference proxy's OAuth client id, not merely to the scope string.
        assert_eq!(
            authorize_service_claims(&claims("user_01JEXAMPLE", "gateway:resolve"), allowed)
                .unwrap_err()
                .status,
            StatusCode::UNAUTHORIZED
        );
        let mut wrong_client_id = claims(allowed, "gateway:resolve");
        wrong_client_id.client_id = "other-client".into();
        assert_eq!(
            authorize_service_claims(&wrong_client_id, allowed)
                .unwrap_err()
                .status,
            StatusCode::UNAUTHORIZED
        );
        let mut wrong_authorized_party = claims(allowed, "gateway:resolve");
        wrong_authorized_party.azp = "other-client".into();
        assert_eq!(
            authorize_service_claims(&wrong_authorized_party, allowed)
                .unwrap_err()
                .status,
            StatusCode::UNAUTHORIZED
        );
        // Valid subject but missing scope → 403.
        assert_eq!(
            authorize_service_claims(&claims(allowed, "governance:read"), allowed)
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            authorize_service_claims(&claims(allowed, ""), allowed)
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
    }

    // Asserts the issuer/audience/expiry contract `authorize_internal` relies on.
    // Uses HS256 so the test needs no asymmetric keypair; only the
    // signature-independent claim validation is exercised here.
    #[derive(Serialize)]
    struct SignedClaims<'a> {
        sub: &'a str,
        client_id: &'a str,
        azp: &'a str,
        scope: &'a str,
        iss: &'a str,
        aud: &'a str,
        exp: i64,
    }

    const ISSUER: &str = "https://issuer.example";
    const AUDIENCE: &str = "https://audience.example";
    // 2100-01-01T00:00:00Z, comfortably past any realistic run time.
    const FAR_FUTURE: i64 = 4_102_444_800;
    const SECRET: &[u8] = b"service-token-test-secret";

    fn validation() -> Validation {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[AUDIENCE]);
        validation
    }

    fn sign(sub: &str, iss: &str, aud: &str, exp: i64) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &SignedClaims {
                sub,
                client_id: sub,
                azp: sub,
                scope: "gateway:resolve",
                iss,
                aud,
                exp,
            },
            &EncodingKey::from_secret(SECRET),
        )
        .expect("signing test token")
    }

    #[test]
    fn validation_accepts_matching_issuer_audience_and_future_expiry() {
        let token = sign("blue-inference-proxy", ISSUER, AUDIENCE, FAR_FUTURE);
        let decoded =
            decode::<ServiceClaims>(&token, &DecodingKey::from_secret(SECRET), &validation());
        assert!(decoded.is_ok());
        assert_eq!(decoded.unwrap().claims.sub, "blue-inference-proxy");
    }

    #[test]
    fn validation_rejects_wrong_audience_issuer_and_expiry() {
        let key = DecodingKey::from_secret(SECRET);
        let wrong_aud = sign("x", ISSUER, "https://evil.example", FAR_FUTURE);
        assert!(decode::<ServiceClaims>(&wrong_aud, &key, &validation()).is_err());
        let wrong_iss = sign("x", "https://evil.example", AUDIENCE, FAR_FUTURE);
        assert!(decode::<ServiceClaims>(&wrong_iss, &key, &validation()).is_err());
        let expired = sign("x", ISSUER, AUDIENCE, 1);
        assert!(decode::<ServiceClaims>(&expired, &key, &validation()).is_err());
    }
}
/// Run the embedded database migrations without constructing the application or
/// starting a listener. Deployment tooling invokes this before rolling out new
/// replicas.
pub async fn migrate_database_from_env() -> anyhow::Result<()> {
    let database_url = std::env::var("HARNESS_DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("HARNESS_DATABASE_URL is required"))?;
    db::migrate(&database_url)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

#[cfg(test)]
mod gateway_ttl_config_tests {
    use super::*;

    #[test]
    fn the_shipped_gateway_overlay_declares_a_parsable_token_ttl() {
        let overlay: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../../../deploy/blue.gateway.yaml")).unwrap();
        let settings = Some(overlay);
        // Reading through the same helper AppConfig uses, so a typo in the key
        // path or a non-integer value fails here rather than at boot.
        assert_eq!(
            positive_setting(
                "HARNESS_GATEWAY_INFERENCE_TOKEN_TTL_SECONDS__UNSET",
                &settings,
                &["gateway", "inference_jwt", "token_ttl_seconds"],
                1,
            )
            .unwrap(),
            43_200
        );
    }
}
