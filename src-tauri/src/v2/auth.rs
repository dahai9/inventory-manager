//! Network edition identity, authentication, and authorization.
//!
//! This module deliberately keeps the authentication boundary independent of
//! Tauri commands.  A server adapter can call the transaction functions from
//! an HTTP/gRPC request, while a Tauri adapter can use the same functions when
//! it is configured for the network edition.  The PostgreSQL migration next to
//! this file is the database-side part of the same boundary.
//!
//! The `argon2` dependency is kept explicit in `Cargo.toml`; the authentication
//! code must never silently fall back to a weaker password scheme.

use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::{collections::BTreeSet, fmt};
use thiserror::Error;
use uuid::Uuid;

/// Argon2id parameters used for new credentials.
///
/// The memory cost is expressed in KiB.  These values are deliberately
/// explicit instead of relying on a crate default so a future crate upgrade
/// cannot silently weaken password hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordPolicy {
    pub memory_cost_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
    pub output_len: usize,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            memory_cost_kib: 19 * 1024,
            iterations: 2,
            parallelism: 1,
            output_len: 32,
        }
    }
}

impl PasswordPolicy {
    fn params(self) -> Result<Params, AuthError> {
        Params::new(
            self.memory_cost_kib,
            self.iterations,
            self.parallelism,
            Some(self.output_len),
        )
        .map_err(|error| AuthError::PasswordHash(error.to_string()))
    }
}

/// Argon2id password hasher/verifier.  The dummy hash is used for unknown
/// login names so the miss path still performs an Argon2 operation.
pub struct PasswordService {
    argon2: Argon2<'static>,
    dummy_hash: String,
}

impl PasswordService {
    pub fn new(policy: PasswordPolicy) -> Result<Self, AuthError> {
        let params = policy.params()?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let dummy_hash = Self::hash_with(&argon2, b"inventory-v2-unknown-login")?;
        Ok(Self { argon2, dummy_hash })
    }

    pub fn recommended() -> Result<Self, AuthError> {
        Self::new(PasswordPolicy::default())
    }

    pub fn hash_password(&self, password: &str) -> Result<String, AuthError> {
        if password.is_empty() {
            return Err(AuthError::InvalidInput(
                "password must not be empty".to_owned(),
            ));
        }
        Self::hash_with(&self.argon2, password.as_bytes())
    }

    /// Verify a stored PHC string.  A malformed or non-Argon2id credential is
    /// reported as corruption rather than being accepted as another scheme.
    pub fn verify_password(&self, password: &str, encoded_hash: &str) -> Result<bool, AuthError> {
        let parsed = PasswordHash::new(encoded_hash)
            .map_err(|error| AuthError::CredentialCorrupt(error.to_string()))?;
        if parsed.algorithm.as_str() != "argon2id" {
            return Err(AuthError::CredentialCorrupt(
                "credential is not an Argon2id PHC string".to_owned(),
            ));
        }
        Ok(self
            .argon2
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    }

    fn verify_dummy(&self, password: &str) {
        // The result is intentionally discarded.  This is only a timing
        // equalizer for a login name that does not exist in the tenant.
        let _ = self.verify_password(password, &self.dummy_hash);
    }

    fn hash_with(argon2: &Argon2<'static>, password: &[u8]) -> Result<String, AuthError> {
        let salt = SaltString::generate(&mut OsRng);
        argon2
            .hash_password(password, &salt)
            .map(|hash| hash.to_string())
            .map_err(|error| AuthError::PasswordHash(error.to_string()))
    }
}

/// Lockout settings.  A failure reaches the threshold atomically in the
/// credentials transaction and then locks the account for `lock_seconds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockoutPolicy {
    pub max_failures: u32,
    pub lock_seconds: i64,
}

impl Default for LockoutPolicy {
    fn default() -> Self {
        Self {
            max_failures: 5,
            lock_seconds: 15 * 60,
        }
    }
}

