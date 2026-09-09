use gh_common::GhError;
use gh_service::GatewayConfig;

use crate::{GatewayAdapter, GatewayRoute, InvalidCredentialReason, UpstreamCredentialPlacement};

/// LiteLLM's client-facing routing adapter.
///
/// Every harness points at Blue's inference proxy rather than LiteLLM itself.
/// The proxy replaces the pseudotoken with the user's LiteLLM virtual key.
pub(crate) struct LiteLlmAdapter;

impl GatewayAdapter for LiteLlmAdapter {
    fn kind(&self) -> &'static str {
        "litellm"
    }

    fn client_route(&self, gateway: &GatewayConfig) -> Result<GatewayRoute, GhError> {
        let base_url = gateway
            .proxy_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| GhError::config("gateway runtime config is missing proxy_url"))?
            .trim_end_matches('/')
            .to_owned();
        let token = gateway
            .pseudotoken
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| GhError::config("gateway runtime config is missing pseudotoken"))?
            .to_owned();

        Ok(GatewayRoute { base_url, token })
    }

    fn upstream_path(&self, path_and_query: &str) -> Result<String, GhError> {
        Ok(path_and_query.to_owned())
    }

    fn upstream_credential_placement(&self) -> UpstreamCredentialPlacement {
        UpstreamCredentialPlacement::AuthorizationBearer
    }

    fn inspect_response_status(&self, status: u16) -> bool {
        status == 401
    }

    fn classify_invalid_credential(
        &self,
        status: u16,
        body: &[u8],
    ) -> Option<InvalidCredentialReason> {
        if status != 401 {
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        let kind = value
            .pointer("/error/type")
            .or_else(|| value.pointer("/detail/error/type"))
            .and_then(serde_json::Value::as_str);
        if kind == Some("token_not_found_in_db") {
            return Some(InvalidCredentialReason::NotFound);
        }
        let message = value
            .pointer("/error/message")
            .or_else(|| value.pointer("/detail/error/message"))
            .or_else(|| value.get("detail"))
            .and_then(serde_json::Value::as_str)?;
        message
            .to_ascii_lowercase()
            .contains("key is blocked")
            .then_some(InvalidCredentialReason::Blocked)
    }
}
