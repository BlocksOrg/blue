use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::{ApiError, AppConfig};

pub(crate) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub(crate) struct Pools {
    pub(crate) control: PgPool,
    pub(crate) gateway_logs: PgPool,
}

async fn control_pool(database_url: &str, max_connections: u32) -> Result<PgPool, ApiError> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(database_url)
        .await
        .map_err(|error| ApiError::internal(format!("connecting to postgres_control: {error}")))
}

/// Apply the embedded migration set. This is intentionally only called by the
/// release migration command; serving replicas are verify-only.
pub(crate) async fn migrate(database_url: &str) -> Result<(), ApiError> {
    let pool = control_pool(database_url, 1).await?;
    migrate_pool(&pool).await
}

async fn migrate_pool(pool: &PgPool) -> Result<(), ApiError> {
    // Historical migration 0002 selected the `auth` schema before pg_trgm was
    // first installed. Put this relocatable extension in its final schema up
    // front so clean installs can create the trigram indexes in migration 0029;
    // migration 0030 remains the recorded compatibility repair for upgrades.
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA public") // sqlx-guard: allow-raw: DDL not preparable by checked macros
        .execute(pool)
        .await
        .map_err(|error| ApiError::internal(format!("preparing pg_trgm: {error}")))?;
    sqlx::query( // sqlx-guard: allow-raw: anonymous DO block DDL not preparable by checked macros
        "DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='pg_trgm' AND n.nspname<>'public') THEN ALTER EXTENSION pg_trgm SET SCHEMA public; END IF; END $$",
    )
    .execute(pool)
    .await
    .map_err(|error| ApiError::internal(format!("normalizing pg_trgm schema: {error}")))?;

    // SQLx rejects a database containing migrations newer than this binary.
    // A rollback across expand-only migrations is nevertheless safe, so prove
    // compatibility and avoid asking SQLx to migrate backwards in that case.
    if applied_migration_max(pool).await? > embedded_migration_max() {
        return verify_schema(pool).await;
    }
    MIGRATOR
        .run(pool)
        .await
        .map_err(|error| ApiError::internal(format!("running control migrations: {error}")))?;
    verify_schema(pool).await
}

fn embedded_migration_max() -> i64 {
    MIGRATOR
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}

