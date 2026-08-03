//! PostgreSQL storage boundary for the network edition.
//!
//! The network edition keeps the same logical model as the offline SQLite
//! edition, but every business transaction is explicitly bound to a tenant and
//! a verified bearer session. The SQL migrations add `FORCE ROW LEVEL
//! SECURITY`, so a missing or stale context cannot broaden a query's
//! visibility.

use super::auth::{authorize_session_in_transaction, AuthError, AuthenticatedSession};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::fmt;
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

/// Connection settings for the network PostgreSQL service.
#[derive(Clone, PartialEq, Eq)]
pub struct NetworkDatabaseConfig {
    pub url: String,
    /// Optional privileged connection used only for immutable migrations.
    /// The runtime URL must use a restricted non-owner role.
    pub migration_url: Option<String>,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout: Duration,
    pub require_restricted_role: bool,
}

impl fmt::Debug for NetworkDatabaseConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetworkDatabaseConfig")
            .field("url", &"<redacted>")
            .field(
                "migration_url",
                &self.migration_url.as_ref().map(|_| "<redacted>"),
            )
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("require_restricted_role", &self.require_restricted_role)
            .finish()
    }
}

impl NetworkDatabaseConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), NetworkDatabaseError> {
        if self.url.trim().is_empty() {
            return Err(NetworkDatabaseError::InvalidConfig(
                "PostgreSQL URL must not be empty".to_owned(),
            ));
        }
        if self
            .migration_url
            .as_ref()
            .is_some_and(|url| url.trim().is_empty())
        {
            return Err(NetworkDatabaseError::InvalidConfig(
                "migration_url must not be empty when configured".to_owned(),
            ));
        }
        if self.min_connections > self.max_connections {
            return Err(NetworkDatabaseError::InvalidConfig(
                "min_connections must not exceed max_connections".to_owned(),
            ));
        }
        if self.max_connections == 0 {
            return Err(NetworkDatabaseError::InvalidConfig(
                "max_connections must be greater than zero".to_owned(),
            ));
        }
        if self.acquire_timeout.is_zero() {
            return Err(NetworkDatabaseError::InvalidConfig(
                "acquire_timeout must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for NetworkDatabaseConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            migration_url: None,
            max_connections: 16,
            min_connections: 1,
            acquire_timeout: Duration::from_secs(5),
            require_restricted_role: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum NetworkDatabaseError {
    #[error("invalid PostgreSQL configuration: {0}")]
    InvalidConfig(String),
    #[error("PostgreSQL operation failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("PostgreSQL migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("tenant {0} does not exist or is inactive")]
    TenantUnavailable(Uuid),
    #[error("runtime PostgreSQL role is unsafe: {0}")]
    InsecureRuntimeRole(String),
    #[error("network authorization failed: {0}")]
    Authorization(#[from] AuthError),
}

/// A pool that is safe to share between network requests.
#[derive(Clone, Debug)]
pub struct NetworkDatabase {
    pool: PgPool,
}

impl NetworkDatabase {
    /// Apply migrations through the optional privileged connection, then open
    /// the runtime pool using a restricted role.
    pub async fn connect(config: &NetworkDatabaseConfig) -> Result<Self, NetworkDatabaseError> {
        config.validate()?;
        if let Some(migration_url) = &config.migration_url {
            let migration_pool = PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(config.acquire_timeout)
                .connect(migration_url)
                .await?;
            sqlx::migrate!("./migrations/postgres")
                .run(&migration_pool)
                .await?;
            migration_pool.close().await;
        }
        let pool = PgPoolOptions::new()
            .min_connections(config.min_connections)
            .max_connections(config.max_connections)
            .acquire_timeout(config.acquire_timeout)
            .connect(&config.url)
            .await?;

        if config.require_restricted_role {
            validate_runtime_role(&pool).await?;
        }

        Ok(Self { pool })
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Begin one authenticated and authorized business request. Tenant,
    /// session, actor, permission and entitlement are all resolved before the
    /// transaction is exposed to a repository.
    pub async fn begin_authorized_request(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        required_permission: &str,
    ) -> Result<AuthorizedTransaction<'_>, NetworkDatabaseError> {
        let mut transaction = self.pool.begin().await?;
        let session = authorize_session_in_transaction(
            &mut transaction,
            tenant_id,
            session_token,
            required_permission,
        )
        .await?;
        Ok(AuthorizedTransaction {
            transaction,
            session,
        })
    }
}

async fn validate_runtime_role(pool: &PgPool) -> Result<(), NetworkDatabaseError> {
    let row = sqlx::query(
        r#"
        SELECT current_user AS role_name,
               r.rolsuper,
               r.rolbypassrls,
               EXISTS (
                   SELECT 1
                     FROM pg_class c
                     JOIN pg_namespace n ON n.oid = c.relnamespace
                    WHERE n.nspname = 'public'
                      AND c.relkind IN ('r', 'p')
                      AND c.relowner = r.oid
               ) AS owns_public_tables
          FROM pg_roles r
         WHERE r.rolname = current_user
        "#,
    )
    .fetch_one(pool)
    .await?;
    let role_name: String = row.try_get("role_name")?;
    let superuser: bool = row.try_get("rolsuper")?;
    let bypass_rls: bool = row.try_get("rolbypassrls")?;
    let owns_public_tables: bool = row.try_get("owns_public_tables")?;
    if superuser || bypass_rls || owns_public_tables {
        return Err(NetworkDatabaseError::InsecureRuntimeRole(format!(
            "role {role_name} must be NOSUPERUSER, NOBYPASSRLS, and not own public tables"
        )));
    }
    Ok(())
}

pub struct AuthorizedTransaction<'pool> {
    transaction: Transaction<'pool, Postgres>,
    session: AuthenticatedSession,
}

impl<'pool> AuthorizedTransaction<'pool> {
    pub fn session(&self) -> &AuthenticatedSession {
        &self.session
    }

    pub fn sqlx_transaction(&mut self) -> &mut Transaction<'pool, Postgres> {
        &mut self.transaction
    }

    pub async fn commit(self) -> Result<(), NetworkDatabaseError> {
        self.transaction.commit().await.map_err(Into::into)
    }

    pub async fn rollback(self) -> Result<(), NetworkDatabaseError> {
        self.transaction.rollback().await.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_is_conservative() {
        let config = NetworkDatabaseConfig::default();
        assert_eq!(config.max_connections, 16);
        assert_eq!(config.min_connections, 1);
        assert_eq!(config.acquire_timeout, Duration::from_secs(5));
        assert!(config.validate().is_err());
    }

    #[test]
    fn configuration_rejects_invalid_pool_bounds() {
        let mut config = NetworkDatabaseConfig::new("postgres://localhost/inventory");
        config.min_connections = 3;
        config.max_connections = 2;
        assert!(matches!(
            config.validate(),
            Err(NetworkDatabaseError::InvalidConfig(message)) if message.contains("min_connections")
        ));
    }

    #[test]
    fn configuration_accepts_a_valid_url() {
        let config = NetworkDatabaseConfig::new("postgres://localhost/inventory");
        assert!(config.validate().is_ok());
    }
}
