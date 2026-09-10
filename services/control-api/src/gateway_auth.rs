//! Session-bound inference JWT issuance and verification-key publication.

use axum::extract::State;
use axum::Json;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{encode, Algorithm, Header};
use serde::Serialize;
use sqlx::PgPool;
use std::sync::Arc;
use time::OffsetDateTime;
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

/// Bind (or rebind) a gateway session to the Better Auth session that
/// authorized it, returning the instant the binding expires.
///
/// `issued_at` is the signature-verified `iat` of the caller's bearer token.
/// A binding revoked by `blue logout` is reactivated only by a token issued
/// *after* the revocation, which keeps every pre-logout JWT dead.
///
/// Relaxing the previously-sticky `revoked_at` is safe only because the first
/// statement below bails when the `auth."session"` row is gone, and
/// `revoke_credentials` deletes those rows on suspend, delete, and admin
/// session-revoke. That leaves CLI logout as the only path into a revoked row
/// that can still be reactivated. **Reordering these two statements, or
/// dropping the source-session predicate, reopens revoked sessions.**
pub(crate) async fn renew_gateway_auth_session(
    pool: &PgPool,
    oauth_session_id: &str,
    subject: &str,
    user_id: Uuid,
    issued_at: OffsetDateTime,
) -> Result<Option<OffsetDateTime>, ApiError> {
    // Read, never extend. Sliding the browser session from a CLI poll let the
    // gateway binding outlive the session the user actually maintains, and put
    // the session lifetime in two places at once; apps/dashboard/lib/auth.ts
    // owns it now.
    let source_expires_at: Option<OffsetDateTime> = sqlx::query_scalar!(
        "select \"expiresAt\" from auth.\"session\" where id=$1 and \"userId\"=$2 and \"expiresAt\">now()",
        oauth_session_id,
        subject
    )
    .fetch_optional(pool)
    .await?;
    let Some(source_expires_at) = source_expires_at else {
        return Ok(None);
    };
    // greatest() ignores NULLs, so reactivated_at only ever moves forward.
    let renewed = sqlx::query!(
        "insert into public.gateway_auth_sessions(oauth_session_id,user_id,source_expires_at) values($1,$2,$3) on conflict(oauth_session_id) do update set source_expires_at=excluded.source_expires_at,revoked_at=null,reactivated_at=greatest(gateway_auth_sessions.reactivated_at,gateway_auth_sessions.revoked_at),updated_at=now() where gateway_auth_sessions.user_id=excluded.user_id and (gateway_auth_sessions.revoked_at is null or gateway_auth_sessions.revoked_at < $4) returning source_expires_at, reactivated_at",
        oauth_session_id,
        user_id,
        source_expires_at,
        issued_at
    )
    .fetch_optional(pool)
    .await?;
    let Some(renewed) = renewed else {
        return Ok(None);
    };
    if let Some(reactivated_at) = renewed.reactivated_at {
        // A clock-skewed fleet shows up here as a reactivation the operator
        // did not expect. `iat` is signature-verified, so the skew exposure is
        // bounded and identical to the existing tokens_valid_after rule.
        tracing::info!(
            %user_id,
            oauth_session_id,
            revoked_at = %reactivated_at,
            issued_at = %issued_at,
            "reactivated a revoked gateway auth session"
        );
    }
    Ok(Some(renewed.source_expires_at))
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
    // Mirrors the fail-closed tokens_valid_after rule: a bearer token whose
    // issue time we cannot read cannot be compared against a revocation.
    let issued_at = who.issued_at.ok_or_else(ApiError::unauthorized)?;
    let source_expires_at = renew_gateway_auth_session(
        &state.pool,
        oauth_session_id,
        &who.subject,
        who.user_id,
        issued_at,
    )
    .await?
    .ok_or_else(ApiError::unauthorized)?;
    let key_ring = state
        .gateway_jwt
        .as_ref()
        .ok_or_else(|| ApiError::internal("gateway inference JWT signing is not configured"))?;
    let now = OffsetDateTime::now_utc();
    let expires_at = inference_token_expiry(now, key_ring.token_ttl, source_expires_at);
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

/// The JWT must never outlive the browser session that authorized it, so the
/// configured TTL is only a ceiling.
fn inference_token_expiry(
    now: OffsetDateTime,
    ttl: time::Duration,
    source_expires_at: OffsetDateTime,
) -> OffsetDateTime {
    std::cmp::min(now + ttl, source_expires_at)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inference_token_expiry_clamps_to_the_nearer_bound() {
        let now = OffsetDateTime::now_utc();
        let ttl = time::Duration::hours(12);

        // A session with hours left: the TTL is the binding constraint.
        assert_eq!(
            inference_token_expiry(now, ttl, now + time::Duration::days(2)),
            now + ttl
        );

        // A session about to end: the session is.
        let ending = now + time::Duration::minutes(3);
        assert_eq!(inference_token_expiry(now, ttl, ending), ending);
    }
}