async fn applied_migration_max(pool: &PgPool) -> Result<i64, ApiError> {
    let exists: bool = sqlx::query_scalar( // sqlx-guard: allow-raw: inspects sqlx-internal _sqlx_migrations, absent from the migration schema
        "SELECT to_regclass('public._sqlx_migrations') IS NOT NULL OR to_regclass('_sqlx_migrations') IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| ApiError::internal(format!("locating migration history: {error}")))?;
    if !exists {
        return Ok(0);
    }
    sqlx::query_scalar("SELECT COALESCE(max(version), 0) FROM _sqlx_migrations") // sqlx-guard: allow-raw: reads sqlx-internal _sqlx_migrations, absent from the migration schema
        .fetch_one(pool)
        .await
        .map_err(|error| ApiError::internal(format!("reading migration history: {error}")))
}

/// Prove that every migration embedded in this release exists with its exact
/// checksum. A newer suffix is accepted only while the schema compatibility
/// floor still includes this binary, which permits rollback across expand-only
/// releases without allowing rollback across contract migrations.
pub(crate) async fn verify_schema(pool: &PgPool) -> Result<(), ApiError> {
    let rows =
        sqlx::query("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version") // sqlx-guard: allow-raw: reads sqlx-internal _sqlx_migrations, absent from the migration schema
            .fetch_all(pool)
            .await
            .map_err(|error| {
                ApiError::internal(format!(
                    "verifying control schema (run `control-api migrate` first): {error}"
                ))
            })?;

    let expected = MIGRATOR
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .collect::<Vec<_>>();
    if rows.len() < expected.len() {
        return Err(ApiError::internal(format!(
            "control schema is incomplete: expected at least {} applied migrations, found {}",
            expected.len(),
            rows.len()
        )));
    }
    let expected_len = expected.len();
    for (row, migration) in rows.iter().zip(expected) {
        let version: i64 = row
            .try_get("version")
            .map_err(|error| ApiError::internal(format!("reading migration version: {error}")))?;
        let checksum: Vec<u8> = row
            .try_get("checksum")
            .map_err(|error| ApiError::internal(format!("reading migration checksum: {error}")))?;
        let success: bool = row
            .try_get("success")
            .map_err(|error| ApiError::internal(format!("reading migration status: {error}")))?;
        if version != migration.version || checksum.as_slice() != migration.checksum.as_ref() {
            return Err(ApiError::internal(format!(
                "control schema migration {version} does not match this release"
            )));
        }
        if !success {
            return Err(ApiError::internal(format!(
                "control schema migration {version} is incomplete"
            )));
        }
    }

    for row in rows.iter().skip(expected_len) {
        let success: bool = row.try_get("success").map_err(|error| {
            ApiError::internal(format!("reading newer migration status: {error}"))
        })?;
        if !success {
            let version: i64 = row.try_get("version").unwrap_or_default();
            return Err(ApiError::internal(format!(
                "control schema migration {version} is incomplete"
            )));
        }
    }

    // sqlx-guard: allow-raw: bootstrap compatibility read runs before migrate ordering is guaranteed
    let minimum_migration_version: i64 = sqlx::query_scalar(
        "SELECT minimum_migration_version FROM public.schema_compatibility WHERE singleton = true",
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| ApiError::internal(format!("reading schema compatibility floor: {error}")))?
    .ok_or_else(|| ApiError::internal("schema compatibility floor is missing"))?;
    let embedded_max = embedded_migration_max();
    if minimum_migration_version > embedded_max {
        return Err(ApiError::internal(format!(
            "control schema requires migration generation {minimum_migration_version}, but this binary supports {embedded_max}"
        )));
    }
    Ok(())
}

pub(crate) async fn connect(config: &AppConfig) -> Result<Pools, ApiError> {
    let control = control_pool(&config.database_url, config.database_max_connections).await?;
    verify_schema(&control).await?;
    let gateway_logs = PgPoolOptions::new()
        .max_connections(config.gateway_log_database_max_connections)
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect(&config.database_url)
        .await
        .map_err(|error| ApiError::internal(format!("connecting gateway log pool: {error}")))?;
    Ok(Pools {
        control,
        gateway_logs,
    })
}

#[cfg(all(test, feature = "postgres-tests"))]
mod tests {
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use uuid::Uuid;

    use crate::secrets::{Envelope, SecretProtector};
    use crate::{
        managed_gateway_row, migrate_legacy_gateway_credentials, resolved_gateway_credential_row,
    };

    #[sqlx::test(migrations = false)]
    async fn serving_schema_verification_accepts_the_exact_embedded_set(pool: PgPool) {
        super::migrate_pool(&pool).await.unwrap();
        super::verify_schema(&pool).await.unwrap();

        sqlx::query("UPDATE _sqlx_migrations SET checksum = decode('00', 'hex') WHERE version = (SELECT max(version) FROM _sqlx_migrations)") // sqlx-guard: allow-raw: test fixture mutating sqlx-internal _sqlx_migrations
            .execute(&pool)
            .await
            .unwrap();
        let error = super::verify_schema(&pool).await.unwrap_err();
        assert!(error.to_string().contains("does not match this release"));
    }

    #[sqlx::test(migrations = false)]
    async fn compatible_newer_expand_migration_allows_rollback(pool: PgPool) {
        super::migrate_pool(&pool).await.unwrap();
        sqlx::query("INSERT INTO _sqlx_migrations(version, description, installed_on, success, checksum, execution_time) VALUES ($1, 'future expand', now(), true, decode('01', 'hex'), 0)") // sqlx-guard: allow-raw: test fixture mutating sqlx-internal _sqlx_migrations
            .bind(super::embedded_migration_max() + 1)
            .execute(&pool)
            .await
            .unwrap();
        super::verify_schema(&pool).await.unwrap();
        super::migrate_pool(&pool).await.unwrap();
    }

    #[sqlx::test(migrations = false)]
    async fn contract_floor_rejects_an_incompatible_rollback(pool: PgPool) {
        super::migrate_pool(&pool).await.unwrap();
        let future = super::embedded_migration_max() + 1;
        sqlx::query("INSERT INTO _sqlx_migrations(version, description, installed_on, success, checksum, execution_time) VALUES ($1, 'future contract', now(), true, decode('01', 'hex'), 0)") // sqlx-guard: allow-raw: test fixture mutating sqlx-internal _sqlx_migrations
            .bind(future)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE public.schema_compatibility SET minimum_migration_version = $1, updated_at = now() WHERE singleton = true") // sqlx-guard: allow-raw: test fixture writing schema_compatibility floor directly
            .bind(future)
            .execute(&pool)
            .await
            .unwrap();
        let error = super::verify_schema(&pool).await.unwrap_err();
        assert!(error.to_string().contains("requires migration generation"));
        assert!(super::migrate_pool(&pool).await.is_err());
    }

    #[sqlx::test(migrations = false)]
    async fn migrations_enforce_user_role_and_tenant_email_constraints(pool: PgPool) {
        super::migrate_pool(&pool).await.unwrap();
        let org_id = Uuid::new_v4();
        sqlx::query!(
            "insert into organizations(id,slug,name) values($1,$2,$3)",
            org_id,
            "checked-queries",
            "Checked Queries"
        )
        .execute(&pool)
        .await
        .unwrap();
        let first = Uuid::new_v4();
        sqlx::query!(
            "insert into users(id,organization_id,subject,email,role) values($1,$2,$3,$4,'admin')",
            first,
            org_id,
            "subject-1",
            "admin@example.com"
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(sqlx::query!(
            "insert into users(id,organization_id,subject,email,role) values($1,$2,$3,$4,'owner')",
            Uuid::new_v4(),
            org_id,
            "subject-2",
            "owner@example.com"
        )
        .execute(&pool)
        .await
        .is_err());
        assert!(sqlx::query!(
            "insert into users(id,organization_id,subject,email,role) values($1,$2,$3,$4,'member')",
            Uuid::new_v4(),
            org_id,
            "subject-3",
            "admin@example.com"
        )
        .execute(&pool)
        .await
        .is_err());
    }

    #[sqlx::test(migrations = false)]
    async fn failed_cross_schema_transaction_leaves_no_partial_user(pool: PgPool) {
        super::migrate_pool(&pool).await.unwrap();
        let org_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        sqlx::query!(
            "insert into organizations(id,slug,name) values($1,$2,$3)",
            org_id,
            "rollback",
            "Rollback"
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut transaction = pool.begin().await.unwrap();
        sqlx::query!(
            "insert into users(id,organization_id,subject,email,role) values($1,$2,$3,$4,'member')",
            user_id,
            org_id,
            "shared-subject",
            "member@example.com"
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        let auth_insert = sqlx::query!(
            "insert into auth.\"user\"(id,name,email,\"emailVerified\",\"createdAt\",\"updatedAt\") values($1,$2,$3,true,now(),now()),($1,$2,$3,true,now(),now())",
            "shared-subject",
            "Member",
            "member@example.com"
        )
        .execute(&mut *transaction)
        .await;
        assert!(auth_insert.is_err());
        transaction.rollback().await.unwrap();
        let exists = sqlx::query_scalar!(
            "select exists(select 1 from users where id=$1) as \"exists!\"",
            user_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!exists);
    }

    #[sqlx::test(migrations = false)]
    async fn gateway_credential_queries_preserve_identity_and_nullable_fields(pool: PgPool) {
        super::migrate_pool(&pool).await.unwrap();
        let org_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let credential_version = Uuid::new_v4();
        let expires_at = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let oauth_session_id = "gateway-oauth-session";
        sqlx::query!(
            "insert into organizations(id,slug,name) values($1,$2,$3)",
            org_id,
            "gateway-row",
            "Gateway Row"
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into users(id,organization_id,subject,email,role) values($1,$2,$3,$4,'member')",
            user_id,
            org_id,
            "gateway-subject",
            "gateway@example.com"
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into auth.\"user\"(id,name,email,\"emailVerified\",\"updatedAt\") values($1,$2,$3,true,now())",
            "gateway-subject",
            "Gateway User",
            "gateway@example.com"
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into auth.\"session\"(id,\"expiresAt\",token,\"updatedAt\",\"userId\") values($1,$2,$3,now(),$4)",
            oauth_session_id,
            expires_at,
            "gateway-session-token",
            "gateway-subject"
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into gateway_key_selections(user_id,gateway_email,credential_ciphertext,credential_nonce,credential_wrapped_key,encryption_key_id,credential_version,credential_expires_at,provisioner_metadata) values($1,$2,$3,$4,$5,$6,$7,$8,$9)",
            user_id,
            "gateway@example.com",
            &[1_u8, 2, 3][..],
            &[4_u8, 5][..],
            &[6_u8, 7][..],
            "test-key",
            credential_version,
            expires_at,
            serde_json::json!({"scope": "e2e"})
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into gateway_auth_sessions(oauth_session_id,user_id,source_expires_at) values($1,$2,$3)",
            oauth_session_id,
            user_id,
            expires_at
        )
        .execute(&pool)
        .await
        .unwrap();

        let managed = managed_gateway_row(&pool, user_id).await.unwrap().unwrap();
        assert_eq!(managed.source_key_hash, None);
        assert_eq!(managed.source_key_alias, None);
        assert_eq!(managed.credential_version, credential_version);
        assert_eq!(
            managed.credential_ciphertext.as_deref(),
            Some(&[1, 2, 3][..])
        );

        let resolved = resolved_gateway_credential_row(&pool, user_id, oauth_session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.user_id, user_id);
        assert_eq!(resolved.organization_id, org_id);
        let credential = resolved.credential();
        assert_eq!(credential.source_key_hash, None);
        assert_eq!(credential.credential_version, credential_version);
        assert_eq!(credential.provisioner_metadata["scope"], "e2e");

        sqlx::query!("delete from auth.\"session\" where id=$1", oauth_session_id)
            .execute(&pool)
            .await
            .unwrap();
        let revoked: bool = sqlx::query_scalar!(
            "select revoked_at is not null as \"revoked!\" from gateway_auth_sessions where oauth_session_id=$1",
            oauth_session_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(revoked);
        assert!(
            resolved_gateway_credential_row(&pool, user_id, oauth_session_id)
                .await
                .unwrap()
                .is_none()
        );

        sqlx::query!(
            "update users set active=false,status='suspended' where id=$1",
            user_id
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            resolved_gateway_credential_row(&pool, user_id, oauth_session_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test(migrations = false)]
    async fn gateway_session_renewal_is_rolling_but_cannot_revive_revocation(pool: PgPool) {
        super::migrate_pool(&pool).await.unwrap();
        let org_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let subject = "rolling-gateway-subject";
        let session_id = "rolling-gateway-session";
        sqlx::query!(
            "insert into organizations(id,slug,name) values($1,$2,$3)",
            org_id,
            "rolling-gateway",
            "Rolling Gateway"
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into users(id,organization_id,subject,email,role) values($1,$2,$3,$4,'member')",
            user_id,
            org_id,
            subject,
            "rolling@example.com"
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into auth.\"user\"(id,name,email,\"emailVerified\",\"updatedAt\") values($1,$2,$3,true,now())",
            subject,
            "Rolling User",
            "rolling@example.com"
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into auth.\"session\"(id,\"expiresAt\",token,\"updatedAt\",\"userId\") values($1,now()+interval '5 minutes',$2,now(),$3)",
            session_id,
            "rolling-session-token",
            subject
        )
        .execute(&pool)
        .await
        .unwrap();

        let renewed =
            crate::gateway_auth::renew_gateway_auth_session(&pool, session_id, subject, user_id)
                .await
                .unwrap()
                .unwrap();
        let remaining = renewed - OffsetDateTime::now_utc();
        assert!(remaining > time::Duration::hours(11));
        assert!(remaining <= time::Duration::hours(12));

        sqlx::query!(
            "update gateway_auth_sessions set revoked_at=now() where oauth_session_id=$1",
            session_id
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(crate::gateway_auth::renew_gateway_auth_session(
            &pool, session_id, subject, user_id
        )
        .await
        .unwrap()
        .is_none());
    }

    #[sqlx::test(migrations = false)]
    async fn legacy_plaintext_gateway_credential_is_encrypted_in_place(pool: PgPool) {
        super::migrate_pool(&pool).await.unwrap();
        let org_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        sqlx::query!(
            "insert into organizations(id,slug,name) values($1,$2,$3)",
            org_id,
            "legacy-gateway",
            "Legacy Gateway"
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into users(id,organization_id,subject,email,role) values($1,$2,$3,$4,'member')",
            user_id,
            org_id,
            "legacy-gateway-subject",
            "legacy-gateway@example.com"
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "insert into gateway_key_selections(user_id,gateway_email,proxy_virtual_key,credential_state) values($1,$2,$3,'ready')",
            user_id,
            "legacy-gateway@example.com",
            "legacy-upstream-secret"
        )
        .execute(&pool)
        .await
        .unwrap();
        let protector =
            SecretProtector::environment("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                .await
                .unwrap();

        migrate_legacy_gateway_credentials(&pool, &protector)
            .await
            .unwrap();

        let row = sqlx::query!(
            "select proxy_virtual_key,credential_ciphertext,credential_nonce,credential_wrapped_key,encryption_key_id from gateway_key_selections where user_id=$1",
            user_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(row.proxy_virtual_key.is_none());
        let plaintext = protector
            .decrypt(&Envelope {
                ciphertext: row.credential_ciphertext.unwrap(),
                nonce: row.credential_nonce.unwrap(),
                wrapped_key: row.credential_wrapped_key.unwrap(),
                key_id: row.encryption_key_id.unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(plaintext.as_slice(), b"legacy-upstream-secret");
    }
}
