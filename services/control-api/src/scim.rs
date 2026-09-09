use super::*;
use axum::body::Body;
use axum::http::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::{self, Next};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};

const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/ServiceProviderConfig", get(service_provider_config))
        .route("/ResourceTypes", get(resource_types))
        .route("/Schemas", get(schemas))
        .route("/Users", get(list_users).post(create_user))
        .route(
            "/Users/:id",
            get(get_user)
                .put(replace_user)
                .patch(patch_user)
                .delete(delete_user),
        )
        .route("/Groups", get(list_groups).post(create_group))
        .route(
            "/Groups/:id",
            get(get_group)
                .put(replace_group)
                .patch(patch_group)
                .delete(delete_group),
        )
        .route_layer(middleware::from_fn_with_state(state, scim_auth_guard))
}

#[derive(Clone, Copy)]
struct ScimPrincipal(Uuid);

async fn scim_auth_guard(
    State(state): State<Arc<AppState>>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, ScimError> {
    let organization_id = authorize(&state, request.headers()).await?;
    request
        .extensions_mut()
        .insert(ScimPrincipal(organization_id));
    Ok(next.run(request).await)
}

struct ScimJson(StatusCode, serde_json::Value);

impl IntoResponse for ScimJson {
    fn into_response(self) -> Response {
        let mut response = if self.0 == StatusCode::NO_CONTENT {
            self.0.into_response()
        } else {
            (self.0, Json(self.1)).into_response()
        };
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/scim+json"),
        );
        response
    }
}

#[derive(Debug)]
struct ScimError {
    status: StatusCode,
    scim_type: Option<&'static str>,
    detail: String,
}

impl ScimError {
    fn new(status: StatusCode, scim_type: Option<&'static str>, detail: impl Into<String>) -> Self {
        Self {
            status,
            scim_type,
            detail: detail.into(),
        }
    }
    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, None, "invalid SCIM bearer token")
    }
    fn invalid(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("invalidValue"), detail)
    }
    fn invalid_path(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("invalidPath"), detail)
    }
    fn not_found(kind: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, None, format!("{kind} not found"))
    }
    fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, Some("uniqueness"), detail)
    }
}

impl From<sqlx::Error> for ScimError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .and_then(|value| value.code())
            .as_deref()
            == Some("23505")
        {
            return Self::conflict("a resource with that identifier already exists");
        }
        tracing::error!(%error, "SCIM database error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
            "database operation failed",
        )
    }
}

impl From<ApiError> for ScimError {
    fn from(error: ApiError) -> Self {
        Self::new(error.status, None, error.message)
    }
}

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let unauthorized = self.status == StatusCode::UNAUTHORIZED;
        let mut body = json!({"schemas":[ERROR_SCHEMA],"status":self.status.as_u16().to_string(),"detail":self.detail});
        if let Some(scim_type) = self.scim_type {
            body["scimType"] = json!(scim_type);
        }
        let mut response = ScimJson(self.status, body).into_response();
        if unauthorized {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"scim\""),
            );
        }
        response
    }
}

