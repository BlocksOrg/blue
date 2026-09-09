use crate::{
    EnsureRequest, GatewayProvisioner, ProvisionedCredential, ProvisionerError, RevokeRequest,
    RevokeResponse, SecretString,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct LiteLlmProvisioner {
    base_url: String,
    admin_key: String,
    http: reqwest::Client,
}

/// The bundled provisioner is an example deployment policy. Fork or edit this
/// function when building your Control API to encode your own LiteLLM models,
/// team selection, budgets, resets, and rate limits in the provisioner itself.
struct KeyPolicy {
    models: Vec<String>,
    team_scope: bool,
    max_budget: Option<f64>,
    budget_duration: Option<String>,
    rpm_limit: Option<i64>,
    tpm_limit: Option<i64>,
    max_parallel_requests: Option<i64>,
    duration: Option<String>,
}

fn policy_for(_request: &EnsureRequest) -> KeyPolicy {
    KeyPolicy {
        models: Vec::new(),
        team_scope: false,
        max_budget: None,
        budget_duration: None,
        rpm_limit: None,
        tpm_limit: None,
        max_parallel_requests: None,
        duration: None,
    }
}

impl LiteLlmProvisioner {
    pub fn new(
        base_url: impl Into<String>,
        admin_key: impl Into<String>,
    ) -> Result<Self, ProvisionerError> {
        let base_url = base_url.into();
        if !base_url.starts_with("https://")
            && std::env::var("BLUE_ALLOW_INSECURE_DEV").as_deref() != Ok("true")
        {
            return Err(ProvisionerError::InvalidConfig(
                "LiteLLM URL must use HTTPS".into(),
            ));
        }
        let admin_key = admin_key.into();
        if admin_key.trim().is_empty() {
            return Err(ProvisionerError::InvalidConfig(
                "HARNESS_LITELLM_ADMIN_KEY is required".into(),
            ));
        }
        Ok(Self {
            base_url,
            admin_key,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|error| ProvisionerError::InvalidConfig(error.to_string()))?,
        })
    }

    fn rejection_message(&self, status: reqwest::StatusCode, body: &[u8]) -> String {
        let detail = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("detail")
                    .and_then(Value::as_str)
                    .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
                    .or_else(|| value.get("error").and_then(Value::as_str))
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .map(str::to_owned)
            });
        let Some(mut detail) = detail else {
            return format!("gateway returned HTTP {status}");
        };
        if !self.admin_key.is_empty() {
            detail = detail.replace(&self.admin_key, "[REDACTED]");
        }
        let detail = detail
            .split_whitespace()
            .map(|word| {
                if word.starts_with("sk-") || word.starts_with("Bearer") {
                    "[REDACTED]"
                } else {
                    word
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "gateway returned HTTP {status}: {}",
            detail.chars().take(500).collect::<String>()
        )
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ProvisionerError> {
        let url = format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(&self.admin_key)
            .header("accept", "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| ProvisionerError::Unavailable("request failed".into()))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|_| {
            ProvisionerError::Rejected("gateway returned an unreadable response".into())
        })?;
        if path.trim_matches('/') == "key/delete" && status == reqwest::StatusCode::NOT_FOUND {
            return Ok(json!({"deleted": false}));
        }
        if !status.is_success() {
            return Err(ProvisionerError::Rejected(
                self.rejection_message(status, &bytes),
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| ProvisionerError::Rejected("gateway returned invalid JSON".into()))
    }

    async fn user(&self, email: &str) -> Result<(String, Vec<String>), ProvisionerError> {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/user/list", self.base_url.trim_end_matches('/')),
            &[("user_email", email), ("page_size", "100")],
        )
        .map_err(|e| ProvisionerError::InvalidConfig(e.to_string()))?;
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.admin_key)
            .send()
            .await
            .map_err(|_| ProvisionerError::Unavailable("user lookup failed".into()))?;
        let value: Value = response
            .json()
            .await
            .map_err(|_| ProvisionerError::Rejected("invalid user response".into()))?;
        let users = value
            .get("users")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|u| {
                u.get("user_email")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.eq_ignore_ascii_case(email))
            })
            .collect::<Vec<_>>();
        if users.is_empty() {
            return Err(ProvisionerError::AccountMissing(email.into()));
        }
        if users.len() != 1 {
            return Err(ProvisionerError::Conflict(format!(
                "multiple users match {email}"
            )));
        }
        let id = users[0]
            .get("user_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ProvisionerError::Rejected("user has no id".into()))?
            .to_owned();
        let teams = users[0]
            .get("teams")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|t| {
                t.get("team_id")
                    .or_else(|| t.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        Ok((id, teams))
    }

    async fn existing_key_invalid(&self, external_id: &str) -> Result<bool, ProvisionerError> {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/key/info", self.base_url.trim_end_matches('/')),
            &[("key", external_id)],
        )
        .map_err(|e| ProvisionerError::InvalidConfig(e.to_string()))?;
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.admin_key)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|_| ProvisionerError::Unavailable("key lookup failed".into()))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|_| {
            ProvisionerError::Rejected("gateway returned an unreadable key response".into())
        })?;
        let value = serde_json::from_slice::<Value>(&bytes).ok();
        // Only the management endpoint's 404 identifies the queried key as
        // missing. A 401 token_not_found_in_db here may instead identify a
        // broken admin credential and must not invalidate every managed key.
        let missing = status == reqwest::StatusCode::NOT_FOUND;
        if missing {
            return Ok(true);
        }
        if !status.is_success() {
            return Err(ProvisionerError::Rejected(
                self.rejection_message(status, &bytes),
            ));
        }
        let value = value.ok_or_else(|| {
            ProvisionerError::Rejected("gateway returned invalid key JSON".into())
        })?;
        Ok(
            value.pointer("/info/blocked").and_then(Value::as_bool) == Some(true)
                || value.get("blocked").and_then(Value::as_bool) == Some(true),
        )
    }

    async fn available_alias(&self, base: &str, user_id: &str) -> Result<String, ProvisionerError> {
        // One bounded list request supplies every candidate we inspect. Avoid
        // turning a full alias range into one upstream request per suffix.
        let url = reqwest::Url::parse_with_params(
            &format!("{}/key/list", self.base_url.trim_end_matches('/')),
            &[
                ("user_id", user_id),
                ("return_full_object", "true"),
                ("size", "100"),
            ],
        )
        .map_err(|e| ProvisionerError::InvalidConfig(e.to_string()))?;
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.admin_key)
            .send()
            .await
            .map_err(|_| ProvisionerError::Unavailable("key alias lookup failed".into()))?;
        if !response.status().is_success() {
            return Err(ProvisionerError::Rejected(format!(
                "gateway returned HTTP {} while checking key aliases",
                response.status()
            )));
        }
        let value: Value = response.json().await.map_err(|_| {
            ProvisionerError::Rejected("gateway returned invalid key list JSON".into())
        })?;
        let keys = value.get("keys").and_then(Value::as_array);
        for suffix in 1..=100 {
            let candidate = if suffix == 1 {
                base.to_owned()
            } else {
                format!("{base}-{suffix}")
            };
            let occupied = keys
                .into_iter()
                .flatten()
                .any(|key| key.get("key_alias").and_then(Value::as_str) == Some(&candidate));
            if !occupied {
                return Ok(candidate);
            }
        }
        Err(ProvisionerError::Conflict(format!(
            "no available LiteLLM key alias for {base}"
        )))
    }
}

