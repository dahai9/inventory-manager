use super::upgrade::{NetworkUpgradeImportResponse, NetworkUpgradeImportStatus};
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

        super::voiding::initialize_offline_operation_password(&pool).await?;

        let seed = Self::seed_local_workspace(&pool).await?;
        Ok(Self { pool, seed })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn workspace_id(&self) -> &str {
        &self.seed.workspace_id
    }

    pub async fn source_identity(&self) -> Result<(String, String), String> {
        sqlx::query_as("SELECT id, source_instance_id FROM workspaces WHERE id = ?1")
            .bind(self.workspace_id())
            .fetch_one(self.pool())
            .await
            .map_err(|err| format!("无法读取离线工作区身份: {err}"))
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

    pub async fn verify_upgrade_source(
        &self,
        workspace_id: &str,
        source_instance_id: &str,
    ) -> Result<(), String> {
        let identity: Option<(String, String)> =
            sqlx::query_as("SELECT id, source_instance_id FROM workspaces WHERE id = ?1")
                .bind(self.workspace_id())
                .fetch_optional(self.pool())
                .await
                .map_err(|err| format!("无法验证离线升级来源: {err}"))?;
        match identity {
            Some((current_workspace, current_instance))
                if current_workspace == workspace_id && current_instance == source_instance_id =>
            {
                Ok(())
            }
            _ => Err("升级包不是当前离线工作区生成的，拒绝归档".to_owned()),
        }
    }

    /// Permanently freezes this offline workspace after a successful one-time
    /// import. The marker is persisted so restarting the app cannot reopen
    /// writes against the archived source.
    pub async fn mark_read_only(&self, export_id: &str, checksum: &str) -> Result<(), String> {
        self.archive_workspace(export_id, checksum, None).await
    }

    pub async fn archive_after_network_import(
        &self,
        response: &NetworkUpgradeImportResponse,
        target_workspace_id: Uuid,
    ) -> Result<(), String> {
        let import_status = match response.status {
            NetworkUpgradeImportStatus::Imported => "imported",
            NetworkUpgradeImportStatus::AlreadyImported => "already_imported",
        };
        let entity_counts_json = serde_json::to_string(&response.entity_counts)
            .map_err(|err| format!("无法编码升级结果计数: {err}"))?;
        let details = ArchiveDetails {
            migration_id: &response.migration_id,
            target_workspace_id,
            import_status,
            entity_counts_json: &entity_counts_json,
            server_imported_at: response.imported_at.as_deref(),
        };
        self.archive_workspace(&response.export_id, &response.checksum, Some(details))
            .await
    }

    async fn archive_workspace(
        &self,
        export_id: &str,
        checksum: &str,
        details: Option<ArchiveDetails<'_>>,
    ) -> Result<(), String> {
        if export_id.trim().is_empty() || checksum.trim().is_empty() {
            return Err("升级归档缺少 export_id 或 checksum".to_owned());
        }
        let checksum = checksum.trim();
        if checksum.len() != 64
            || !checksum
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err("升级归档 checksum 必须是小写 SHA-256".to_owned());
        }
        let now = now_utc()?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|err| format!("无法开始离线归档事务: {err}"))?;
        let existing: Option<(String, String)> =
            sqlx::query_as("SELECT checksum, status FROM migration_packages WHERE export_id = ?1")
                .bind(export_id.trim())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|err| format!("无法检查离线升级归档记录: {err}"))?;
        if let Some((existing_checksum, status)) = existing {
            if existing_checksum != checksum {
                return Err(format!(
                    "export_id 已归档为不同 checksum，拒绝覆盖: {status}"
                ));
            }
        }
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
        .bind(checksum)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(|err| format!("无法保存离线升级归档记录: {err}"))?;

        if let Some(details) = details {
            if details.migration_id.trim().is_empty() {
                return Err("升级结果缺少 migration_id".to_owned());
            }
            let result = sqlx::query(
                r#"
                INSERT INTO migration_result_reports
                    (export_id, workspace_id, migration_id, target_workspace_id,
                     checksum, import_status, entity_counts_json,
                     server_imported_at, archived_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(export_id) DO UPDATE SET
                    import_status = excluded.import_status,
                    entity_counts_json = excluded.entity_counts_json,
                    server_imported_at = COALESCE(
                        excluded.server_imported_at,
                        migration_result_reports.server_imported_at
                    )
                WHERE migration_result_reports.migration_id = excluded.migration_id
                  AND migration_result_reports.target_workspace_id = excluded.target_workspace_id
                  AND migration_result_reports.checksum = excluded.checksum
                "#,
            )
            .bind(export_id.trim())
            .bind(self.workspace_id())
            .bind(details.migration_id.trim())
            .bind(details.target_workspace_id.to_string())
            .bind(checksum)
            .bind(details.import_status)
            .bind(details.entity_counts_json)
            .bind(details.server_imported_at)
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(|err| format!("无法保存升级结果报告: {err}"))?;
            if result.rows_affected() != 1 {
                return Err("已有升级结果与服务器响应不一致，拒绝覆盖".to_owned());
            }
        }
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

struct ArchiveDetails<'a> {
    migration_id: &'a str,
    target_workspace_id: Uuid,
    import_status: &'a str,
    entity_counts_json: &'a str,
    server_imported_at: Option<&'a str>,
}

