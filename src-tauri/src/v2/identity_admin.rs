//! Tenant-scoped user, membership, role and permission administration.
//!
//! Platform provisioning is deliberately not exposed here. Every method starts
//! from an authenticated tenant session, requires an active `tenant_admin`
//! system role in addition to the named permission, and derives all audit
//! actor fields from that session.

use super::auth::{AuthError, AuthorizationDenial, PasswordService};
use super::network::{NetworkResult, NetworkService, NetworkServiceError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use std::collections::HashSet;
use uuid::Uuid;

pub const PERMISSION_USERS_READ: &str = "identity.users.read";
pub const PERMISSION_USERS_WRITE: &str = "identity.users.write";
pub const PERMISSION_MEMBERSHIPS_WRITE: &str = "identity.memberships.write";
pub const PERMISSION_PERMISSIONS_READ: &str = "identity.permissions.read";

const TENANT_ADMIN_ROLE: &str = "tenant_admin";
const DEFAULT_PAGE_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 200;
const MIN_PASSWORD_BYTES: usize = 12;
const MAX_PASSWORD_BYTES: usize = 128;
const CREATE_USER_SCOPE: &str = "identity_create_user";
const DISABLE_USER_SCOPE: &str = "identity_disable_user";
const REPLACE_ROLES_SCOPE: &str = "identity_replace_membership_roles";
const ADMIN_MUTATION_LOCK_NAMESPACE: i64 = 0x4944_454e_5449_5459;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ListTenantUsersRequest {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub include_disabled: bool,
    #[serde(default)]
    pub after_user_id: Option<Uuid>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateTenantUserRequest {
    pub request_id: String,
    pub login: String,
    pub display_name: String,
    #[serde(default)]
    pub email: Option<String>,
    pub password: String,
    pub role_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DisableTenantUserRequest {
    pub request_id: String,
    pub user_id: Uuid,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReplaceMembershipRolesRequest {
    pub request_id: String,
    pub membership_id: Uuid,
    #[serde(default)]
    pub role_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MembershipPermissionsRequest {
    pub membership_id: Uuid,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TenantRoleSummary {
    pub role_id: Uuid,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub system_role: bool,
    pub permission_codes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TenantUserSummary {
    pub user_id: Uuid,
    pub login: String,
    pub display_name: String,
    pub email: Option<String>,
    pub account_status: String,
    pub membership_id: Uuid,
    pub membership_status: String,
    pub consumes_license_seat: bool,
    pub role_codes: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListTenantUsersResponse {
    pub users: Vec<TenantUserSummary>,
    pub next_after_user_id: Option<Uuid>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateTenantUserResponse {
    pub user: TenantUserSummary,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DisableTenantUserResponse {
    pub user_id: Uuid,
    pub membership_id: Uuid,
    pub account_status: String,
    pub membership_status: String,
    pub revoked_session_count: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MembershipPermissionsResponse {
    pub user_id: Uuid,
    pub membership_id: Uuid,
    pub account_status: String,
    pub membership_status: String,
    pub role_codes: Vec<String>,
    pub permission_codes: Vec<String>,
}

impl NetworkService {
    pub async fn list_tenant_users(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: ListTenantUsersRequest,
    ) -> NetworkResult<ListTenantUsersResponse> {
        let request = normalize_list_request(request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_USERS_READ)
            .await?;
        let membership_id = authorized.session().identity.membership_id;
        let transaction = authorized.sqlx_transaction();
        ensure_tenant_admin(transaction, tenant_id, membership_id).await?;

        let mut rows = sqlx::query(
            r#"
            SELECT u.id AS user_id,
                   u.login,
                   u.display_name,
                   u.email,
                   u.status AS account_status,
                   m.id AS membership_id,
                   m.status AS membership_status,
                   m.consumes_license_seat,
                   u.created_at::text AS created_at,
                   u.updated_at::text AS updated_at,
                   COALESCE((
                       SELECT array_agg(r.code ORDER BY r.code)
                         FROM membership_roles mr
                         JOIN roles r
                           ON r.tenant_id = mr.tenant_id
                          AND r.id = mr.role_id
                        WHERE mr.tenant_id = u.tenant_id
                          AND mr.membership_id = m.id
                          AND r.active
                   ), ARRAY[]::text[]) AS role_codes
              FROM users u
              JOIN memberships m
                ON m.tenant_id = u.tenant_id AND m.user_id = u.id
             WHERE u.tenant_id = $1
               AND ($2::boolean OR (u.status = 'active' AND m.status = 'active'))
               AND ($3::text IS NULL OR u.normalized_login LIKE $3
                    OR lower(u.display_name) LIKE $3
                    OR lower(COALESCE(u.email, '')) LIKE $3)
               AND ($4::uuid IS NULL OR u.id > $4)
             ORDER BY u.id
             LIMIT $5
            "#,
        )
        .bind(tenant_id)
        .bind(request.include_disabled)
        .bind(request.search.as_deref())
        .bind(request.after_user_id)
        .bind(i64::from(request.limit + 1))
        .fetch_all(&mut **transaction)
        .await?;

        let has_more = rows.len() > request.limit as usize;
        if has_more {
            rows.pop();
        }
        let users = rows
            .into_iter()
            .map(user_summary_from_row)
            .collect::<NetworkResult<Vec<_>>>()?;
        let next_after_user_id = has_more
            .then(|| users.last().map(|user| user.user_id))
            .flatten();
        authorized.commit().await?;
        Ok(ListTenantUsersResponse {
            users,
            next_after_user_id,
        })
    }

    pub async fn create_tenant_user(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: CreateTenantUserRequest,
    ) -> NetworkResult<CreateTenantUserResponse> {
        let request = normalize_create_request(request)?;
        let digest = identity_request_digest(&json!({
            "login": &request.normalized_login,
            "display_name": &request.display_name,
            "email": &request.email,
            "password_bytes": request.password.len(),
            "role_ids": &request.role_ids,
        }))?;
        let mut authorized = self
            .database()
            .begin_serialized_authorized_request(
                tenant_id,
                session_token,
                PERMISSION_USERS_WRITE,
                ADMIN_MUTATION_LOCK_NAMESPACE,
            )
            .await?;
        let session = authorized.session().clone();
        let transaction = authorized.sqlx_transaction();
        ensure_tenant_admin(transaction, tenant_id, session.identity.membership_id).await?;
        let password_hash = self
            .password_service()
            .hash_password(&request.password)
            .map_err(NetworkServiceError::Auth)?;
        if let Some(response) = claim_create_user_idempotency(
            transaction,
            tenant_id,
            &request.request_id,
            &digest,
            &request.password,
            &password_hash,
            self.password_service(),
        )
        .await?
        {
            authorized.commit().await?;
            return Ok(response);
        }
        ensure_license_seat_available(transaction, tenant_id).await?;
        let roles = load_active_roles(transaction, tenant_id, &request.role_ids).await?;

        let user_id = Uuid::now_v7();
        let membership_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO users
                (tenant_id, id, login, normalized_login, display_name, email)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(tenant_id)
        .bind(user_id)
        .bind(&request.login)
        .bind(&request.normalized_login)
        .bind(&request.display_name)
        .bind(&request.email)
        .execute(&mut **transaction)
        .await
        .map_err(|error| identity_conflict_or_sqlx("user", &request.login, error))?;
        sqlx::query(
            "INSERT INTO credentials (tenant_id, user_id, password_hash) VALUES ($1, $2, $3)",
        )
        .bind(tenant_id)
        .bind(user_id)
        .bind(password_hash)
        .execute(&mut **transaction)
        .await?;
        sqlx::query(
            "INSERT INTO memberships (tenant_id, id, user_id, status, consumes_license_seat) VALUES ($1, $2, $3, 'active', true)",
        )
        .bind(tenant_id)
        .bind(membership_id)
        .bind(user_id)
        .execute(&mut **transaction)
        .await?;
        for (role_id, _) in &roles {
            sqlx::query(
                "INSERT INTO membership_roles (tenant_id, membership_id, role_id, assigned_by_user_id) VALUES ($1, $2, $3, $4)",
            )
            .bind(tenant_id)
            .bind(membership_id)
            .bind(role_id)
            .bind(session.identity.user_id)
            .execute(&mut **transaction)
            .await?;
        }

        write_identity_audit(
            transaction,
            tenant_id,
            &session,
            IdentityAuditEvent {
                action: "identity.user.created",
                entity_type: "user",
                entity_id: user_id,
                request_id: &request.request_id,
                details: json!({
                    "login": request.login,
                    "display_name": request.display_name,
                    "membership_id": membership_id,
                    "role_codes": roles.iter().map(|(_, code)| code).collect::<Vec<_>>(),
                }),
            },
        )
        .await?;
        let user = load_user_summary(transaction, tenant_id, user_id).await?;
        let response = CreateTenantUserResponse { user };
        finish_identity_idempotency(
            transaction,
            tenant_id,
            CREATE_USER_SCOPE,
            &request.request_id,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn disable_tenant_user(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: DisableTenantUserRequest,
    ) -> NetworkResult<DisableTenantUserResponse> {
        let request_id = required_text("request_id", request.request_id, 200)?;
        let digest = identity_request_digest(&json!({ "user_id": request.user_id }))?;
        let mut authorized = self
            .database()
            .begin_serialized_authorized_request(
                tenant_id,
                session_token,
                PERMISSION_USERS_WRITE,
                ADMIN_MUTATION_LOCK_NAMESPACE,
            )
            .await?;
        let session = authorized.session().clone();
        let transaction = authorized.sqlx_transaction();
        ensure_tenant_admin(transaction, tenant_id, session.identity.membership_id).await?;
        if request.user_id == session.identity.user_id {
            return Err(NetworkServiceError::Conflict {
                entity: "user".to_owned(),
                key: "current_session".to_owned(),
            });
        }
        if let Some(response) = claim_identity_idempotency(
            transaction,
            tenant_id,
            DISABLE_USER_SCOPE,
            &request_id,
            &digest,
        )
        .await?
        {
            authorized.commit().await?;
            return Ok(response);
        }

        let target = lock_user_membership(transaction, tenant_id, request.user_id).await?;
        let role_codes =
            membership_role_codes(transaction, tenant_id, target.membership_id).await?;
        if target.is_active() && role_codes.iter().any(|role| role == TENANT_ADMIN_ROLE) {
            protect_last_tenant_admin(transaction, tenant_id, target.membership_id).await?;
        }

        sqlx::query(
            "UPDATE users SET status = 'disabled', updated_at = CURRENT_TIMESTAMP WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(request.user_id)
        .execute(&mut **transaction)
        .await?;
        sqlx::query(
            "UPDATE memberships SET status = 'suspended', updated_at = CURRENT_TIMESTAMP WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(target.membership_id)
        .execute(&mut **transaction)
        .await?;
        let revoked_session_count = sqlx::query(
            "UPDATE sessions SET revoked_at = CURRENT_TIMESTAMP, revoke_reason = 'user_disabled' WHERE tenant_id = $1 AND user_id = $2 AND revoked_at IS NULL",
        )
        .bind(tenant_id)
        .bind(request.user_id)
        .execute(&mut **transaction)
        .await?
        .rows_affected();
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = CURRENT_TIMESTAMP WHERE tenant_id = $1 AND user_id = $2 AND revoked_at IS NULL",
        )
        .bind(tenant_id)
        .bind(request.user_id)
        .execute(&mut **transaction)
        .await?;

        write_identity_audit(
            transaction,
            tenant_id,
            &session,
            IdentityAuditEvent {
                action: "identity.user.disabled",
                entity_type: "user",
                entity_id: request.user_id,
                request_id: &request_id,
                details: json!({
                    "membership_id": target.membership_id,
                    "previous_account_status": target.account_status,
                    "previous_membership_status": target.membership_status,
                    "revoked_session_count": revoked_session_count,
                }),
            },
        )
        .await?;
        let response = DisableTenantUserResponse {
            user_id: request.user_id,
            membership_id: target.membership_id,
            account_status: "disabled".to_owned(),
            membership_status: "suspended".to_owned(),
            revoked_session_count,
        };
        finish_identity_idempotency(
            transaction,
            tenant_id,
            DISABLE_USER_SCOPE,
            &request_id,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn replace_membership_roles(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: ReplaceMembershipRolesRequest,
    ) -> NetworkResult<MembershipPermissionsResponse> {
        let request = normalize_replace_roles_request(request)?;
        let digest = identity_request_digest(&json!({
            "membership_id": request.membership_id,
            "role_ids": &request.role_ids,
        }))?;
        let mut authorized = self
            .database()
            .begin_serialized_authorized_request(
                tenant_id,
                session_token,
                PERMISSION_MEMBERSHIPS_WRITE,
                ADMIN_MUTATION_LOCK_NAMESPACE,
            )
            .await?;
        let session = authorized.session().clone();
        let transaction = authorized.sqlx_transaction();
        ensure_tenant_admin(transaction, tenant_id, session.identity.membership_id).await?;
        if let Some(response) = claim_identity_idempotency(
            transaction,
            tenant_id,
            REPLACE_ROLES_SCOPE,
            &request.request_id,
            &digest,
        )
        .await?
        {
            authorized.commit().await?;
            return Ok(response);
        }
        let target = lock_membership_user(transaction, tenant_id, request.membership_id).await?;
        let previous_roles =
            membership_role_codes(transaction, tenant_id, request.membership_id).await?;
        let roles = load_active_roles(transaction, tenant_id, &request.role_ids).await?;
        let next_role_codes = roles
            .iter()
            .map(|(_, code)| code.clone())
            .collect::<Vec<_>>();
        let was_admin =
            target.is_active() && previous_roles.iter().any(|role| role == TENANT_ADMIN_ROLE);
        let remains_admin =
            target.is_active() && next_role_codes.iter().any(|role| role == TENANT_ADMIN_ROLE);
        if was_admin && !remains_admin {
            protect_last_tenant_admin(transaction, tenant_id, request.membership_id).await?;
        }

        sqlx::query("DELETE FROM membership_roles WHERE tenant_id = $1 AND membership_id = $2")
            .bind(tenant_id)
            .bind(request.membership_id)
            .execute(&mut **transaction)
            .await?;
        for (role_id, _) in &roles {
            sqlx::query(
                "INSERT INTO membership_roles (tenant_id, membership_id, role_id, assigned_by_user_id) VALUES ($1, $2, $3, $4)",
            )
            .bind(tenant_id)
            .bind(request.membership_id)
            .bind(role_id)
            .bind(session.identity.user_id)
            .execute(&mut **transaction)
            .await?;
        }
        write_identity_audit(
            transaction,
            tenant_id,
            &session,
            IdentityAuditEvent {
                action: "identity.membership.roles_replaced",
                entity_type: "membership",
                entity_id: request.membership_id,
                request_id: &request.request_id,
                details: json!({
                    "user_id": target.user_id,
                    "previous_role_codes": previous_roles,
                    "role_codes": next_role_codes,
                }),
            },
        )
        .await?;
        let response =
            load_effective_permissions(transaction, tenant_id, request.membership_id).await?;
        finish_identity_idempotency(
            transaction,
            tenant_id,
            REPLACE_ROLES_SCOPE,
            &request.request_id,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn list_tenant_roles(
        &self,
        tenant_id: Uuid,
        session_token: &str,
    ) -> NetworkResult<Vec<TenantRoleSummary>> {
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_PERMISSIONS_READ)
            .await?;
        let membership_id = authorized.session().identity.membership_id;
        let transaction = authorized.sqlx_transaction();
        ensure_tenant_admin(transaction, tenant_id, membership_id).await?;
        let rows = sqlx::query(
            r#"
            SELECT r.id, r.code, r.name, r.description, r.system_role,
                   COALESCE(array_agg(p.code ORDER BY p.code)
                       FILTER (WHERE p.id IS NOT NULL), ARRAY[]::text[])
                       AS permission_codes
              FROM roles r
              LEFT JOIN role_permissions rp
                ON rp.tenant_id = r.tenant_id AND rp.role_id = r.id
              LEFT JOIN permissions p
                ON p.tenant_id = rp.tenant_id AND p.id = rp.permission_id
             WHERE r.tenant_id = $1 AND r.active
             GROUP BY r.id, r.code, r.name, r.description, r.system_role
             ORDER BY r.system_role DESC, r.code
            "#,
        )
        .bind(tenant_id)
        .fetch_all(&mut **transaction)
        .await?;
        let roles = rows
            .into_iter()
            .map(|row| {
                Ok(TenantRoleSummary {
                    role_id: row.try_get("id")?,
                    code: row.try_get("code")?,
                    name: row.try_get("name")?,
                    description: row.try_get("description")?,
                    system_role: row.try_get("system_role")?,
                    permission_codes: row.try_get("permission_codes")?,
                })
            })
            .collect::<NetworkResult<Vec<_>>>()?;
        authorized.commit().await?;
        Ok(roles)
    }

    pub async fn membership_effective_permissions(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: MembershipPermissionsRequest,
    ) -> NetworkResult<MembershipPermissionsResponse> {
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_PERMISSIONS_READ)
            .await?;
        let actor_membership_id = authorized.session().identity.membership_id;
        let transaction = authorized.sqlx_transaction();
        ensure_tenant_admin(transaction, tenant_id, actor_membership_id).await?;
        let response =
            load_effective_permissions(transaction, tenant_id, request.membership_id).await?;
        authorized.commit().await?;
        Ok(response)
    }
}

#[derive(Debug)]
struct NormalizedListRequest {
    search: Option<String>,
    include_disabled: bool,
    after_user_id: Option<Uuid>,
    limit: u32,
}

#[derive(Debug)]
struct NormalizedCreateRequest {
    request_id: String,
    login: String,
    normalized_login: String,
    display_name: String,
    email: Option<String>,
    password: String,
    role_ids: Vec<Uuid>,
}

#[derive(Debug)]
struct NormalizedReplaceRolesRequest {
    request_id: String,
    membership_id: Uuid,
    role_ids: Vec<Uuid>,
}

#[derive(Debug)]
struct LockedMembership {
    user_id: Uuid,
    membership_id: Uuid,
    account_status: String,
    membership_status: String,
}

impl LockedMembership {
    fn is_active(&self) -> bool {
        self.account_status == "active" && self.membership_status == "active"
    }
}

fn normalize_list_request(request: ListTenantUsersRequest) -> NetworkResult<NormalizedListRequest> {
    let search = request
        .search
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"));
    let limit = request.limit.unwrap_or(DEFAULT_PAGE_SIZE);
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(NetworkServiceError::Invalid(format!(
            "limit must be between 1 and {MAX_PAGE_SIZE}"
        )));
    }
    Ok(NormalizedListRequest {
        search,
        include_disabled: request.include_disabled,
        after_user_id: request.after_user_id,
        limit,
    })
}

fn normalize_create_request(
    request: CreateTenantUserRequest,
) -> NetworkResult<NormalizedCreateRequest> {
    let request_id = required_text("request_id", request.request_id, 200)?;
    let login = required_text("login", request.login, 128)?;
    if !login
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@'))
    {
        return Err(NetworkServiceError::Invalid(
            "login contains unsupported characters".to_owned(),
        ));
    }
    let normalized_login = login.to_ascii_lowercase();
    let display_name = required_text("display_name", request.display_name, 200)?;
    let email = request
        .email
        .map(|email| email.trim().to_ascii_lowercase())
        .filter(|email| !email.is_empty());
    if email
        .as_ref()
        .is_some_and(|email| email.len() > 320 || !email.contains('@'))
    {
        return Err(NetworkServiceError::Invalid("email is invalid".to_owned()));
    }
    if !(MIN_PASSWORD_BYTES..=MAX_PASSWORD_BYTES).contains(&request.password.len()) {
        return Err(NetworkServiceError::Invalid(format!(
            "password must contain between {MIN_PASSWORD_BYTES} and {MAX_PASSWORD_BYTES} bytes"
        )));
    }
    let role_ids = unique_role_ids(request.role_ids)?;
    if role_ids.is_empty() {
        return Err(NetworkServiceError::Invalid(
            "at least one role is required".to_owned(),
        ));
    }
    Ok(NormalizedCreateRequest {
        request_id,
        login,
        normalized_login,
        display_name,
        email,
        password: request.password,
        role_ids,
    })
}

fn normalize_replace_roles_request(
    request: ReplaceMembershipRolesRequest,
) -> NetworkResult<NormalizedReplaceRolesRequest> {
    Ok(NormalizedReplaceRolesRequest {
        request_id: required_text("request_id", request.request_id, 200)?,
        membership_id: request.membership_id,
        role_ids: unique_role_ids(request.role_ids)?,
    })
}

fn unique_role_ids(role_ids: Vec<Uuid>) -> NetworkResult<Vec<Uuid>> {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(role_ids.len());
    for role_id in role_ids {
        if !seen.insert(role_id) {
            return Err(NetworkServiceError::Invalid(format!(
                "duplicate role_id {role_id}"
            )));
        }
        unique.push(role_id);
    }
    unique.sort_unstable();
    Ok(unique)
}

fn required_text(field: &str, value: String, max_bytes: usize) -> NetworkResult<String> {
    let value = value.trim().to_owned();
    if value.is_empty() || value.len() > max_bytes {
        return Err(NetworkServiceError::Invalid(format!(
            "{field} must contain between 1 and {max_bytes} bytes"
        )));
    }
    Ok(value)
}

async fn ensure_tenant_admin(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    membership_id: Uuid,
) -> NetworkResult<()> {
    let is_admin: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
              FROM membership_roles mr
              JOIN roles r
                ON r.tenant_id = mr.tenant_id AND r.id = mr.role_id
             WHERE mr.tenant_id = $1
               AND mr.membership_id = $2
               AND r.code = 'tenant_admin'
               AND r.active
        )
        "#,
    )
    .bind(tenant_id)
    .bind(membership_id)
    .fetch_one(&mut **transaction)
    .await?;
    if !is_admin {
        return Err(NetworkServiceError::Auth(AuthError::AccessDenied(
            AuthorizationDenial::MissingPermission,
        )));
    }
    Ok(())
}

async fn ensure_license_seat_available(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
) -> NetworkResult<()> {
    // Serializing only user creation for this tenant avoids oversubscribing
    // the final seat while keeping unrelated tenants fully concurrent.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
        .bind(tenant_id)
        .execute(&mut **transaction)
        .await?;
    let row = sqlx::query(
        r#"
        SELECT (SELECT count(*)::bigint
                  FROM memberships
                 WHERE tenant_id = $1
                   AND status = 'active'
                   AND consumes_license_seat) AS consumed_seats,
               COALESCE((SELECT max(seat_limit)::bigint
                  FROM license_entitlements
                 WHERE tenant_id = $1
                   AND status = 'active'
                   AND revoked_at IS NULL
                   AND verified_at IS NOT NULL
                   AND starts_at <= CURRENT_TIMESTAMP
                   AND CURRENT_TIMESTAMP < expires_at), 0) AS seat_limit
        "#,
    )
    .bind(tenant_id)
    .fetch_one(&mut **transaction)
    .await?;
    let consumed: i64 = row.try_get("consumed_seats")?;
    let limit: i64 = row.try_get("seat_limit")?;
    if consumed >= limit {
        return Err(NetworkServiceError::Conflict {
            entity: "license_seat".to_owned(),
            key: format!("{consumed}/{limit}"),
        });
    }
    Ok(())
}

async fn load_active_roles(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    role_ids: &[Uuid],
) -> NetworkResult<Vec<(Uuid, String)>> {
    if role_ids.is_empty() {
        return Ok(Vec::new());
    }
    let roles = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, code FROM roles WHERE tenant_id = $1 AND id = ANY($2) AND active ORDER BY id FOR SHARE",
    )
    .bind(tenant_id)
    .bind(role_ids)
    .fetch_all(&mut **transaction)
    .await?;
    if roles.len() != role_ids.len() {
        return Err(NetworkServiceError::Invalid(
            "one or more roles do not exist or are inactive in this tenant".to_owned(),
        ));
    }
    Ok(roles)
}

async fn lock_user_membership(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> NetworkResult<LockedMembership> {
    lock_membership_query(transaction, tenant_id, "u.id", user_id).await
}

async fn lock_membership_user(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    membership_id: Uuid,
) -> NetworkResult<LockedMembership> {
    lock_membership_query(transaction, tenant_id, "m.id", membership_id).await
}

async fn lock_membership_query(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    field: &str,
    id: Uuid,
) -> NetworkResult<LockedMembership> {
    let sql = format!(
        "SELECT u.id AS user_id, m.id AS membership_id, u.status AS account_status, m.status AS membership_status FROM users u JOIN memberships m ON m.tenant_id = u.tenant_id AND m.user_id = u.id WHERE u.tenant_id = $1 AND {field} = $2 FOR UPDATE OF u, m"
    );
    let row = sqlx::query(&sql)
        .bind(tenant_id)
        .bind(id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| {
            NetworkServiceError::Invalid("tenant user or membership not found".to_owned())
        })?;
    Ok(LockedMembership {
        user_id: row.try_get("user_id")?,
        membership_id: row.try_get("membership_id")?,
        account_status: row.try_get("account_status")?,
        membership_status: row.try_get("membership_status")?,
    })
}

async fn membership_role_codes(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    membership_id: Uuid,
) -> NetworkResult<Vec<String>> {
    sqlx::query_scalar(
        "SELECT r.code FROM membership_roles mr JOIN roles r ON r.tenant_id = mr.tenant_id AND r.id = mr.role_id WHERE mr.tenant_id = $1 AND mr.membership_id = $2 AND r.active ORDER BY r.code",
    )
    .bind(tenant_id)
    .bind(membership_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(Into::into)
}

async fn protect_last_tenant_admin(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    target_membership_id: Uuid,
) -> NetworkResult<()> {
    let administrators: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT m.id
          FROM memberships m
          JOIN users u
            ON u.tenant_id = m.tenant_id AND u.id = m.user_id
          JOIN membership_roles mr
            ON mr.tenant_id = m.tenant_id AND mr.membership_id = m.id
          JOIN roles r
            ON r.tenant_id = mr.tenant_id AND r.id = mr.role_id
         WHERE m.tenant_id = $1
           AND m.status = 'active'
           AND u.status = 'active'
           AND r.code = 'tenant_admin'
           AND r.active
         ORDER BY m.id
         FOR UPDATE OF m, u, mr, r
        "#,
    )
    .bind(tenant_id)
    .fetch_all(&mut **transaction)
    .await?;
    if administrators.as_slice() == [target_membership_id] {
        return Err(NetworkServiceError::Conflict {
            entity: "tenant_admin".to_owned(),
            key: "last_active_administrator".to_owned(),
        });
    }
    Ok(())
}

async fn load_user_summary(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> NetworkResult<TenantUserSummary> {
    let row = sqlx::query(
        r#"
        SELECT u.id AS user_id, u.login, u.display_name, u.email,
               u.status AS account_status, m.id AS membership_id,
               m.status AS membership_status, m.consumes_license_seat,
               u.created_at::text AS created_at, u.updated_at::text AS updated_at,
               COALESCE((SELECT array_agg(r.code ORDER BY r.code)
                           FROM membership_roles mr
                           JOIN roles r ON r.tenant_id = mr.tenant_id AND r.id = mr.role_id
                          WHERE mr.tenant_id = u.tenant_id
                            AND mr.membership_id = m.id AND r.active), ARRAY[]::text[]) AS role_codes
          FROM users u
          JOIN memberships m ON m.tenant_id = u.tenant_id AND m.user_id = u.id
         WHERE u.tenant_id = $1 AND u.id = $2
        "#,
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_one(&mut **transaction)
    .await?;
    user_summary_from_row(row)
}

fn user_summary_from_row(row: sqlx::postgres::PgRow) -> NetworkResult<TenantUserSummary> {
    Ok(TenantUserSummary {
        user_id: row.try_get("user_id")?,
        login: row.try_get("login")?,
        display_name: row.try_get("display_name")?,
        email: row.try_get("email")?,
        account_status: row.try_get("account_status")?,
        membership_id: row.try_get("membership_id")?,
        membership_status: row.try_get("membership_status")?,
        consumes_license_seat: row.try_get("consumes_license_seat")?,
        role_codes: row.try_get("role_codes")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

async fn load_effective_permissions(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    membership_id: Uuid,
) -> NetworkResult<MembershipPermissionsResponse> {
    let row = sqlx::query(
        r#"
        SELECT u.id AS user_id, m.id AS membership_id,
               u.status AS account_status, m.status AS membership_status,
               COALESCE(array_agg(DISTINCT r.code ORDER BY r.code)
                   FILTER (WHERE r.id IS NOT NULL AND r.active), ARRAY[]::text[]) AS role_codes,
               COALESCE(array_agg(DISTINCT p.code ORDER BY p.code)
                   FILTER (WHERE r.active AND p.id IS NOT NULL), ARRAY[]::text[]) AS permission_codes
          FROM memberships m
          JOIN users u ON u.tenant_id = m.tenant_id AND u.id = m.user_id
          LEFT JOIN membership_roles mr
            ON mr.tenant_id = m.tenant_id AND mr.membership_id = m.id
          LEFT JOIN roles r
            ON r.tenant_id = mr.tenant_id AND r.id = mr.role_id
          LEFT JOIN role_permissions rp
            ON rp.tenant_id = r.tenant_id AND rp.role_id = r.id
          LEFT JOIN permissions p
            ON p.tenant_id = rp.tenant_id AND p.id = rp.permission_id
         WHERE m.tenant_id = $1 AND m.id = $2
         GROUP BY u.id, m.id, u.status, m.status
        "#,
    )
    .bind(tenant_id)
    .bind(membership_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| NetworkServiceError::Invalid("tenant membership not found".to_owned()))?;
    let account_status: String = row.try_get("account_status")?;
    let membership_status: String = row.try_get("membership_status")?;
    let mut permission_codes: Vec<String> = row.try_get("permission_codes")?;
    if account_status != "active" || membership_status != "active" {
        permission_codes.clear();
    }
    Ok(MembershipPermissionsResponse {
        user_id: row.try_get("user_id")?,
        membership_id: row.try_get("membership_id")?,
        account_status,
        membership_status,
        role_codes: row.try_get("role_codes")?,
        permission_codes,
    })
}

struct IdentityAuditEvent<'a> {
    action: &'a str,
    entity_type: &'a str,
    entity_id: Uuid,
    request_id: &'a str,
    details: serde_json::Value,
}

async fn write_identity_audit(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    session: &super::auth::AuthenticatedSession,
    event: IdentityAuditEvent<'_>,
) -> NetworkResult<()> {
    sqlx::query(
        r#"
        INSERT INTO audit_logs
            (tenant_id, id, actor_id, membership_id, device_id, session_id,
             action, entity_type, entity_id, request_id, result, details_json)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'success', $11)
        "#,
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .bind(session.identity.user_id)
    .bind(session.identity.membership_id)
    .bind(session.device_id)
    .bind(session.session_id)
    .bind(event.action)
    .bind(event.entity_type)
    .bind(event.entity_id)
    .bind(event.request_id)
    .bind(event.details)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn claim_create_user_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    key: &str,
    payload_digest: &str,
    password: &str,
    new_password_hash: &str,
    passwords: &PasswordService,
) -> NetworkResult<Option<CreateTenantUserResponse>> {
    let request_commitment = format!("{payload_digest}:{new_password_hash}");
    let inserted = sqlx::query(
        r#"
        INSERT INTO idempotency_records
            (tenant_id, id, scope, idempotency_key, request_hash, response_json)
        VALUES ($1, $2, $3, $4, $5, '{"state":"in_progress"}'::jsonb)
        ON CONFLICT (tenant_id, scope, idempotency_key) DO NOTHING
        "#,
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .bind(CREATE_USER_SCOPE)
    .bind(key)
    .bind(request_commitment)
    .execute(&mut **transaction)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT request_hash, response_json::text AS response_json FROM idempotency_records WHERE tenant_id = $1 AND scope = $2 AND idempotency_key = $3 FOR UPDATE",
    )
    .bind(tenant_id)
    .bind(CREATE_USER_SCOPE)
    .bind(key)
    .fetch_one(&mut **transaction)
    .await?;
    let stored_commitment: String = row.try_get("request_hash")?;
    let Some((stored_payload_digest, stored_password_hash)) = stored_commitment.split_once(':')
    else {
        return Err(NetworkServiceError::Invalid(
            "create-user idempotency commitment is invalid".to_owned(),
        ));
    };
    let password_matches = passwords
        .verify_password(password, stored_password_hash)
        .map_err(NetworkServiceError::Auth)?;
    if stored_payload_digest != payload_digest || !password_matches {
        return Err(NetworkServiceError::Conflict {
            entity: "idempotency_key".to_owned(),
            key: key.to_owned(),
        });
    }
    let response_json: String = row.try_get("response_json")?;
    let response = serde_json::from_str(&response_json).map_err(|_| {
        NetworkServiceError::Invalid("idempotency record has no committed response".to_owned())
    })?;
    Ok(Some(response))
}

async fn claim_identity_idempotency<T: DeserializeOwned>(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    scope: &str,
    key: &str,
    digest: &str,
) -> NetworkResult<Option<T>> {
    let inserted = sqlx::query(
        r#"
        INSERT INTO idempotency_records
            (tenant_id, id, scope, idempotency_key, request_hash, response_json)
        VALUES ($1, $2, $3, $4, $5, '{"state":"in_progress"}'::jsonb)
        ON CONFLICT (tenant_id, scope, idempotency_key) DO NOTHING
        "#,
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .bind(scope)
    .bind(key)
    .bind(digest)
    .execute(&mut **transaction)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT request_hash, response_json::text AS response_json FROM idempotency_records WHERE tenant_id = $1 AND scope = $2 AND idempotency_key = $3 FOR UPDATE",
    )
    .bind(tenant_id)
    .bind(scope)
    .bind(key)
    .fetch_one(&mut **transaction)
    .await?;
    if row.try_get::<String, _>("request_hash")? != digest {
        return Err(NetworkServiceError::Conflict {
            entity: "idempotency_key".to_owned(),
            key: key.to_owned(),
        });
    }
    let response_json: String = row.try_get("response_json")?;
    let response = serde_json::from_str(&response_json).map_err(|_| {
        NetworkServiceError::Invalid("idempotency record has no committed response".to_owned())
    })?;
    Ok(Some(response))
}

async fn finish_identity_idempotency<T: Serialize>(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    scope: &str,
    key: &str,
    response: &T,
) -> NetworkResult<()> {
    let result = sqlx::query(
        "UPDATE idempotency_records SET response_json = $4::jsonb WHERE tenant_id = $1 AND scope = $2 AND idempotency_key = $3",
    )
    .bind(tenant_id)
    .bind(scope)
    .bind(key)
    .bind(serde_json::to_string(response)?)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(NetworkServiceError::Invalid(
            "idempotency claim disappeared".to_owned(),
        ));
    }
    Ok(())
}

fn identity_request_digest<T: Serialize>(request: &T) -> NetworkResult<String> {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(request)?);
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn identity_conflict_or_sqlx(entity: &str, key: &str, error: sqlx::Error) -> NetworkServiceError {
    if error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| code == "23505")
    {
        NetworkServiceError::Conflict {
            entity: entity.to_owned(),
            key: key.to_owned(),
        }
    } else {
        NetworkServiceError::Sqlx(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_normalizes_login_and_rejects_duplicate_roles() {
        let role_id = Uuid::now_v7();
        let normalized = normalize_create_request(CreateTenantUserRequest {
            request_id: " request-1 ".to_owned(),
            login: " Operator.ONE ".to_owned(),
            display_name: " Operator One ".to_owned(),
            email: Some(" USER@EXAMPLE.COM ".to_owned()),
            password: "long-enough-password".to_owned(),
            role_ids: vec![role_id],
        })
        .expect("normalize valid request");
        assert_eq!(normalized.normalized_login, "operator.one");
        assert_eq!(normalized.email.as_deref(), Some("user@example.com"));

        let duplicate = normalize_replace_roles_request(ReplaceMembershipRolesRequest {
            request_id: "request-2".to_owned(),
            membership_id: Uuid::now_v7(),
            role_ids: vec![role_id, role_id],
        });
        assert!(matches!(duplicate, Err(NetworkServiceError::Invalid(_))));
    }

    #[test]
    fn list_request_enforces_page_bounds_and_normalizes_search() {
        let normalized = normalize_list_request(ListTenantUsersRequest {
            search: Some(" Alice ".to_owned()),
            limit: Some(10),
            ..ListTenantUsersRequest::default()
        })
        .expect("normalize list request");
        assert_eq!(normalized.search.as_deref(), Some("%alice%"));
        assert!(normalize_list_request(ListTenantUsersRequest {
            limit: Some(MAX_PAGE_SIZE + 1),
            ..ListTenantUsersRequest::default()
        })
        .is_err());
    }
}
