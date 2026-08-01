//! PostgreSQL storage boundary for the network edition.
//!
//! The network edition keeps the same logical model as the offline SQLite
//! edition, but every business transaction is explicitly bound to a tenant.
//! `begin_tenant` sets the tenant and actor settings locally on the checked-out
//! connection before any application query can run.  The SQL migration adds
//! `FORCE ROW LEVEL SECURITY`, so a missing or stale context cannot broaden a
//! query's visibility.

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

/// Connection settings for the network PostgreSQL service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkDatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout: Duration,
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
            max_connections: 16,
            min_connections: 1,
            acquire_timeout: Duration::from_secs(5),
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
}

/// A pool that is safe to share between network requests.
#[derive(Clone, Debug)]
pub struct NetworkDatabase {
    pool: PgPool,
}

impl NetworkDatabase {
    /// Connect to PostgreSQL and apply immutable SQLx migrations.
    pub async fn connect(config: &NetworkDatabaseConfig) -> Result<Self, NetworkDatabaseError> {
        config.validate()?;
        let pool = PgPoolOptions::new()
            .min_connections(config.min_connections)
            .max_connections(config.max_connections)
            .acquire_timeout(config.acquire_timeout)
            .connect(&config.url)
            .await?;

        sqlx::migrate!("./migrations/postgres").run(&pool).await?;

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Begin a transaction with an RLS tenant context.
    ///
    /// The settings use `is_local = true`, so they disappear when this
    /// transaction commits or rolls back.  Callers must use the returned
    /// transaction for all reads and writes in one application operation.
    pub async fn begin_tenant(
        &self,
        tenant_id: Uuid,
        actor_user_id: Option<Uuid>,
    ) -> Result<TenantTransaction<'_>, NetworkDatabaseError> {
        let mut transaction = self.pool.begin().await?;
        let actor_value = actor_user_id.map(|id| id.to_string()).unwrap_or_default();

        sqlx::query(
            "SELECT set_config('app.tenant_id', $1, true), set_config('app.actor_user_id', $2, true)",
        )
        .bind(tenant_id.to_string())
        .bind(actor_value)
        .execute(&mut *transaction)
        .await?;

        let active =
            sqlx::query_scalar::<_, bool>("SELECT active FROM tenants WHERE id = $1 FOR SHARE")
                .bind(tenant_id)
                .fetch_optional(&mut *transaction)
                .await?;
        if active != Some(true) {
            transaction.rollback().await?;
            return Err(NetworkDatabaseError::TenantUnavailable(tenant_id));
        }

        Ok(TenantTransaction {
            transaction,
            tenant_id,
            actor_user_id,
        })
    }
}

/// A transaction carrying the tenant and actor identity used by RLS and
/// audit repositories.  It intentionally exposes the SQLx transaction only
/// through a mutable reference, making it difficult to accidentally use the
/// pool (and lose the tenant context) in the middle of an operation.
pub struct TenantTransaction<'pool> {
    transaction: Transaction<'pool, Postgres>,
    tenant_id: Uuid,
    actor_user_id: Option<Uuid>,
}

impl<'pool> TenantTransaction<'pool> {
    pub fn tenant_id(&self) -> Uuid {
        self.tenant_id
    }

    pub fn actor_user_id(&self) -> Option<Uuid> {
        self.actor_user_id
    }

    /// Borrow the underlying transaction for repository queries.
    ///
    /// Repository methods should accept this borrow instead of acquiring a
    /// second pool connection.  All SQL issued through it remains covered by
    /// the transaction-local RLS settings.
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