pub(crate) fn now_utc() -> Result<String, String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|err| format!("无法生成 UTC 时间: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Connection;
    use std::collections::BTreeMap;

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

    #[tokio::test]
    async fn repeat_shipment_migration_preserves_existing_foreign_keys() {
        let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:")
            .await
            .expect("open migration test database");
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0001_inventory_v2_core.sql"
        ))
        .execute(&mut connection)
        .await
        .expect("apply original schema");
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0002_upgrade_result_reports.sql"
        ))
        .execute(&mut connection)
        .await
        .expect("apply upgrade report schema");
        sqlx::raw_sql(
            r#"
            INSERT INTO workspaces (id, name, source_instance_id, created_at)
            VALUES ('w', 'Workspace', 'instance', '2026-08-01T00:00:00Z');
            INSERT INTO business_parties (id, workspace_id, normalized_name, display_name, created_at)
            VALUES ('owner', 'w', 'owner', 'Owner', '2026-08-01T00:00:00Z'),
                   ('receiver', 'w', 'receiver', 'Receiver', '2026-08-01T00:00:00Z');
            INSERT INTO skus (id, workspace_id, code, name, created_at)
            VALUES ('sku', 'w', 'SKU', 'Model', '2026-08-01T00:00:00Z');
            INSERT INTO warehouses (id, workspace_id, code, name, created_at)
            VALUES ('warehouse', 'w', 'WH', 'Warehouse', '2026-08-01T00:00:00Z');
            INSERT INTO locations (id, workspace_id, warehouse_id, code, name, kind, created_at)
            VALUES ('location', 'w', 'warehouse', 'STORAGE', 'Storage', 'storage', '2026-08-01T00:00:00Z');
            INSERT INTO inbound_receipts
                (id, workspace_id, receipt_no, owner_party_id, warehouse_id, received_at,
                 status, actor_id, idempotency_key, request_id, created_at)
            VALUES ('receipt', 'w', 'R-1', 'owner', 'warehouse', '2026-08-01T00:00:00Z',
                    'posted', 'actor', 'receipt-key', 'receipt-request', '2026-08-01T00:00:00Z');
            INSERT INTO inbound_receipt_lines
                (id, workspace_id, receipt_id, sku_id, declared_quantity, scanned_quantity, created_at)
            VALUES ('receipt-line', 'w', 'receipt', 'sku', 1, 1, '2026-08-01T00:00:00Z');
            INSERT INTO inventory_units
                (id, workspace_id, barcode, inbound_receipt_line_id, owner_party_id, sku_id,
                 location_id, inventory_status, quality_status, version, received_at, updated_at)
            VALUES ('unit', 'w', 'SN-1', 'receipt-line', 'owner', 'sku', 'location',
                    'delivered', 'passed', 1, '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z');
            INSERT INTO outbound_orders
                (id, workspace_id, order_no, upstream_receiver_id, status, actor_id,
                 idempotency_key, request_id, created_at)
            VALUES ('order', 'w', 'O-1', 'receiver', 'completed', 'actor',
                    'order-key', 'order-request', '2026-08-01T00:00:00Z');
            INSERT INTO outbound_order_lines
                (id, workspace_id, outbound_order_id, sku_id, required_quantity,
                 allocated_quantity, shipped_quantity, delivered_quantity, created_at)
            VALUES ('order-line', 'w', 'order', 'sku', 1, 1, 1, 1, '2026-08-01T00:00:00Z');
            INSERT INTO outbound_allocations
                (id, workspace_id, outbound_order_line_id, inventory_unit_id, status,
                 allocated_by, allocated_at)
            VALUES ('allocation', 'w', 'order-line', 'unit', 'shipped', 'actor', '2026-08-01T00:00:00Z');
            INSERT INTO outbound_shipments
                (id, workspace_id, shipment_no, outbound_order_id, status, shipped_at,
                 actor_id, idempotency_key, request_id, created_at)
            VALUES ('shipment', 'w', 'S-1', 'order', 'delivered', '2026-08-01T00:00:00Z',
                    'actor', 'shipment-key', 'shipment-request', '2026-08-01T00:00:00Z');
            INSERT INTO outbound_shipment_lines
                (id, workspace_id, outbound_shipment_id, outbound_allocation_id,
                 inventory_unit_id, scanned_barcode_snapshot, created_at)
            VALUES ('shipment-line', 'w', 'shipment', 'allocation', 'unit', 'SN-1', '2026-08-01T00:00:00Z');
            INSERT INTO delivery_confirmations
                (id, workspace_id, outbound_shipment_id, confirmation_code, confirmed_by,
                 confirmed_at, idempotency_key, request_id, created_at)
            VALUES ('confirmation', 'w', 'shipment', 'C-1', 'receiver', '2026-08-01T00:00:00Z',
                    'confirmation-key', 'confirmation-request', '2026-08-01T00:00:00Z');
            INSERT INTO delivery_confirmation_lines
                (id, workspace_id, delivery_confirmation_id, outbound_shipment_line_id,
                 result, created_at)
            VALUES ('confirmation-line', 'w', 'confirmation', 'shipment-line',
                    'accepted', '2026-08-01T00:00:00Z');
            "#,
        )
        .execute(&mut connection)
        .await
        .expect("seed shipment history");

        let mut migration = connection
            .begin()
            .await
            .expect("begin repeat shipment migration");
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0003_repeat_outbound_after_return.sql"
        ))
        .execute(&mut *migration)
        .await
        .expect("apply repeat shipment migration");
        migration
            .commit()
            .await
            .expect("commit repeat shipment migration");

        let foreign_key_errors = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut connection)
            .await
            .expect("check foreign keys");
        assert!(foreign_key_errors.is_empty());
        let retained_confirmation: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM delivery_confirmation_lines WHERE outbound_shipment_line_id = 'shipment-line'",
        )
        .fetch_one(&mut connection)
        .await
        .expect("read retained confirmation");
        assert_eq!(retained_confirmation, 1);
    }

    #[tokio::test]
    async fn network_confirmation_archives_idempotently_and_rejects_checksum_changes() {
        let database = OfflineDatabase::connect("sqlite::memory:", false)
            .await
            .expect("open in-memory database");
        let source_instance_id: String =
            sqlx::query_scalar("SELECT source_instance_id FROM workspaces WHERE id = ?1")
                .bind(database.workspace_id())
                .fetch_one(database.pool())
                .await
                .expect("read source instance");
        database
            .verify_upgrade_source(database.workspace_id(), &source_instance_id)
            .await
            .expect("matching package source");
        assert!(database
            .verify_upgrade_source(database.workspace_id(), &Uuid::now_v7().to_string())
            .await
            .is_err());

        let export_id = Uuid::now_v7().to_string();
        let migration_id = format!("invpack_{}", "1".repeat(64));
        let checksum = "a".repeat(64);
        let target_workspace_id = Uuid::now_v7();
        let response = NetworkUpgradeImportResponse {
            status: NetworkUpgradeImportStatus::Imported,
            export_id: export_id.clone(),
            migration_id: migration_id.clone(),
            checksum: checksum.clone(),
            imported_at: None,
            entity_counts: BTreeMap::from([("inventory_units".to_owned(), 3)]),
        };
        database
            .archive_after_network_import(&response, target_workspace_id)
            .await
            .expect("archive confirmed import");
        assert!(database.is_read_only().await.expect("read archive flag"));
        let report: (String, String, String, String) = sqlx::query_as(
            "SELECT migration_id, target_workspace_id, checksum, entity_counts_json FROM migration_result_reports WHERE export_id = ?1",
        )
        .bind(&export_id)
        .fetch_one(database.pool())
        .await
        .expect("read result report");
        assert_eq!(report.0, migration_id);
        assert_eq!(report.1, target_workspace_id.to_string());
        assert_eq!(report.2, checksum);
        assert_eq!(report.3, r#"{"inventory_units":3}"#);

        let mut replay = response;
        replay.status = NetworkUpgradeImportStatus::AlreadyImported;
        replay.imported_at = Some("2026-08-04 00:00:00+00".to_owned());
        database
            .archive_after_network_import(&replay, target_workspace_id)
            .await
            .expect("archive replay is idempotent");
        let report_count: i64 = sqlx::query_scalar("SELECT count(*) FROM migration_result_reports")
            .fetch_one(database.pool())
            .await
            .expect("count reports");
        assert_eq!(report_count, 1);
        assert!(database
            .mark_read_only(&export_id, &"b".repeat(64))
            .await
            .is_err());
    }
}
