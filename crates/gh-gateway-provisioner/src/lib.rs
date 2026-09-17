//! Compile-time authoring contract for Blue gateway provisioners.
//!
//! Implement [`GatewayProvisioner`] in a library crate and pass it to
//! `control_api::serve_with_provisioner`. Provisioners execute in-process; no
//! network server or wire protocol is involved.

#[cfg(feature = "litellm")]
pub mod litellm;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

impl Serialize for SecretString {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserIdentity {
    pub id: String,
    pub email: String,
    pub organization_id: String,
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnsureReason {
    Missing,
    ConfigurationChanged,
    ReconciliationDue,
    CredentialInvalidated,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreviousCredential {
    pub external_id: String,
    pub alias: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnsureRequest {
    pub identity: UserIdentity,
    pub reason: EnsureReason,
    pub previous: Option<PreviousCredential>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProvisionedCredential {
    /// Present for a newly created or rotated credential. Omit only when the
    /// previous credential remains valid; Blue will retain its encrypted value.
    pub credential: Option<SecretString>,
    pub external_id: String,
    pub alias: String,
    #[serde(default)]
    pub metadata: Value,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevokeRequest {
    pub identity: UserIdentity,
    pub external_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevokeResponse {
    pub revoked: bool,
}

/// A provider-neutral model returned by gateway discovery.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub models: Vec<DiscoveredModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
}

#[derive(Debug, Error)]
pub enum ProvisionerError {
    #[error("invalid provisioning configuration: {0}")]
    InvalidConfig(String),
    #[error("gateway account is not provisioned: {0}")]
    AccountMissing(String),
    #[error("gateway provisioning conflict: {0}")]
    Conflict(String),
    #[error("gateway credential is no longer valid: {0}")]
    CredentialInvalid(String),
    #[error("gateway is unavailable: {0}")]
    Unavailable(String),
    #[error("gateway rejected provisioning: {0}")]
    Rejected(String),
    #[error("gateway model discovery is unsupported")]
    DiscoveryUnsupported,
    #[error("gateway rejected model discovery authentication: {0}")]
    DiscoveryAuth(String),
    #[error("gateway returned an invalid model catalog: {0}")]
    DiscoveryResponse(String),
}

#[async_trait]
pub trait GatewayProvisioner: Send + Sync + 'static {
    /// Stable name selected by `gateway.provisioner.type` in `blue.yaml`.
    fn kind(&self) -> &str;

    /// Revision of the provisioner's policy. Changing it causes existing
    /// credentials to be reconciled even when `blue.yaml` is unchanged.
    fn policy_revision(&self) -> Option<&str> {
        None
    }

    /// Discover the gateway's current model catalog. Provisioners predating
    /// this capability remain source-compatible and explicitly report that
    /// discovery is unsupported.
    async fn list_models(&self) -> Result<ModelCatalog, ProvisionerError> {
        Err(ProvisionerError::DiscoveryUnsupported)
    }

    async fn ensure(
        &self,
        request: EnsureRequest,
    ) -> Result<ProvisionedCredential, ProvisionerError>;
    async fn revoke(&self, request: RevokeRequest) -> Result<RevokeResponse, ProvisionerError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_are_redacted() {
        assert_eq!(
            format!("{:?}", SecretString::new("sk-secret")),
            "SecretString([REDACTED])"
        );
    }
}
