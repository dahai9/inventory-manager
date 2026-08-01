use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{ConnectOptions, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone)]
struct LocalSeed {
    workspace_id: String,
    warehouse_id: String,
    receiving_location_id: String,
    storage_location_id: String,
    quarantine_location_id: String,
    shipping_location_id: String,
}

#[derive(Clone)]
pub struct OfflineDatabase {
    pool: SqlitePool,
    seed: LocalSeed,
}

impl OfflineDatabase {
    pub async fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("无法创建离线数据库目录: {err}"))?;
        }

        let url = format!("sqlite://{}", path.to_string_lossy());
        Self::connect(&url, true).await
    }

    async fn connect(url: &str, create_if_missing: bool) -> Result<Self, String> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(|err| format!("离线数据库地址无效: {err}"))?
            .create_if_missing(create_if_missing)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .disable_statement_logging();

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|err| format!("无法打开离线数据库: {err}"))?;

        sqlx::migrate!("./migrations/sqlite")
            .run(&pool)
            .await
            .map_err(|err| format!("无法升级离线数据库: {err}"))?;

        let seed = Self::seed_local_workspace(&pool).await?;
        Ok(Self { pool, seed })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn workspace_id(&self) -> &str {
        &self.seed.workspace_id
    }

    pub fn receiving_location_id(&self) -> &str {
        &self.seed.receiving_location_id
    }

    pub fn warehouse_id(&self) -> &str {
        &self.seed.warehouse_id
    }

    pub fn storage_location_id(&self) -> &str {
        &self.seed.storage_location_id
    }

    pub fn quarantine_location_id(&self) -> &str {
        &self.seed.quarantine_location_id
    }

    pub fn shipping_location_id(&self) -> &str {
        &self.seed.shipping_location_id
    }

    pub async fn is_read_only(&self) -> Result<bool, String> {
        let value: i64 = sqlx::query_scalar("SELECT read_only FROM workspaces WHERE id = ?1")
            .bind(self.workspace_id())
            .fetch_one(self.pool())
            .await
            .map_err(|err| format!("无法读取离线工作区写入状态: {err}"))?;
        Ok(value != 0)
    }

    /// Permanently freezes this offline workspace after a successful one-time
    /// import. The marker is persisted so restarting the app cannot reopen
    /// writes against the archived source.
    pub async fn mark_read_only(&self, export_id: &str, checksum: &str) -> Result<(), String> {
        if export_id.trim().is_empty() || checksum.trim().is_empty() {
            return Err("升级归档缺少 export_id 或 checksum".to_owned());
        }
        let now = now_utc()?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|err| format!("无法开始离线归档事务: {err}"))?;
        sqlx::query("UPDATE workspaces SET read_only = 1 WHERE id = ?1")
            .bind(self.workspace_id())
            .execute(&mut *transaction)
            .await
            .map_err(|err| format!("无法冻结离线工作区: {err}"))?;
        sqlx::query(
            r#"
            INSERT INTO migration_packages
                (id, workspace_id, export_id, direction, schema_version, checksum, status, created_at, imported_at)
            VALUES (?1, ?2, ?3, 'offline_to_network', 1, ?4, 'archived', ?5, ?5)
            ON CONFLICT(export_id) DO UPDATE SET
                checksum = excluded.checksum,
                status = 'archived',
                imported_at = excluded.imported_at
            "#,
        )
        .bind(Uuid::now_v7().to_string())
        .bind(self.workspace_id())
        .bind(export_id.trim())
        .bind(checksum.trim())
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(|err| format!("无法保存离线升级归档记录: {err}"))?;
        transaction
            .commit()
            .await
            .map_err(|err| format!("无法提交离线归档事务: {err}"))
    }

    async fn seed_local_workspace(pool: &SqlitePool) -> Result<LocalSeed, String> {
        if let Some(workspace_id) =
            sqlx::query_scalar::<_, String>("SELECT id FROM workspaces ORDER BY created_at LIMIT 1")
                .fetch_optional(pool)
                .await
                .map_err(|err| format!("无法读取离线工作区: {err}"))?
        {
            return Self::load_local_seed(pool, workspace_id).await;
        }

        let now = now_utc()?;
        let workspace_id = Uuid::now_v7().to_string();
        let source_instance_id = Uuid::now_v7().to_string();
        let warehouse_id = Uuid::now_v7().to_string();
        let receiving_location_id = Uuid::now_v7().to_string();
        let storage_location_id = Uuid::now_v7().to_string();
        let quarantine_location_id = Uuid::now_v7().to_string();
        let shipping_location_id = Uuid::now_v7().to_string();
        let mut transaction = pool
            .begin()
            .await
            .map_err(|err| format!("无法初始化离线数据库事务: {err}"))?;

        sqlx::query(
            r#"
            INSERT INTO workspaces (id, name, timezone, source_instance_id, created_at)
            VALUES (?1, '离线工作区', 'Asia/Shanghai', ?2, ?3)
            "#,
        )
        .bind(&workspace_id)
        .bind(source_instance_id)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(|err| format!("无法初始化离线工作区: {err}"))?;

        sqlx::query(
            r#"
            INSERT INTO warehouses (id, workspace_id, code, name, created_at)
            VALUES (?1, ?2, 'DEFAULT', '默认仓库', ?3)
            "#,
        )
        .bind(&warehouse_id)
        .bind(&workspace_id)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(|err| format!("无法初始化默认仓库: {err}"))?;

        for (id, code, name, kind) in [
            (&receiving_location_id, "RECEIVING", "待检区", "receiving"),
            (&storage_location_id, "STORAGE", "可用库存区", "storage"),
            (
                &quarantine_location_id,
                "QUARANTINE",
                "隔离区",
                "quarantine",
            ),
            (&shipping_location_id, "SHIPPING", "待出库区", "shipping"),
        ] {
            sqlx::query(
                r#"
                INSERT INTO locations
                    (id, workspace_id, warehouse_id, code, name, kind, created_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
            )
            .bind(id)
            .bind(&workspace_id)
            .bind(&warehouse_id)
            .bind(code)
            .bind(name)
            .bind(kind)
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(|err| format!("无法初始化默认库位: {err}"))?;
        }

        transaction
            .commit()
            .await
            .map_err(|err| format!("无法提交离线数据库初始化: {err}"))?;

        Ok(LocalSeed {
            workspace_id,
            warehouse_id,
            receiving_location_id,
            storage_location_id,
            quarantine_location_id,
            shipping_location_id,
        })
    }

    async fn load_local_seed(pool: &SqlitePool, workspace_id: String) -> Result<LocalSeed, String> {
        async fn location_id(
            pool: &SqlitePool,
            workspace_id: &str,
            code: &str,
        ) -> Result<String, String> {
            sqlx::query_scalar("SELECT id FROM locations WHERE workspace_id = ?1 AND code = ?2")
                .bind(workspace_id)
                .bind(code)
                .fetch_one(pool)
                .await
                .map_err(|err| format!("默认库位 {code} 缺失: {err}"))
        }

        let warehouse_id: String = sqlx::query_scalar(
            "SELECT id FROM warehouses WHERE workspace_id = ?1 AND code = 'DEFAULT'",
        )
        .bind(&workspace_id)
        .fetch_one(pool)
        .await
        .map_err(|err| format!("默认仓库缺失: {err}"))?;

        Ok(LocalSeed {
            receiving_location_id: location_id(pool, &workspace_id, "RECEIVING").await?,
            storage_location_id: location_id(pool, &workspace_id, "STORAGE").await?,
            quarantine_location_id: location_id(pool, &workspace_id, "QUARANTINE").await?,
            shipping_location_id: location_id(pool, &workspace_id, "SHIPPING").await?,
            workspace_id,
            warehouse_id,
        })
    }
}

pub(crate) fn now_utc() -> Result<String, String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|err| format!("无法生成 UTC 时间: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_and_local_seed_are_repeatable() {
        let database = OfflineDatabase::connect("sqlite::memory:", false)
            .await
            .expect("open in-memory database");
        let existing_seed = OfflineDatabase::seed_local_workspace(database.pool())
            .await
            .expect("seed can run twice");

        let workspace_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces")
            .fetch_one(database.pool())
            .await
            .expect("count workspaces");
        let location_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM locations")
            .fetch_one(database.pool())
            .await
            .expect("count locations");

        assert_eq!(workspace_count, 1);
        assert_eq!(location_count, 4);
        assert_eq!(existing_seed.workspace_id, database.workspace_id());
        assert!(Uuid::parse_str(database.workspace_id()).is_ok());
    }
}