#[async_trait]
impl GatewayProvisioner for LiteLlmProvisioner {
    fn kind(&self) -> &'static str {
        "builtin-litellm"
    }

    async fn ensure(
        &self,
        request: EnsureRequest,
    ) -> Result<ProvisionedCredential, ProvisionerError> {
        let policy = policy_for(&request);
        let (user_id, teams) = self.user(&request.identity.email).await?;
        let team_id = match (policy.team_scope, teams.len()) {
            (false, _) => None,
            (true, 1) => Some(teams[0].clone()),
            (true, _) => {
                return Err(ProvisionerError::Conflict(format!(
                    "team scope requires exactly one team; found {}",
                    teams.len()
                )))
            }
        };
        let base_alias = format!(
            "blue:{}",
            request.identity.email.trim().to_ascii_lowercase()
        );
        if let Some(previous) = &request.previous {
            if self.existing_key_invalid(&previous.external_id).await? {
                return Err(ProvisionerError::CredentialInvalid(
                    "LiteLLM key was deleted or blocked".into(),
                ));
            }
            let alias = previous.alias.clone();
            let mut body = json!({
                "key": previous.external_id,
                "user_id": user_id,
                "key_alias": alias,
                "models": policy.models,
                "metadata": {
                    "managed_by":"blue",
                    "owner_email":request.identity.email.trim().to_ascii_lowercase(),
                    "provisioner":"builtin-litellm"
                }
            });
            for (name, value) in [
                ("max_budget", policy.max_budget.map(Value::from)),
                ("budget_duration", policy.budget_duration.map(Value::from)),
                ("rpm_limit", policy.rpm_limit.map(Value::from)),
                ("tpm_limit", policy.tpm_limit.map(Value::from)),
                (
                    "max_parallel_requests",
                    policy.max_parallel_requests.map(Value::from),
                ),
                ("duration", policy.duration.map(Value::from)),
                ("team_id", team_id.clone().map(Value::from)),
            ] {
                if let Some(value) = value {
                    body[name] = value;
                }
            }
            self.call(reqwest::Method::POST, "/key/update", Some(body))
                .await?;
            return Ok(ProvisionedCredential {
                credential: None,
                external_id: previous.external_id.clone(),
                alias,
                metadata: json!({"team_id":team_id,"models":policy.models}),
                expires_at: None,
            });
        }
        let alias = self.available_alias(&base_alias, &user_id).await?;
        let mut body = json!({
            "user_id": user_id, "key_alias": alias, "models": policy.models,
            "metadata": {"managed_by":"blue", "owner_email":request.identity.email.trim().to_ascii_lowercase(), "provisioner":"builtin-litellm"},
            "key_type":"llm_api"
        });
        for (name, value) in [
            ("max_budget", policy.max_budget.map(Value::from)),
            ("budget_duration", policy.budget_duration.map(Value::from)),
            ("rpm_limit", policy.rpm_limit.map(Value::from)),
            ("tpm_limit", policy.tpm_limit.map(Value::from)),
            (
                "max_parallel_requests",
                policy.max_parallel_requests.map(Value::from),
            ),
            ("duration", policy.duration.map(Value::from)),
            ("team_id", team_id.clone().map(Value::from)),
        ] {
            if let Some(value) = value {
                body[name] = value;
            }
        }
        let generated = self
            .call(reqwest::Method::POST, "/key/generate", Some(body))
            .await?;
        let key = generated
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProvisionerError::Rejected("key generation returned no credential".into())
            })?
            .to_owned();
        let external_id = generated
            .get("token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| hex::encode(Sha256::digest(key.as_bytes())));
        if let Some(previous) = request.previous.filter(|p| p.external_id != external_id) {
            if let Err(error) = self
                .call(
                    reqwest::Method::POST,
                    "/key/delete",
                    Some(json!({"keys":[previous.external_id]})),
                )
                .await
            {
                tracing::warn!(%error, "failed to revoke superseded LiteLLM key");
            }
        }
        Ok(ProvisionedCredential {
            credential: Some(SecretString::new(key)),
            external_id,
            alias,
            metadata: json!({"team_id":team_id,"models":policy.models}),
            expires_at: generated
                .get("expires")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    async fn revoke(&self, request: RevokeRequest) -> Result<RevokeResponse, ProvisionerError> {
        self.call(
            reqwest::Method::POST,
            "/key/delete",
            Some(json!({"keys":[request.external_id]})),
        )
        .await?;
        Ok(RevokeResponse { revoked: true })
    }
}
