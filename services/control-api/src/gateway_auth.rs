//! Session-bound inference JWT issuance and verification-key publication.

use axum::extract::State;
use axum::Json;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{encode, Algorithm, Header};
use serde::Serialize;
use sqlx::PgPool;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::{ApiError, AppState, Principal};

#[derive(Serialize)]
struct GatewayInferenceClaims {
    iss: String,
    aud: String,
    sub: String,
    iat: i64,
    exp: i64,
    jti: String,
    blue_oauth_session_id: String,
    scope: String,
}

pub(crate) async fn renew_gateway_auth_session(
    pool: &PgPool,
    oauth_session_id: &str,
    subject: &str,
    user_id: Uuid,
) -> Result<Option<OffsetDateTime>, ApiError> {
    let mut transaction = pool.begin().await?;
    let source_expires_at: Option<OffsetDateTime> = sqlx::query_scalar!(
        "update auth.\"session\" set \"expiresAt\"=now() + interval '12 hours', \"updatedAt\"=now() where id=$1 and \"userId\"=$2 and \"expiresAt\">now() returning \"expiresAt\"",
        oauth_session_id,
        subject
    )
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(source_expires_at) = source_expires_at else {
        return Ok(None);
    };
    let active: Option<OffsetDateTime> = sqlx::query_scalar!(
        "insert into public.gateway_auth_sessions(oauth_session_id,user_id,source_expires_at) values($1,$2,$3) on conflict(oauth_session_id) do update set source_expires_at=excluded.source_expires_at,updated_at=now() where gateway_auth_sessions.user_id=excluded.user_id and gateway_auth_sessions.revoked_at is null returning source_expires_at",
        oauth_session_id,
        user_id,
        source_expires_at
    )
    .fetch_optional(&mut *transaction)
    .await?;
    if active.is_none() {
        return Ok(None);
    }
    transaction.commit().await?;
    Ok(active)
}

pub(crate) async fn mint_gateway_inference_token(
    state: &AppState,
    who: &Principal,
) -> Result<String, ApiError> {
    if !who.bearer_token {
        return Err(ApiError::unauthorized());
    }
    let oauth_session_id = who
        .oauth_session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(ApiError::unauthorized)?;
    let source_expires_at =
        renew_gateway_auth_session(&state.pool, oauth_session_id, &who.subject, who.user_id)
            .await?
            .ok_or_else(ApiError::unauthorized)?;
    let key_ring = state
        .gateway_jwt
        .as_ref()
        .ok_or_else(|| ApiError::internal("gateway inference JWT signing is not configured"))?;
    let now = OffsetDateTime::now_utc();
    let expires_at = std::cmp::min(now + Duration::days(30), source_expires_at);
    let claims = GatewayInferenceClaims {
        iss: key_ring.issuer.clone(),
        aud: key_ring.audience.clone(),
        sub: who.user_id.to_string(),
        iat: now.unix_timestamp(),
        exp: expires_at.unix_timestamp(),
        jti: Uuid::new_v4().to_string(),
        blue_oauth_session_id: oauth_session_id.to_owned(),
        scope: "gateway:infer".into(),
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(key_ring.active_kid.clone());
    encode(&header, &claims, &key_ring.signing_key)
        .map_err(|error| ApiError::internal(format!("signing gateway inference JWT: {error}")))
}

pub(crate) async fn gateway_jwks(
    State(state): State<Arc<AppState>>,
) -> Result<Json<JwkSet>, ApiError> {
    state
        .gateway_jwt
        .as_ref()
        .map(|ring| Json(ring.public_jwks.clone()))
        .ok_or_else(|| ApiError::not_found("gateway mode is not enabled"))
}