impl LockoutPolicy {
    fn validate(self) -> Result<(), AuthError> {
        if self.max_failures == 0 || self.lock_seconds <= 0 {
            return Err(AuthError::InvalidInput(
                "lockout policy must have positive limits".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockoutState {
    pub failed_attempts: u32,
    pub locked_until_epoch_seconds: Option<i64>,
}

impl LockoutState {
    pub fn is_locked_at(self, now_epoch_seconds: i64) -> bool {
        self.locked_until_epoch_seconds
            .is_some_and(|locked_until| locked_until > now_epoch_seconds)
    }

    pub fn record_failure(
        &mut self,
        now_epoch_seconds: i64,
        policy: LockoutPolicy,
    ) -> Result<FailureOutcome, AuthError> {
        policy.validate()?;
        if self.is_locked_at(now_epoch_seconds) {
            return Ok(FailureOutcome {
                failed_attempts: self.failed_attempts,
                locked: true,
                locked_until_epoch_seconds: self.locked_until_epoch_seconds,
            });
        }

        self.failed_attempts = self.failed_attempts.saturating_add(1);
        if self.failed_attempts >= policy.max_failures {
            self.locked_until_epoch_seconds = Some(now_epoch_seconds + policy.lock_seconds);
        }
        Ok(FailureOutcome {
            failed_attempts: self.failed_attempts,
            locked: self.is_locked_at(now_epoch_seconds),
            locked_until_epoch_seconds: self.locked_until_epoch_seconds,
        })
    }

    pub fn record_success(&mut self) {
        self.failed_attempts = 0;
        self.locked_until_epoch_seconds = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureOutcome {
    pub failed_attempts: u32,
    pub locked: bool,
    pub locked_until_epoch_seconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Active,
    Disabled,
}

impl AccountStatus {
    fn from_database(value: &str) -> Self {
        if value == "active" {
            Self::Active
        } else {
            Self::Disabled
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipStatus {
    Active,
    Inactive,
}

impl MembershipStatus {
    fn from_database(value: &str) -> Self {
        if value == "active" {
            Self::Active
        } else {
            Self::Inactive
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseEntitlementSnapshot {
    pub status: String,
    pub seat_limit: u32,
    pub starts_at_epoch_seconds: i64,
    pub expires_at_epoch_seconds: i64,
    pub revoked: bool,
}

impl LicenseEntitlementSnapshot {
    pub fn is_valid_at(&self, now_epoch_seconds: i64) -> bool {
        self.status == "active"
            && !self.revoked
            && self.starts_at_epoch_seconds <= now_epoch_seconds
            && now_epoch_seconds < self.expires_at_epoch_seconds
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationSnapshot {
    pub tenant_active: bool,
    pub account_status: AccountStatus,
    pub membership_status: MembershipStatus,
    pub active_roles: BTreeSet<String>,
    pub permissions: BTreeSet<String>,
    pub membership_consumes_license_seat: bool,
    pub consumed_license_seats: u32,
    pub license_entitlements: Vec<LicenseEntitlementSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AuthorizationDenial {
    #[error("tenant is inactive")]
    TenantInactive,
    #[error("account is disabled")]
    AccountDisabled,
    #[error("membership is not active")]
    MembershipInactive,
    #[error("membership has no active role")]
    NoActiveRole,
    #[error("tenant has no valid license entitlement")]
    LicenseUnavailable,
    #[error("required permission is missing")]
    MissingPermission,
    #[error("principal does not exist in the tenant")]
    UnknownPrincipal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationDecision {
    Allowed,
    Denied { reason: AuthorizationDenial },
}

impl AuthorizationDecision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

pub fn evaluate_authorization(
    snapshot: &AuthorizationSnapshot,
    required_permission: &str,
    now_epoch_seconds: i64,
) -> AuthorizationDecision {
    if !snapshot.tenant_active {
        return AuthorizationDecision::Denied {
            reason: AuthorizationDenial::TenantInactive,
        };
    }
    if snapshot.account_status != AccountStatus::Active {
        return AuthorizationDecision::Denied {
            reason: AuthorizationDenial::AccountDisabled,
        };
    }
    if snapshot.membership_status != MembershipStatus::Active {
        return AuthorizationDecision::Denied {
            reason: AuthorizationDenial::MembershipInactive,
        };
    }
    if snapshot.active_roles.is_empty() {
        return AuthorizationDecision::Denied {
            reason: AuthorizationDenial::NoActiveRole,
        };
    }
    let seats_are_covered = |entitlement: &LicenseEntitlementSnapshot| {
        !snapshot.membership_consumes_license_seat
            || entitlement.seat_limit >= snapshot.consumed_license_seats
    };
    if !snapshot.license_entitlements.iter().any(|entitlement| {
        entitlement.is_valid_at(now_epoch_seconds) && seats_are_covered(entitlement)
    }) {
        return AuthorizationDecision::Denied {
            reason: AuthorizationDenial::LicenseUnavailable,
        };
    }
    let required_permission = required_permission.trim().to_ascii_lowercase();
    if required_permission.is_empty() || !snapshot.permissions.contains(&required_permission) {
        return AuthorizationDecision::Denied {
            reason: AuthorizationDenial::MissingPermission,
        };
    }
    AuthorizationDecision::Allowed
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedIdentity {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub membership_id: Uuid,
    pub account_status: AccountStatus,
    pub membership_status: MembershipStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedSession {
    pub identity: AuthenticatedIdentity,
    pub session_id: Uuid,
    pub device_id: Uuid,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid authentication input: {0}")]
    InvalidInput(String),
    #[error("password hashing failed: {0}")]
    PasswordHash(String),
    #[error("stored credential is corrupt: {0}")]
    CredentialCorrupt(String),
    #[error("invalid authorization data: {0}")]
    InvalidAuthorizationState(String),
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("account is temporarily locked")]
    LoginLocked,
    #[error("invalid or expired refresh token")]
    InvalidRefreshToken,
    #[error("invalid, expired, or revoked session")]
    InvalidSession,
    #[error("authorization denied: {0}")]
    AccessDenied(AuthorizationDenial),
    #[error("database operation failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Password login with a transactionally committed lockout counter.
///
/// A wrong password updates `credentials` and commits before returning an
/// error.  This is important: returning an error before commit would roll the
/// transaction back and make a brute-force counter ineffective.
pub async fn authenticate_password(
    pool: &PgPool,
    passwords: &PasswordService,
    tenant_id: Uuid,
    normalized_login: &str,
    password: &str,
    policy: LockoutPolicy,
) -> Result<AuthenticatedIdentity, AuthError> {
    policy.validate()?;
    let normalized_login = normalized_login.trim().to_ascii_lowercase();
    if normalized_login.is_empty() || password.is_empty() {
        return Err(AuthError::InvalidCredentials);
    }

    let mut transaction = pool.begin().await?;
    set_tenant_context(&mut transaction, tenant_id).await?;
    let row = sqlx::query(
        r#"
        SELECT u.id AS user_id,
               m.id AS membership_id,
               u.status AS account_status,
               m.status AS membership_status,
               c.password_hash,
               c.locked_until IS NOT NULL
                   AND c.locked_until > CURRENT_TIMESTAMP AS is_locked
          FROM users u
          JOIN credentials c
            ON c.tenant_id = u.tenant_id AND c.user_id = u.id
          JOIN memberships m
            ON m.tenant_id = u.tenant_id AND m.user_id = u.id
         WHERE u.tenant_id = $1 AND u.normalized_login = $2
         FOR UPDATE OF c
        "#,
    )
    .bind(tenant_id)
    .bind(&normalized_login)
    .fetch_optional(&mut *transaction)
    .await?;

    let Some(row) = row else {
        passwords.verify_dummy(password);
        transaction.commit().await?;
        return Err(AuthError::InvalidCredentials);
    };

    let is_locked: bool = row.try_get("is_locked")?;
    if is_locked {
        transaction.commit().await?;
        return Err(AuthError::LoginLocked);
    }

    let password_hash: String = row.try_get("password_hash")?;
    let matches = passwords.verify_password(password, &password_hash)?;
    if !matches {
        let update = sqlx::query(
            r#"
            UPDATE credentials
               SET failed_login_count = failed_login_count + 1,
                   locked_until = CASE
                       WHEN failed_login_count + 1 >= $3
                       THEN CURRENT_TIMESTAMP
                            + make_interval(secs => $4::double precision)
                       ELSE locked_until
                   END,
                   updated_at = CURRENT_TIMESTAMP
             WHERE tenant_id = $1 AND user_id = $2
         RETURNING locked_until IS NOT NULL
                       AND locked_until > CURRENT_TIMESTAMP AS is_locked
            "#,
        )
        .bind(tenant_id)
        .bind(row.try_get::<Uuid, _>("user_id")?)
        .bind(policy.max_failures as i32)
        .bind(policy.lock_seconds)
        .fetch_one(&mut *transaction)
        .await?;
        let locked = update.try_get::<bool, _>("is_locked")?;
        transaction.commit().await?;
        return if locked {
            Err(AuthError::LoginLocked)
        } else {
            Err(AuthError::InvalidCredentials)
        };
    }

    let user_id: Uuid = row.try_get("user_id")?;
    let membership_id: Uuid = row.try_get("membership_id")?;
    let account_status =
        AccountStatus::from_database(row.try_get::<String, _>("account_status")?.as_str());
    let membership_status =
        MembershipStatus::from_database(row.try_get::<String, _>("membership_status")?.as_str());
    sqlx::query(
        r#"
        UPDATE credentials
           SET failed_login_count = 0,
               locked_until = NULL,
               last_authenticated_at = CURRENT_TIMESTAMP,
               updated_at = CURRENT_TIMESTAMP
         WHERE tenant_id = $1 AND user_id = $2
        "#,
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(AuthenticatedIdentity {
        tenant_id,
        user_id,
        membership_id,
        account_status,
        membership_status,
    })
}

/// Check all authorization dimensions in one transaction snapshot.  The
/// caller should invoke this before a business write, using the same
/// transaction as that write, so a role/license revocation cannot race it.
pub async fn authorize_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedIdentity,
    required_permission: &str,
) -> Result<AuthorizationDecision, AuthError> {
    set_principal_context(transaction, identity.tenant_id, Some(identity.user_id)).await?;
    // Lock every authorization input before evaluating it.  The locks keep a
    // concurrent account, role, seat, or entitlement revocation from
    // committing halfway through a business transaction.
    sqlx::query("SELECT id FROM tenants WHERE id = $1 FOR SHARE")
        .bind(identity.tenant_id)
        .fetch_optional(&mut **transaction)
        .await?;
    sqlx::query("SELECT id FROM users WHERE tenant_id = $1 AND id = $2 FOR SHARE")
        .bind(identity.tenant_id)
        .bind(identity.user_id)
        .fetch_optional(&mut **transaction)
        .await?;
    sqlx::query(
        "SELECT id FROM memberships WHERE tenant_id = $1 AND id = $2 AND user_id = $3 FOR SHARE",
    )
    .bind(identity.tenant_id)
    .bind(identity.membership_id)
    .bind(identity.user_id)
    .fetch_optional(&mut **transaction)
    .await?;
    sqlx::query(
        "SELECT id FROM memberships WHERE tenant_id = $1 AND status = 'active' AND consumes_license_seat FOR SHARE",
    )
    .bind(identity.tenant_id)
    .fetch_all(&mut **transaction)
    .await?;
    let core = sqlx::query(
        r#"
        SELECT t.active AS tenant_active,
               u.status AS account_status,
               m.status AS membership_status,
               m.consumes_license_seat AS membership_consumes_license_seat,
               (SELECT count(*)::bigint
                  FROM memberships seat_membership
                 WHERE seat_membership.tenant_id = t.id
                   AND seat_membership.status = 'active'
                   AND seat_membership.consumes_license_seat
               ) AS consumed_license_seats,
               EXTRACT(EPOCH FROM CURRENT_TIMESTAMP)::bigint
                   AS database_now_epoch_seconds
          FROM tenants t
          JOIN users u
            ON u.tenant_id = t.id AND u.id = $2
          JOIN memberships m
            ON m.tenant_id = u.tenant_id
           AND m.id = $3
           AND m.user_id = u.id
         WHERE t.id = $1
        "#,
    )
    .bind(identity.tenant_id)
    .bind(identity.user_id)
    .bind(identity.membership_id)
    .fetch_optional(&mut **transaction)
    .await?;

    let Some(core) = core else {
        return Ok(AuthorizationDecision::Denied {
            reason: AuthorizationDenial::UnknownPrincipal,
        });
    };
    let tenant_active: bool = core.try_get("tenant_active")?;
    let account_status =
        AccountStatus::from_database(core.try_get::<String, _>("account_status")?.as_str());
    let membership_status =
        MembershipStatus::from_database(core.try_get::<String, _>("membership_status")?.as_str());
    let membership_consumes_license_seat: bool =
        core.try_get("membership_consumes_license_seat")?;
    let consumed_license_seats: u32 = core
        .try_get::<i64, _>("consumed_license_seats")?
        .try_into()
        .map_err(|_| {
            AuthError::InvalidAuthorizationState("invalid license seat count".to_owned())
        })?;
    let database_now_epoch_seconds: i64 = core.try_get("database_now_epoch_seconds")?;

    sqlx::query(
        r#"
        SELECT mr.membership_id
          FROM membership_roles mr
          JOIN roles r
            ON r.tenant_id = mr.tenant_id AND r.id = mr.role_id
         WHERE mr.tenant_id = $1
           AND mr.membership_id = $2
        FOR SHARE OF mr, r
        "#,
    )
    .bind(identity.tenant_id)
    .bind(identity.membership_id)
    .fetch_all(&mut **transaction)
    .await?;
    let role_rows = sqlx::query(
        r#"
        SELECT r.code
          FROM membership_roles mr
          JOIN roles r
            ON r.tenant_id = mr.tenant_id AND r.id = mr.role_id
         WHERE mr.tenant_id = $1
           AND mr.membership_id = $2
           AND r.active
        "#,
    )
    .bind(identity.tenant_id)
    .bind(identity.membership_id)
    .fetch_all(&mut **transaction)
    .await?;
    let active_roles = role_rows
        .into_iter()
        .map(|row| row.try_get::<String, _>("code"))
        .collect::<Result<BTreeSet<_>, _>>()?;

    sqlx::query(
        r#"
        SELECT p.id
          FROM membership_roles mr
          JOIN roles r
            ON r.tenant_id = mr.tenant_id AND r.id = mr.role_id AND r.active
          JOIN role_permissions rp
            ON rp.tenant_id = r.tenant_id AND rp.role_id = r.id
          JOIN permissions p
            ON p.tenant_id = rp.tenant_id AND p.id = rp.permission_id
         WHERE mr.tenant_id = $1 AND mr.membership_id = $2
        FOR SHARE OF mr, r, rp, p
        "#,
    )
    .bind(identity.tenant_id)
    .bind(identity.membership_id)
    .fetch_all(&mut **transaction)
    .await?;
    let permission_rows = sqlx::query(
        r#"
        SELECT DISTINCT p.code
          FROM membership_roles mr
          JOIN roles r
            ON r.tenant_id = mr.tenant_id AND r.id = mr.role_id
           AND r.active
          JOIN role_permissions rp
            ON rp.tenant_id = r.tenant_id AND rp.role_id = r.id
          JOIN permissions p
            ON p.tenant_id = rp.tenant_id AND p.id = rp.permission_id
         WHERE mr.tenant_id = $1 AND mr.membership_id = $2
        "#,
    )
    .bind(identity.tenant_id)
    .bind(identity.membership_id)
    .fetch_all(&mut **transaction)
    .await?;
    let permissions = permission_rows
        .into_iter()
        .map(|row| row.try_get::<String, _>("code"))
        .collect::<Result<BTreeSet<_>, _>>()?;

    sqlx::query("SELECT id FROM license_entitlements WHERE tenant_id = $1 FOR SHARE")
        .bind(identity.tenant_id)
        .fetch_all(&mut **transaction)
        .await?;
    let entitlement_rows = sqlx::query(
        r#"
        SELECT status,
               seat_limit,
               EXTRACT(EPOCH FROM starts_at)::bigint AS starts_at_epoch_seconds,
               EXTRACT(EPOCH FROM expires_at)::bigint AS expires_at_epoch_seconds,
               revoked_at IS NOT NULL AS revoked
          FROM license_entitlements
         WHERE tenant_id = $1
           AND verified_at IS NOT NULL
        "#,
    )
    .bind(identity.tenant_id)
    .fetch_all(&mut **transaction)
    .await?;
    let license_entitlements = entitlement_rows
        .into_iter()
        .map(|row| -> Result<LicenseEntitlementSnapshot, AuthError> {
            let seat_limit: u32 =
                row.try_get::<i32, _>("seat_limit")?
                    .try_into()
                    .map_err(|_| {
                        AuthError::InvalidAuthorizationState(
                            "invalid entitlement seat limit".to_owned(),
                        )
                    })?;
            Ok(LicenseEntitlementSnapshot {
                status: row.try_get("status")?,
                seat_limit,
                starts_at_epoch_seconds: row.try_get("starts_at_epoch_seconds")?,
                expires_at_epoch_seconds: row.try_get("expires_at_epoch_seconds")?,
                revoked: row.try_get("revoked")?,
            })
        })
        .collect::<Result<Vec<_>, AuthError>>()?;

    let snapshot = AuthorizationSnapshot {
        tenant_active,
        account_status,
        membership_status,
        active_roles,
        permissions,
        membership_consumes_license_seat,
        consumed_license_seats,
        license_entitlements,
    };
    Ok(evaluate_authorization(
        &snapshot,
        required_permission,
        database_now_epoch_seconds,
    ))
}

/// Convenience form for read-only authorization checks.  Business mutations
/// should use [`authorize_in_transaction`] directly to share their write
/// transaction.
pub async fn authorize(
    pool: &PgPool,
    identity: &AuthenticatedIdentity,
    required_permission: &str,
) -> Result<AuthorizationDecision, AuthError> {
    let mut transaction = pool.begin().await?;
    let decision =
        authorize_in_transaction(&mut transaction, identity, required_permission).await?;
    transaction.commit().await?;
    Ok(decision)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPolicy {
    pub session_ttl_seconds: i64,
    pub refresh_ttl_seconds: i64,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            session_ttl_seconds: 15 * 60,
            refresh_ttl_seconds: 30 * 24 * 60 * 60,
        }
    }
}

impl SessionPolicy {
    fn validate(self) -> Result<(), AuthError> {
        if self.session_ttl_seconds <= 0 || self.refresh_ttl_seconds <= 0 {
            return Err(AuthError::InvalidInput(
                "session TTLs must be positive".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct IssuedSession {
    pub session_id: Uuid,
    pub refresh_token_id: Uuid,
    pub session_token: String,
    pub refresh_token: String,
    pub session_ttl_seconds: i64,
    pub refresh_ttl_seconds: i64,
}

impl fmt::Debug for IssuedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedSession")
            .field("session_id", &self.session_id)
            .field("refresh_token_id", &self.refresh_token_id)
            .field("session_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("session_ttl_seconds", &self.session_ttl_seconds)
            .field("refresh_ttl_seconds", &self.refresh_ttl_seconds)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Session,
    Refresh,
}

impl TokenKind {
    fn domain(self) -> &'static [u8] {
        match self {
            Self::Session => b"inventory-v2/session-token",
            Self::Refresh => b"inventory-v2/refresh-token",
        }
    }
}

/// Hash an opaque bearer token before it is persisted.  The token's domain is
/// included so a refresh token can never be accepted as a session token even
/// if a caller accidentally uses the wrong lookup table.
pub fn hash_token(kind: TokenKind, token: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(kind.domain());
    digest.update([0]);
    digest.update(token.as_bytes());
    digest.finalize().into()
}

/// Resolve an opaque bearer token inside the caller's business transaction.
///
/// The tenant is set before querying the RLS-protected session table, and the
/// actor setting is populated only from the verified session. Business DTOs
/// must never supply either value.
pub async fn authenticate_session_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    session_token: &str,
) -> Result<AuthenticatedSession, AuthError> {
    let session_token = session_token.trim();
    if session_token.is_empty() {
        return Err(AuthError::InvalidSession);
    }
    set_principal_context(transaction, tenant_id, None).await?;
    let token_hash = hash_token(TokenKind::Session, session_token);
    let row = sqlx::query(
        r#"
        SELECT s.id AS session_id,
               s.device_id,
               s.user_id,
               s.membership_id,
               u.status AS account_status,
               m.status AS membership_status,
               d.status AS device_status
          FROM sessions s
          JOIN users u
            ON u.tenant_id = s.tenant_id AND u.id = s.user_id
          JOIN memberships m
            ON m.tenant_id = s.tenant_id
           AND m.id = s.membership_id
           AND m.user_id = s.user_id
          JOIN devices d
            ON d.tenant_id = s.tenant_id
           AND d.id = s.device_id
           AND d.membership_id = s.membership_id
           AND d.user_id = s.user_id
         WHERE s.tenant_id = $1
           AND s.token_hash = $2
           AND s.revoked_at IS NULL
           AND s.expires_at > CURRENT_TIMESTAMP
         FOR SHARE OF s, u, m, d
        "#,
    )
    .bind(tenant_id)
    .bind(token_hash.as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Err(AuthError::InvalidSession);
    };
    if row.try_get::<String, _>("device_status")? != "active" {
        return Err(AuthError::InvalidSession);
    }
    let user_id: Uuid = row.try_get("user_id")?;
    let identity = AuthenticatedIdentity {
        tenant_id,
        user_id,
        membership_id: row.try_get("membership_id")?,
        account_status: AccountStatus::from_database(
            row.try_get::<String, _>("account_status")?.as_str(),
        ),
        membership_status: MembershipStatus::from_database(
            row.try_get::<String, _>("membership_status")?.as_str(),
        ),
    };
    sqlx::query(
        "UPDATE sessions SET last_seen_at = CURRENT_TIMESTAMP WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(row.try_get::<Uuid, _>("session_id")?)
    .execute(&mut **transaction)
    .await?;
    set_principal_context(transaction, tenant_id, Some(user_id)).await?;
    Ok(AuthenticatedSession {
        identity,
        session_id: row.try_get("session_id")?,
        device_id: row.try_get("device_id")?,
    })
}

/// Authenticate and authorize one bearer token without leaving the caller's
/// transaction. Permission names are supplied by server route code, never by
/// the request body.
pub async fn authorize_session_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    session_token: &str,
    required_permission: &str,
) -> Result<AuthenticatedSession, AuthError> {
    let session =
        authenticate_session_in_transaction(transaction, tenant_id, session_token).await?;
    match authorize_in_transaction(transaction, &session.identity, required_permission).await? {
        AuthorizationDecision::Allowed => Ok(session),
        AuthorizationDecision::Denied { reason } => Err(AuthError::AccessDenied(reason)),
    }
}

fn new_token(kind: TokenKind) -> (String, [u8; 32]) {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let digest = hash_token(kind, &token);
    (token, digest)
}

/// Issue one opaque session and one opaque refresh token after checking the
/// complete authorization state in the same transaction.
pub async fn issue_session(
    pool: &PgPool,
    identity: &AuthenticatedIdentity,
    device_id: Uuid,
    required_permission: &str,
    policy: SessionPolicy,
) -> Result<IssuedSession, AuthError> {
    policy.validate()?;
    let mut transaction = pool.begin().await?;
    let decision =
        authorize_in_transaction(&mut transaction, identity, required_permission).await?;
    if let AuthorizationDecision::Denied { reason } = decision {
        transaction.rollback().await?;
        return Err(AuthError::AccessDenied(reason));
    }

    let device_exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
              FROM devices
             WHERE tenant_id = $1
               AND id = $2
               AND membership_id = $3
               AND user_id = $4
               AND status = 'active'
        )
        "#,
    )
    .bind(identity.tenant_id)
    .bind(device_id)
    .bind(identity.membership_id)
    .bind(identity.user_id)
    .fetch_one(&mut *transaction)
    .await?;
    if !device_exists {
        transaction.rollback().await?;
        return Err(AuthError::InvalidInput(
            "device is not registered for this membership".to_owned(),
        ));
    }

    let session_id = Uuid::now_v7();
    let refresh_token_id = Uuid::now_v7();
    let (session_token, session_hash) = new_token(TokenKind::Session);
    let (refresh_token, refresh_hash) = new_token(TokenKind::Refresh);
    sqlx::query(
        r#"
        INSERT INTO sessions
            (tenant_id, id, membership_id, user_id, device_id, token_hash,
             issued_at, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP,
                CURRENT_TIMESTAMP + make_interval(secs => $7::double precision))
        "#,
    )
    .bind(identity.tenant_id)
    .bind(session_id)
    .bind(identity.membership_id)
    .bind(identity.user_id)
    .bind(device_id)
    .bind(session_hash.as_slice())
    .bind(policy.session_ttl_seconds)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO refresh_tokens
            (tenant_id, id, session_id, membership_id, user_id, token_hash,
             issued_at, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP,
                CURRENT_TIMESTAMP + make_interval(secs => $7::double precision))
        "#,
    )
    .bind(identity.tenant_id)
    .bind(refresh_token_id)
    .bind(session_id)
    .bind(identity.membership_id)
    .bind(identity.user_id)
    .bind(refresh_hash.as_slice())
    .bind(policy.refresh_ttl_seconds)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(IssuedSession {
        session_id,
        refresh_token_id,
        session_token,
        refresh_token,
        session_ttl_seconds: policy.session_ttl_seconds,
        refresh_ttl_seconds: policy.refresh_ttl_seconds,
    })
}

/// Rotate a refresh token exactly once and issue a new short-lived session
/// token.  The old refresh row is marked used in the same transaction as the
/// replacement row, so two concurrent refresh requests cannot both succeed.
pub async fn rotate_refresh_token(
    pool: &PgPool,
    tenant_id: Uuid,
    refresh_token: &str,
    required_permission: &str,
    policy: SessionPolicy,
) -> Result<(AuthenticatedIdentity, IssuedSession), AuthError> {
    policy.validate()?;
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(AuthError::InvalidRefreshToken);
    }

    let mut transaction = pool.begin().await?;
    set_tenant_context(&mut transaction, tenant_id).await?;
    let token_hash = hash_token(TokenKind::Refresh, refresh_token);
    let row = sqlx::query(
        r#"
        SELECT rt.id AS refresh_token_id,
               rt.session_id,
               rt.user_id,
               rt.membership_id,
               rt.used_at IS NULL
                   AND rt.revoked_at IS NULL
                   AND rt.expires_at > CURRENT_TIMESTAMP AS token_valid,
               s.device_id,
               s.revoked_at IS NULL
                   AND s.expires_at > CURRENT_TIMESTAMP AS session_valid,
               u.status AS account_status,
               m.status AS membership_status
          FROM refresh_tokens rt
          JOIN sessions s
            ON s.tenant_id = rt.tenant_id AND s.id = rt.session_id
          JOIN users u
            ON u.tenant_id = rt.tenant_id AND u.id = rt.user_id
          JOIN memberships m
            ON m.tenant_id = rt.tenant_id
           AND m.id = rt.membership_id
           AND m.user_id = rt.user_id
          JOIN devices d
            ON d.tenant_id = s.tenant_id
           AND d.id = s.device_id
           AND d.membership_id = s.membership_id
           AND d.user_id = s.user_id
           AND d.status = 'active'
         WHERE rt.tenant_id = $1
           AND rt.token_hash = $2
         FOR UPDATE OF rt, s
        "#,
    )
    .bind(tenant_id)
    .bind(token_hash.as_slice())
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(row) = row else {
        transaction.rollback().await?;
        return Err(AuthError::InvalidRefreshToken);
    };
    if !row.try_get::<bool, _>("token_valid")? || !row.try_get::<bool, _>("session_valid")? {
        transaction.rollback().await?;
        return Err(AuthError::InvalidRefreshToken);
    }

    let identity = AuthenticatedIdentity {
        tenant_id,
        user_id: row.try_get("user_id")?,
        membership_id: row.try_get("membership_id")?,
        account_status: AccountStatus::from_database(
            row.try_get::<String, _>("account_status")?.as_str(),
        ),
        membership_status: MembershipStatus::from_database(
            row.try_get::<String, _>("membership_status")?.as_str(),
        ),
    };
    if let AuthorizationDecision::Denied { reason } =
        authorize_in_transaction(&mut transaction, &identity, required_permission).await?
    {
        transaction.rollback().await?;
        return Err(AuthError::AccessDenied(reason));
    }

    let session_id: Uuid = row.try_get("session_id")?;
    let old_refresh_token_id: Uuid = row.try_get("refresh_token_id")?;
    let new_refresh_token_id = Uuid::now_v7();
    let (session_token, session_hash) = new_token(TokenKind::Session);
    let (refresh_token, refresh_hash) = new_token(TokenKind::Refresh);

    sqlx::query(
        r#"
        INSERT INTO refresh_tokens
            (tenant_id, id, session_id, membership_id, user_id, token_hash,
             issued_at, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP,
                CURRENT_TIMESTAMP + make_interval(secs => $7::double precision))
        "#,
    )
    .bind(tenant_id)
    .bind(new_refresh_token_id)
    .bind(session_id)
    .bind(identity.membership_id)
    .bind(identity.user_id)
    .bind(refresh_hash.as_slice())
    .bind(policy.refresh_ttl_seconds)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        UPDATE refresh_tokens
           SET used_at = CURRENT_TIMESTAMP,
               replaced_by_token_id = $3
         WHERE tenant_id = $1
           AND id = $2
           AND used_at IS NULL
           AND revoked_at IS NULL
        "#,
    )
    .bind(tenant_id)
    .bind(old_refresh_token_id)
    .bind(new_refresh_token_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        UPDATE sessions
           SET token_hash = $3,
               expires_at = CURRENT_TIMESTAMP
                   + make_interval(secs => $4::double precision),
               last_seen_at = CURRENT_TIMESTAMP
         WHERE tenant_id = $1
           AND id = $2
           AND revoked_at IS NULL
        "#,
    )
    .bind(tenant_id)
    .bind(session_id)
    .bind(session_hash.as_slice())
    .bind(policy.session_ttl_seconds)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok((
        identity,
        IssuedSession {
            session_id,
            refresh_token_id: new_refresh_token_id,
            session_token,
            refresh_token,
            session_ttl_seconds: policy.session_ttl_seconds,
            refresh_ttl_seconds: policy.refresh_ttl_seconds,
        },
    ))
}

/// Revoke one session after authenticating its current bearer token.  The
/// operation is deliberately idempotent at the service boundary: a second
/// logout receives the same invalid-session response without changing data.
pub async fn revoke_session(
    pool: &PgPool,
    tenant_id: Uuid,
    session_token: &str,
    reason: &str,
) -> Result<(), AuthError> {
    let mut transaction = pool.begin().await?;
    let session =
        authenticate_session_in_transaction(&mut transaction, tenant_id, session_token).await?;
    sqlx::query(
        "UPDATE sessions SET revoked_at = CURRENT_TIMESTAMP, revoke_reason = $3 WHERE tenant_id = $1 AND id = $2 AND revoked_at IS NULL",
    )
    .bind(tenant_id)
    .bind(session.session_id)
    .bind(if reason.trim().is_empty() {
        "user_logout"
    } else {
        reason.trim()
    })
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = CURRENT_TIMESTAMP WHERE tenant_id = $1 AND session_id = $2 AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(tenant_id)
    .bind(session.session_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn set_tenant_context(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
) -> Result<(), AuthError> {
    set_principal_context(transaction, tenant_id, None).await
}

async fn set_principal_context(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor_user_id: Option<Uuid>,
) -> Result<(), AuthError> {
    sqlx::query(
        "SELECT set_config('app.tenant_id', $1, true), set_config('app.actor_user_id', $2, true)",
    )
    .bind(tenant_id.to_string())
    .bind(actor_user_id.map(|id| id.to_string()).unwrap_or_default())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_never_contains_plaintext() {
        let service = PasswordService::new(PasswordPolicy {
            memory_cost_kib: 8 * 1024,
            iterations: 1,
            parallelism: 1,
            output_len: 16,
        })
        .expect("valid test parameters");
        let plaintext = "correct horse battery staple";
        let encoded = service.hash_password(plaintext).expect("hash succeeds");
        assert!(encoded.starts_with("$argon2id$"));
        assert!(!encoded.contains(plaintext));
        assert!(service.verify_password(plaintext, &encoded).unwrap());
        assert!(!service.verify_password("wrong password", &encoded).unwrap());
    }

    #[test]
    fn failed_attempts_commit_a_lock_at_threshold() {
        let policy = LockoutPolicy {
            max_failures: 3,
            lock_seconds: 60,
        };
        let mut state = LockoutState {
            failed_attempts: 0,
            locked_until_epoch_seconds: None,
        };
        assert!(!state.record_failure(100, policy).unwrap().locked);
        assert!(!state.record_failure(101, policy).unwrap().locked);
        let third = state.record_failure(102, policy).unwrap();
        assert!(third.locked);
        assert_eq!(third.failed_attempts, 3);
        assert!(state.is_locked_at(161));
        assert!(!state.is_locked_at(162));
        state.record_success();
        assert_eq!(state.failed_attempts, 0);
        assert!(!state.is_locked_at(162));
    }

    #[test]
    fn expired_entitlement_is_rejected_by_unified_authorization() {
        let snapshot = AuthorizationSnapshot {
            tenant_active: true,
            account_status: AccountStatus::Active,
            membership_status: MembershipStatus::Active,
            active_roles: ["warehouse_operator".to_owned()].into_iter().collect(),
            permissions: ["inventory.read".to_owned()].into_iter().collect(),
            membership_consumes_license_seat: true,
            consumed_license_seats: 1,
            license_entitlements: vec![LicenseEntitlementSnapshot {
                status: "active".to_owned(),
                seat_limit: 1,
                starts_at_epoch_seconds: 100,
                expires_at_epoch_seconds: 200,
                revoked: false,
            }],
        };
        assert_eq!(
            evaluate_authorization(&snapshot, "inventory.read", 199),
            AuthorizationDecision::Allowed
        );
        assert_eq!(
            evaluate_authorization(&snapshot, "inventory.read", 200),
            AuthorizationDecision::Denied {
                reason: AuthorizationDenial::LicenseUnavailable
            }
        );
    }

    #[test]
    fn token_hash_is_one_way_and_domain_separated() {
        let raw = "test-token";
        assert_ne!(
            hash_token(TokenKind::Session, raw),
            hash_token(TokenKind::Refresh, raw)
        );
        assert_ne!(
            hash_token(TokenKind::Session, raw).as_slice(),
            raw.as_bytes()
        );
    }
}