fn token_matches(expected: &str, actual: &str) -> bool {
    let expected = Sha256::digest(expected.as_bytes());
    let actual = Sha256::digest(actual.as_bytes());
    expected
        .iter()
        .zip(actual.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

async fn authorize(state: &AppState, headers: &HeaderMap) -> Result<Uuid, ScimError> {
    let expected = state
        .config
        .scim_bearer_token
        .as_deref()
        .ok_or_else(ScimError::unauthorized)?;
    let actual = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(ScimError::unauthorized)?;
    if !token_matches(expected, actual) {
        return Err(ScimError::unauthorized());
    }
    sqlx::query_scalar!(
        "select id from organizations where slug=$1",
        &state.config.bootstrap_org
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| {
        ScimError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
            "SCIM organization is unavailable",
        )
    })
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    filter: Option<String>,
    start_index: Option<i64>,
    count: Option<i64>,
}

fn page(query: &ListQuery) -> Result<(i64, i64), ScimError> {
    let start = query.start_index.unwrap_or(1);
    let count = query.count.unwrap_or(100).clamp(0, 200);
    if start < 1 {
        return Err(ScimError::invalid("startIndex must be at least 1"));
    }
    Ok((start, count))
}

fn eq_filter(
    filter: Option<&str>,
    allowed: &[&str],
) -> Result<Option<(String, String)>, ScimError> {
    let Some(filter) = filter.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let mut parts = filter
        .splitn(3, char::is_whitespace)
        .filter(|part| !part.is_empty());
    let attribute = parts.next().unwrap_or_default();
    let operator = parts.next().unwrap_or_default();
    let raw = parts.next().unwrap_or_default().trim();
    if !allowed.contains(&attribute)
        || !operator.eq_ignore_ascii_case("eq")
        || raw.len() < 2
        || !raw.starts_with('"')
        || !raw.ends_with('"')
    {
        return Err(ScimError::new(
            StatusCode::BAD_REQUEST,
            Some("invalidFilter"),
            "only supported equality filters may be used",
        ));
    }
    Ok(Some((
        attribute.into(),
        raw[1..raw.len() - 1].replace("\\\"", "\""),
    )))
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ScimName {
    given_name: Option<String>,
    family_name: Option<String>,
}

#[derive(Deserialize, Clone)]
struct ScimEmail {
    value: String,
    #[serde(default)]
    primary: bool,
    #[serde(rename = "type")]
    _kind: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct UserInput {
    #[serde(default)]
    schemas: Vec<String>,
    external_id: Option<String>,
    user_name: String,
    name: Option<ScimName>,
    #[serde(default)]
    emails: Vec<ScimEmail>,
    active: Option<bool>,
}

fn input_email(input: &UserInput) -> Result<String, ScimError> {
    let value = input
        .emails
        .iter()
        .find(|email| email.primary)
        .or_else(|| input.emails.first())
        .map(|email| email.value.as_str())
        .unwrap_or(&input.user_name);
    normalize_email(value).map_err(|_| ScimError::invalid("a valid primary email is required"))
}

#[derive(FromRow)]
struct ScimUserRow {
    id: Uuid,
    subject: String,
    email: String,
    active: bool,
    external_id: Option<String>,
    user_name: String,
    given_name: Option<String>,
    family_name: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

async fn user_row(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<ScimUserRow, ScimError> {
    sqlx::query_as!(
        ScimUserRow,
        "select u.id,u.subject,u.email,u.active,s.external_id,s.user_name,s.given_name,s.family_name,s.created_at,s.updated_at \
         from scim_user_resources s join users u on u.id=s.user_id where s.organization_id=$1 and u.id=$2 and u.status<>'removed'",
        org_id,
        id
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ScimError::not_found("user"))
}

async fn user_json(pool: &PgPool, row: ScimUserRow) -> Result<serde_json::Value, ScimError> {
    let groups = sqlx::query!(
        "select g.id,g.display_name from scim_groups g join scim_group_members m on m.group_id=g.id where m.user_id=$1 order by g.display_name",
        row.id
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|group| json!({"value":group.id,"display":group.display_name,"type":"direct"}))
    .collect::<Vec<_>>();
    Ok(json!({
        "schemas":[USER_SCHEMA],"id":row.id,"externalId":row.external_id,"userName":row.user_name,
        "name":{"givenName":row.given_name,"familyName":row.family_name},
        "displayName":format!("{} {}",row.given_name.as_deref().unwrap_or(""),row.family_name.as_deref().unwrap_or("")).trim(),
        "emails":[{"value":row.email,"type":"work","primary":true}],"active":row.active,"groups":groups,
        "meta":{"resourceType":"User","created":now_text(row.created_at),"lastModified":now_text(row.updated_at),"location":format!("/scim/v2/Users/{}",row.id)}
    }))
}

async fn list_users(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Query(query): Query<ListQuery>,
) -> Result<ScimJson, ScimError> {
    let (start, count) = page(&query)?;
    let filter = eq_filter(query.filter.as_deref(), &["userName", "externalId"])?;
    let (attribute, value) = filter.map(|(a, v)| (Some(a), Some(v))).unwrap_or_default();
    let total: i64 = sqlx::query_scalar!("select count(*) as \"count!\" from scim_user_resources s join users u on u.id=s.user_id where s.organization_id=$1 and u.status<>'removed' \
         and ($2::text is null or ($2='userName' and lower(s.user_name)=lower($3)) or ($2='externalId' and s.external_id=$3))",
        org_id,
        attribute.as_deref(),
        value.as_deref()).fetch_one(&state.pool).await?;
    let rows = sqlx::query_as!(ScimUserRow,
        "select u.id,u.subject,u.email,u.active,s.external_id,s.user_name,s.given_name,s.family_name,s.created_at,s.updated_at \
         from scim_user_resources s join users u on u.id=s.user_id where s.organization_id=$1 and u.status<>'removed' \
         and ($2::text is null or ($2='userName' and lower(s.user_name)=lower($3)) or ($2='externalId' and s.external_id=$3)) \
         order by lower(s.user_name) offset $4 limit $5",
        org_id,
        attribute.as_deref(),
        value.as_deref(),
        start - 1,
        count).fetch_all(&state.pool).await?;
    let mut resources = Vec::with_capacity(rows.len());
    for row in rows {
        resources.push(user_json(&state.pool, row).await?);
    }
    Ok(ScimJson(
        StatusCode::OK,
        json!({"schemas":[LIST_SCHEMA],"totalResults":total,"startIndex":start,"itemsPerPage":resources.len(),"Resources":resources}),
    ))
}

async fn get_user(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Path(id): Path<Uuid>,
) -> Result<ScimJson, ScimError> {
    Ok(ScimJson(
        StatusCode::OK,
        user_json(&state.pool, user_row(&state.pool, org_id, id).await?).await?,
    ))
}

async fn effective_role(
    transaction: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    mappings: &BTreeMap<String, String>,
) -> Result<String, ScimError> {
    let names: Vec<String> = sqlx::query_scalar!("select g.display_name from scim_groups g join scim_group_members m on m.group_id=g.id where m.user_id=$1",
        user_id).fetch_all(&mut **transaction).await?;
    if names
        .iter()
        .any(|name| mappings.get(name).is_some_and(|role| role == "admin"))
    {
        Ok("admin".into())
    } else {
        Ok("member".into())
    }
}

async fn sync_role(
    transaction: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    mappings: &BTreeMap<String, String>,
) -> Result<(), ScimError> {
    let role = effective_role(transaction, user_id, mappings).await?;
    let target = sqlx::query!(
        "select subject,role from users where id=$1 and provisioning_source='scim'",
        user_id
    )
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(target) = target else {
        return Ok(());
    };
    let subject = target.subject;
    let old_role = target.role;
    if role == old_role {
        return Ok(());
    }
    revoke_credentials(transaction, user_id, &subject).await?;
    sqlx::query!(
        "update users set role=$1,tokens_valid_after=now(),updated_at=now() where id=$2",
        &role,
        user_id
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query!(
        "update auth.\"user\" set \"governanceRole\"=$1,\"updatedAt\"=now() where id=$2",
        &role,
        &subject
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query!(
        "update auth.\"member\" set role=$1 where \"userId\"=$2",
        &role,
        &subject
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn create_user(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Json(input): Json<UserInput>,
) -> Result<ScimJson, ScimError> {
    if !input.schemas.is_empty() && !input.schemas.iter().any(|schema| schema == USER_SCHEMA) {
        return Err(ScimError::invalid("User schema is required"));
    }
    let email = input_email(&input)?;
    let user_name = input.user_name.trim();
    if user_name.is_empty() {
        return Err(ScimError::invalid("userName is required"));
    }
    let active = input.active.unwrap_or(true);
    let auth_org = auth_org_id(&state.pool, org_id).await?;
    let mut transaction = state.pool.begin().await?;
    let existing = sqlx::query!(
        "select id,status,subject,protected,provisioning_source from users where organization_id=$1 and lower(email)=lower($2)",
        org_id,
        &email
    )
    .fetch_optional(&mut *transaction)
    .await?;
    let (id, subject, create_auth) = if let Some(existing) = existing {
        let id = existing.id;
        let status = existing.status;
        let subject = existing.subject;
        let protected = existing.protected;
        let source = existing.provisioning_source;
        let has_scim: bool = sqlx::query_scalar!(
            "select exists(select 1 from scim_user_resources where user_id=$1) as \"exists!\"",
            id
        )
        .fetch_one(&mut *transaction)
        .await?;
        if has_scim || source == "scim" {
            return Err(ScimError::conflict("email is already provisioned"));
        }
        if protected {
            return Err(ScimError::conflict(
                "the protected bootstrap administrator cannot be SCIM managed",
            ));
        }
        if status == "removed" {
            (id, Uuid::new_v4().to_string(), true)
        } else {
            (id, subject, false)
        }
    } else {
        (Uuid::new_v4(), Uuid::new_v4().to_string(), true)
    };
    let name = input.name.clone().unwrap_or(ScimName {
        given_name: None,
        family_name: None,
    });
    let display_name = format!(
        "{} {}",
        name.given_name.as_deref().unwrap_or(""),
        name.family_name.as_deref().unwrap_or("")
    )
    .trim()
    .to_owned();
    if create_auth {
        sqlx::query!("insert into auth.\"user\" (id,name,email,\"emailVerified\",\"createdAt\",\"updatedAt\",\"organizationId\",\"governanceRole\",banned,\"banReason\") values ($1,$2,$3,true,now(),now(),$4,'member',$5,$6)",
        &subject,
        if display_name.is_empty() { &email } else { &display_name },
        &email,
        org_id.to_string(),
        !active,
        if active { None } else { Some("Deactivated by identity provider") }).execute(&mut *transaction).await?;
    } else {
        revoke_credentials(&mut transaction, id, &subject).await?;
        sqlx::query!("delete from auth.\"account\" where \"userId\"=$1", &subject)
            .execute(&mut *transaction)
            .await?;
        sqlx::query!("update auth.\"user\" set name=$1,email=$2,\"emailVerified\"=true,\"organizationId\"=$3,\"governanceRole\"='member',banned=$4,\"banReason\"=$5,\"updatedAt\"=now() where id=$6",
        if display_name.is_empty() { &email } else { &display_name },
        &email,
        org_id.to_string(),
        !active,
        if active { None } else { Some("Deactivated by identity provider") },
        &subject).execute(&mut *transaction).await?;
    }
    sqlx::query!("insert into auth.\"member\" (id,\"organizationId\",\"userId\",role,\"createdAt\") select $1,$2,$3,'member',now() where not exists (select 1 from auth.\"member\" where \"organizationId\"=$2 and \"userId\"=$3)",
        Uuid::new_v4().to_string(),
        &auth_org,
        &subject).execute(&mut *transaction).await?;
    sqlx::query!(
        "update auth.\"member\" set role='member' where \"organizationId\"=$1 and \"userId\"=$2",
        &auth_org,
        &subject
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query!("insert into users (id,organization_id,subject,email,role,status,active,provisioning_source,tokens_valid_after) values ($1,$2,$3,$4,'member',$5,$6,'scim',case when $6 then null else now() end) on conflict (id) do update set subject=excluded.subject,email=excluded.email,role='member',status=excluded.status,active=excluded.active,provisioning_source='scim',tokens_valid_after=excluded.tokens_valid_after,updated_at=now()",
        id,
        org_id,
        &subject,
        &email,
        if active { "active" } else { "suspended" },
        active).execute(&mut *transaction).await?;
    if !create_auth {
        sqlx::query!("update users set tokens_valid_after=now() where id=$1", id)
            .execute(&mut *transaction)
            .await?;
    }
    sqlx::query!("insert into scim_user_resources (user_id,organization_id,external_id,user_name,given_name,family_name) values ($1,$2,$3,$4,$5,$6)",
        id,
        org_id,
        input.external_id.as_deref(),
        user_name,
        name.given_name,
        name.family_name).execute(&mut *transaction).await?;
    sqlx::query!("update auth.\"invitation\" set status='canceled' where lower(email)=lower($1) and status='pending'",
        &email).execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(ScimJson(
        StatusCode::CREATED,
        user_json(&state.pool, user_row(&state.pool, org_id, id).await?).await?,
    ))
}

async fn apply_user(
    state: &AppState,
    org_id: Uuid,
    id: Uuid,
    input: UserInput,
) -> Result<serde_json::Value, ScimError> {
    let current = user_row(&state.pool, org_id, id).await?;
    if input.user_name.trim().is_empty() {
        return Err(ScimError::invalid("userName is required"));
    }
    let email = input_email(&input)?;
    let active = input.active.unwrap_or(true);
    let name = input.name.unwrap_or(ScimName {
        given_name: None,
        family_name: None,
    });
    let display_name = format!(
        "{} {}",
        name.given_name.as_deref().unwrap_or(""),
        name.family_name.as_deref().unwrap_or("")
    )
    .trim()
    .to_owned();
    let gateway_key = if !active {
        sqlx::query_scalar!(
            "select source_key_hash from gateway_key_selections where user_id=$1",
            id
        )
        .fetch_optional(&state.pool)
        .await?
        .flatten()
    } else {
        None
    };
    let mut transaction = state.pool.begin().await?;
    if current.active != active {
        revoke_credentials(&mut transaction, id, &current.subject).await?;
    }
    sqlx::query!("update users set email=$1,status=$2,active=$3,tokens_valid_after=case when $3 then tokens_valid_after else now() end,updated_at=now() where id=$4",
        &email,
        if active { "active" } else { "suspended" },
        active,
        id).execute(&mut *transaction).await?;
    sqlx::query!("update auth.\"user\" set email=$1,name=$2,banned=$3,\"banReason\"=$4,\"updatedAt\"=now() where id=$5",
        &email,
        if display_name.is_empty() { &email } else { &display_name },
        !active,
        if active { None } else { Some("Deactivated by identity provider") },
        &current.subject).execute(&mut *transaction).await?;
    sqlx::query!("update scim_user_resources set external_id=$1,user_name=$2,given_name=$3,family_name=$4,updated_at=now() where user_id=$5",
        input.external_id,
        input.user_name.trim(),
        name.given_name,
        name.family_name,
        id).execute(&mut *transaction).await?;
    if !active {
        sqlx::query!("delete from gateway_key_selections where user_id=$1", id)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    revoke_gateway_key(state, gateway_key).await;
    user_json(&state.pool, user_row(&state.pool, org_id, id).await?).await
}

async fn replace_user(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Path(id): Path<Uuid>,
    Json(input): Json<UserInput>,
) -> Result<ScimJson, ScimError> {
    Ok(ScimJson(
        StatusCode::OK,
        apply_user(&state, org_id, id, input).await?,
    ))
}

#[derive(Deserialize)]
struct PatchRequest {
    schemas: Vec<String>,
    #[serde(rename = "Operations")]
    operations: Vec<PatchOperation>,
}
#[derive(Deserialize)]
struct PatchOperation {
    op: String,
    path: Option<String>,
    value: Option<serde_json::Value>,
}

fn patch_string(value: &serde_json::Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

async fn patch_user(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Path(id): Path<Uuid>,
    Json(patch): Json<PatchRequest>,
) -> Result<ScimJson, ScimError> {
    if !patch.schemas.iter().any(|schema| schema == PATCH_SCHEMA) {
        return Err(ScimError::invalid("PatchOp schema is required"));
    }
    let row = user_row(&state.pool, org_id, id).await?;
    let mut input = UserInput {
        schemas: vec![USER_SCHEMA.into()],
        external_id: row.external_id,
        user_name: row.user_name,
        name: Some(ScimName {
            given_name: row.given_name,
            family_name: row.family_name,
        }),
        emails: vec![ScimEmail {
            value: row.email,
            primary: true,
            _kind: Some("work".into()),
        }],
        active: Some(row.active),
    };
    for operation in patch.operations {
        let op = operation.op.to_lowercase();
        if op != "add" && op != "replace" && op != "remove" {
            return Err(ScimError::invalid(
                "PATCH op must be add, replace, or remove",
            ));
        }
        if operation.path.is_none() {
            let object = operation
                .value
                .and_then(|value| value.as_object().cloned())
                .ok_or_else(|| ScimError::invalid_path("pathless PATCH value must be an object"))?;
            if let Some(value) = object.get("active").and_then(|v| v.as_bool()) {
                input.active = Some(value);
            }
            if let Some(value) = object.get("userName").and_then(patch_string) {
                input.user_name = value;
            }
            if let Some(value) = object.get("externalId").and_then(patch_string) {
                input.external_id = Some(value);
            } else if object.contains_key("externalId") {
                input.external_id = None;
            }
            if let Some(value) = object.get("name") {
                input.name = Some(
                    serde_json::from_value(value.clone())
                        .map_err(|_| ScimError::invalid("name must be a valid object"))?,
                );
            }
            if let Some(value) = object.get("emails") {
                input.emails = serde_json::from_value(value.clone())
                    .map_err(|_| ScimError::invalid("emails must be a valid array"))?;
            }
            continue;
        }
        let path = operation.path.as_deref().unwrap();
        match path {
            "active" => {
                input.active = Some(
                    operation
                        .value
                        .as_ref()
                        .and_then(|v| v.as_bool())
                        .ok_or_else(|| ScimError::invalid("active must be boolean"))?,
                )
            }
            "userName" => {
                input.user_name = operation
                    .value
                    .as_ref()
                    .and_then(patch_string)
                    .ok_or_else(|| ScimError::invalid("userName must be a string"))?
            }
            "externalId" => {
                input.external_id = if op == "remove" {
                    None
                } else {
                    Some(
                        operation
                            .value
                            .as_ref()
                            .and_then(patch_string)
                            .ok_or_else(|| ScimError::invalid("externalId must be a string"))?,
                    )
                }
            }
            "name.givenName" => {
                input.name.as_mut().unwrap().given_name = if op == "remove" {
                    None
                } else {
                    operation.value.as_ref().and_then(patch_string)
                }
            }
            "name.familyName" => {
                input.name.as_mut().unwrap().family_name = if op == "remove" {
                    None
                } else {
                    operation.value.as_ref().and_then(patch_string)
                }
            }
            "name" => {
                input.name = if op == "remove" {
                    None
                } else {
                    Some(
                        serde_json::from_value(
                            operation
                                .value
                                .ok_or_else(|| ScimError::invalid("name value is required"))?,
                        )
                        .map_err(|_| ScimError::invalid("name must be a valid object"))?,
                    )
                }
            }
            "emails" | "emails[type eq \"work\"].value" | "emails[primary eq true].value" => {
                let value = operation
                    .value
                    .as_ref()
                    .and_then(|value| {
                        value.as_str().map(str::to_owned).or_else(|| {
                            value
                                .as_array()
                                .and_then(|items| items.first())
                                .and_then(|item| item.get("value"))
                                .and_then(patch_string)
                        })
                    })
                    .ok_or_else(|| ScimError::invalid("a valid email value is required"))?;
                input.emails = vec![ScimEmail {
                    value,
                    primary: true,
                    _kind: Some("work".into()),
                }];
            }
            _ => {
                return Err(ScimError::invalid_path(format!(
                    "unsupported User PATCH path {path}"
                )))
            }
        }
    }
    Ok(ScimJson(
        StatusCode::OK,
        apply_user(&state, org_id, id, input).await?,
    ))
}

async fn delete_user(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Path(id): Path<Uuid>,
) -> Result<ScimJson, ScimError> {
    let target = user_row(&state.pool, org_id, id).await?;
    let gateway_key = sqlx::query_scalar!(
        "select source_key_hash from gateway_key_selections where user_id=$1",
        id
    )
    .fetch_optional(&state.pool)
    .await?
    .flatten();
    let mut transaction = state.pool.begin().await?;
    revoke_credentials(&mut transaction, id, &target.subject).await?;
    sqlx::query!("delete from scim_group_members where user_id=$1", id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query!("delete from scim_user_resources where user_id=$1", id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query!("update auth.\"invitation\" set \"inviterId\"=(select subject from users where organization_id=$1 and protected limit 1) \
         where \"inviterId\"=$2 and status='pending'",
        org_id,
        &target.subject)
    .execute(&mut *transaction)
    .await?;
    sqlx::query!("delete from auth.\"user\" where id=$1", &target.subject)
        .execute(&mut *transaction)
        .await?;
    sqlx::query!("delete from gateway_key_selections where user_id=$1", id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query!("update users set status='removed',active=false,tokens_valid_after=now(),updated_at=now() where id=$1",
        id).execute(&mut *transaction).await?;
    transaction.commit().await?;
    revoke_gateway_key(&state, gateway_key).await;
    Ok(ScimJson(StatusCode::NO_CONTENT, serde_json::Value::Null))
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GroupMemberInput {
    value: String,
    #[serde(rename = "display")]
    _display: Option<String>,
}
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GroupInput {
    #[serde(rename = "schemas", default)]
    _schemas: Vec<String>,
    external_id: Option<String>,
    display_name: String,
    #[serde(default)]
    members: Vec<GroupMemberInput>,
}

#[derive(FromRow)]
struct GroupRow {
    id: Uuid,
    external_id: Option<String>,
    display_name: String,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

async fn group_row(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<GroupRow, ScimError> {
    sqlx::query_as!(GroupRow,
        "select id,external_id,display_name,created_at,updated_at from scim_groups where organization_id=$1 and id=$2",
        org_id,
        id).fetch_optional(pool).await?.ok_or_else(|| ScimError::not_found("group"))
}

async fn group_json(pool: &PgPool, row: GroupRow) -> Result<serde_json::Value, ScimError> {
    let members = sqlx::query!("select u.id,u.email from users u join scim_group_members m on m.user_id=u.id where m.group_id=$1 order by lower(u.email)", row.id)
        .fetch_all(pool).await?.into_iter().map(|member| json!({"value":member.id,"display":member.email})).collect::<Vec<_>>();
    Ok(
        json!({"schemas":[GROUP_SCHEMA],"id":row.id,"externalId":row.external_id,"displayName":row.display_name,"members":members,
        "meta":{"resourceType":"Group","created":now_text(row.created_at),"lastModified":now_text(row.updated_at),"location":format!("/scim/v2/Groups/{}",row.id)}}),
    )
}

async fn list_groups(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Query(query): Query<ListQuery>,
) -> Result<ScimJson, ScimError> {
    let (start, count) = page(&query)?;
    let filter = eq_filter(query.filter.as_deref(), &["displayName", "externalId"])?;
    let (attribute, value) = filter.map(|(a, v)| (Some(a), Some(v))).unwrap_or_default();
    let total:i64=sqlx::query_scalar!("select count(*) as \"count!\" from scim_groups where organization_id=$1 and ($2::text is null or ($2='displayName' and display_name=$3) or ($2='externalId' and external_id=$3))",
        org_id,
        attribute.as_deref(),
        value.as_deref()).fetch_one(&state.pool).await?;
    let rows=sqlx::query_as!(GroupRow,
        "select id,external_id,display_name,created_at,updated_at from scim_groups where organization_id=$1 and ($2::text is null or ($2='displayName' and display_name=$3) or ($2='externalId' and external_id=$3)) order by display_name offset $4 limit $5",
        org_id,
        attribute.as_deref(),
        value.as_deref(),
        start-1,
        count).fetch_all(&state.pool).await?;
    let mut resources = Vec::with_capacity(rows.len());
    for row in rows {
        resources.push(group_json(&state.pool, row).await?);
    }
    Ok(ScimJson(
        StatusCode::OK,
        json!({"schemas":[LIST_SCHEMA],"totalResults":total,"startIndex":start,"itemsPerPage":resources.len(),"Resources":resources}),
    ))
}

async fn get_group(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Path(id): Path<Uuid>,
) -> Result<ScimJson, ScimError> {
    Ok(ScimJson(
        StatusCode::OK,
        group_json(&state.pool, group_row(&state.pool, org_id, id).await?).await?,
    ))
}

async fn replace_members(
    transaction: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    group_id: Uuid,
    members: &[GroupMemberInput],
    mappings: &BTreeMap<String, String>,
) -> Result<(), ScimError> {
    let old: Vec<Uuid> = sqlx::query_scalar!(
        "select user_id from scim_group_members where group_id=$1",
        group_id
    )
    .fetch_all(&mut **transaction)
    .await?;
    let mut next = Vec::new();
    for member in members {
        let id = Uuid::parse_str(&member.value)
            .map_err(|_| ScimError::invalid("group member value must be a user UUID"))?;
        let exists:bool=sqlx::query_scalar!("select exists(select 1 from scim_user_resources where organization_id=$1 and user_id=$2) as \"exists!\"",
        org_id,
        id).fetch_one(&mut **transaction).await?;
        if !exists {
            return Err(ScimError::invalid(format!(
                "user {} is not a SCIM resource",
                member.value
            )));
        }
        next.push(id);
    }
    sqlx::query!("delete from scim_group_members where group_id=$1", group_id)
        .execute(&mut **transaction)
        .await?;
    for id in &next {
        sqlx::query!(
            "insert into scim_group_members(group_id,user_id) values($1,$2)",
            group_id,
            id
        )
        .execute(&mut **transaction)
        .await?;
    }
    let mut affected = old;
    affected.extend(next);
    affected.sort();
    affected.dedup();
    for id in affected {
        sqlx::query!(
            "update scim_user_resources set updated_at=now() where user_id=$1",
            id
        )
        .execute(&mut **transaction)
        .await?;
        sync_role(transaction, id, mappings).await?;
    }
    Ok(())
}

async fn create_group(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Json(input): Json<GroupInput>,
) -> Result<ScimJson, ScimError> {
    if input.display_name.trim().is_empty() {
        return Err(ScimError::invalid("displayName is required"));
    }
    let id = Uuid::new_v4();
    let mut transaction = state.pool.begin().await?;
    sqlx::query!(
        "insert into scim_groups(id,organization_id,external_id,display_name) values($1,$2,$3,$4)",
        id,
        org_id,
        input.external_id,
        input.display_name.trim()
    )
    .execute(&mut *transaction)
    .await?;
    replace_members(
        &mut transaction,
        org_id,
        id,
        &input.members,
        &state.config.scim_group_role_mappings,
    )
    .await?;
    transaction.commit().await?;
    Ok(ScimJson(
        StatusCode::CREATED,
        group_json(&state.pool, group_row(&state.pool, org_id, id).await?).await?,
    ))
}

async fn replace_group(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Path(id): Path<Uuid>,
    Json(input): Json<GroupInput>,
) -> Result<ScimJson, ScimError> {
    group_row(&state.pool, org_id, id).await?;
    if input.display_name.trim().is_empty() {
        return Err(ScimError::invalid("displayName is required"));
    }
    let mut transaction = state.pool.begin().await?;
    sqlx::query!(
        "update scim_groups set external_id=$1,display_name=$2,updated_at=now() where id=$3",
        input.external_id,
        input.display_name.trim(),
        id
    )
    .execute(&mut *transaction)
    .await?;
    replace_members(
        &mut transaction,
        org_id,
        id,
        &input.members,
        &state.config.scim_group_role_mappings,
    )
    .await?;
    transaction.commit().await?;
    Ok(ScimJson(
        StatusCode::OK,
        group_json(&state.pool, group_row(&state.pool, org_id, id).await?).await?,
    ))
}

fn member_id_from_path(path: &str) -> Option<Uuid> {
    let value = path
        .strip_prefix("members[value eq \"")?
        .strip_suffix("\"]")?;
    Uuid::parse_str(value).ok()
}

async fn patch_group(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Path(id): Path<Uuid>,
    Json(patch): Json<PatchRequest>,
) -> Result<ScimJson, ScimError> {
    let current = group_row(&state.pool, org_id, id).await?;
    let mut display = current.display_name;
    let mut external = current.external_id;
    let existing=sqlx::query!("select u.id,u.email from users u join scim_group_members m on m.user_id=u.id where m.group_id=$1", id).fetch_all(&state.pool).await?;
    let mut members = existing
        .into_iter()
        .map(|member| GroupMemberInput {
            value: member.id.to_string(),
            _display: Some(member.email),
        })
        .collect::<Vec<_>>();
    for operation in patch.operations {
        let op = operation.op.to_lowercase();
        let path = operation.path.as_deref();
        match path {
            Some("displayName") => {
                display = operation
                    .value
                    .as_ref()
                    .and_then(patch_string)
                    .ok_or_else(|| ScimError::invalid("displayName must be a string"))?
            }
            Some("externalId") => {
                external = if op == "remove" {
                    None
                } else {
                    operation.value.as_ref().and_then(patch_string)
                }
            }
            Some("members") => {
                let values = operation
                    .value
                    .and_then(|value| value.as_array().cloned())
                    .ok_or_else(|| ScimError::invalid("members must be an array"))?;
                let parsed = values
                    .into_iter()
                    .map(|value| {
                        serde_json::from_value::<GroupMemberInput>(value)
                            .map_err(|_| ScimError::invalid("invalid member"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if op == "replace" {
                    members = parsed
                } else if op == "add" {
                    members.extend(parsed)
                } else {
                    members.clear()
                }
            }
            Some(path) if member_id_from_path(path).is_some() => {
                if op != "remove" {
                    return Err(ScimError::invalid_path(
                        "filtered member paths support remove only",
                    ));
                }
                let target = member_id_from_path(path).unwrap();
                members.retain(|member| member.value != target.to_string());
            }
            None => {
                let object = operation
                    .value
                    .and_then(|value| value.as_object().cloned())
                    .ok_or_else(|| {
                        ScimError::invalid_path("pathless PATCH value must be an object")
                    })?;
                if let Some(value) = object.get("displayName").and_then(patch_string) {
                    display = value
                }
                if let Some(values) = object.get("members").and_then(|value| value.as_array()) {
                    let parsed = values
                        .iter()
                        .cloned()
                        .map(|value| {
                            serde_json::from_value::<GroupMemberInput>(value)
                                .map_err(|_| ScimError::invalid("invalid member"))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    if op == "replace" {
                        members = parsed
                    } else {
                        members.extend(parsed)
                    }
                }
            }
            Some(path) => {
                return Err(ScimError::invalid_path(format!(
                    "unsupported Group PATCH path {path}"
                )))
            }
        }
    }
    members.sort_by(|a, b| a.value.cmp(&b.value));
    members.dedup_by(|a, b| a.value == b.value);
    replace_group(
        State(state),
        Extension(ScimPrincipal(org_id)),
        Path(id),
        Json(GroupInput {
            _schemas: vec![GROUP_SCHEMA.into()],
            external_id: external,
            display_name: display,
            members,
        }),
    )
    .await
}

async fn delete_group(
    State(state): State<Arc<AppState>>,
    Extension(ScimPrincipal(org_id)): Extension<ScimPrincipal>,
    Path(id): Path<Uuid>,
) -> Result<ScimJson, ScimError> {
    group_row(&state.pool, org_id, id).await?;
    let members: Vec<Uuid> = sqlx::query_scalar!(
        "select user_id from scim_group_members where group_id=$1",
        id
    )
    .fetch_all(&state.pool)
    .await?;
    let mut transaction = state.pool.begin().await?;
    sqlx::query!("delete from scim_groups where id=$1", id)
        .execute(&mut *transaction)
        .await?;
    for user in members {
        sqlx::query!(
            "update scim_user_resources set updated_at=now() where user_id=$1",
            user
        )
        .execute(&mut *transaction)
        .await?;
        sync_role(
            &mut transaction,
            user,
            &state.config.scim_group_role_mappings,
        )
        .await?
    }
    transaction.commit().await?;
    Ok(ScimJson(StatusCode::NO_CONTENT, serde_json::Value::Null))
}

async fn service_provider_config() -> Result<ScimJson, ScimError> {
    Ok(ScimJson(
        StatusCode::OK,
        json!({"schemas":["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],"patch":{"supported":true},"bulk":{"supported":false,"maxOperations":0,"maxPayloadSize":0},"filter":{"supported":true,"maxResults":200},"changePassword":{"supported":false},"sort":{"supported":false},"etag":{"supported":false},"authenticationSchemes":[{"type":"oauthbearertoken","name":"Bearer token","description":"Deployment-managed SCIM bearer token","specUri":"https://www.rfc-editor.org/info/rfc6750","primary":true}],"meta":{"resourceType":"ServiceProviderConfig","location":"/scim/v2/ServiceProviderConfig"}}),
    ))
}
async fn resource_types() -> Result<ScimJson, ScimError> {
    Ok(ScimJson(
        StatusCode::OK,
        json!({"schemas":[LIST_SCHEMA],"totalResults":2,"startIndex":1,"itemsPerPage":2,"Resources":[{"schemas":["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],"id":"User","name":"User","endpoint":"/Users","schema":USER_SCHEMA},{"schemas":["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],"id":"Group","name":"Group","endpoint":"/Groups","schema":GROUP_SCHEMA}]}),
    ))
}
async fn schemas() -> Result<ScimJson, ScimError> {
    Ok(ScimJson(
        StatusCode::OK,
        json!({"schemas":[LIST_SCHEMA],"totalResults":2,"startIndex":1,"itemsPerPage":2,"Resources":[{"id":USER_SCHEMA,"name":"User","description":"User Account","attributes":[]},{"id":GROUP_SCHEMA,"name":"Group","description":"Group","attributes":[]}]}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn constant_time_token_comparison_matches_only_equal_values() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "other"));
    }
    #[test]
    fn parses_okta_equality_filters() {
        assert_eq!(
            eq_filter(Some("userName eq \"dev@example.com\""), &["userName"]).unwrap(),
            Some(("userName".into(), "dev@example.com".into()))
        );
        assert!(eq_filter(Some("name co \"dev\""), &["userName"]).is_err());
    }
}
