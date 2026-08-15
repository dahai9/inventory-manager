use super::auth::{reauthenticate_user_in_transaction, PasswordService};
use super::network::{
    NetworkResult, NetworkService, NetworkServiceError, PERMISSION_INVENTORY_READ,
};
use super::sqlite::OfflineDatabase;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Sqlite, Transaction};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

pub const PERMISSION_DOCUMENT_VOID: &str = "inventory.document.void";
const RECEIPT_SCOPE: &str = "void_inbound_receipt";
const OUTBOUND_SCOPE: &str = "void_outbound_order";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VoidDocumentRequest {
    pub document_id: String,
    pub reason: String,
    pub password: String,
    pub actor_id: Option<String>,
    pub request_id: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VoidDocumentResponse {
    pub document_id: String,
    pub document_no: String,
    pub document_kind: String,
    pub status: String,
    pub voided_at: String,
    pub voided_inventory_count: u32,
    pub released_inventory_count: u32,
    pub quarantined_inventory_count: u32,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChangeOperationPasswordRequest {
    pub current_password: String,
    pub new_password: String,
    pub actor_id: String,
    pub request_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CopyDocumentSnRequest {
    pub document_id: String,
    pub password: String,
    pub actor_id: Option<String>,
    pub request_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CopyDocumentSnResponse {
    pub document_id: String,
    pub document_no: String,
    pub document_kind: String,
    pub barcodes: Vec<String>,
}

pub(crate) async fn initialize_offline_operation_password(
    pool: &sqlx::SqlitePool,
) -> Result<(), String> {
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_credentials WHERE id = 1")
        .fetch_one(pool)
        .await
        .map_err(|error| format!("无法读取离线操作密码: {error}"))?;
    if exists != 0 {
        return Ok(());
    }
    let passwords = PasswordService::recommended().map_err(|error| error.to_string())?;
    let hash = passwords
        .hash_password("admin")
        .map_err(|error| error.to_string())?;
    sqlx::query(
        "INSERT OR IGNORE INTO operation_credentials (id, password_hash, updated_at) VALUES (1, ?1, ?2)",
    )
    .bind(hash)
    .bind(now_utc()?)
    .execute(pool)
    .await
    .map_err(|error| format!("无法初始化离线操作密码: {error}"))?;
    Ok(())
}

impl OfflineDatabase {
    pub async fn copy_receipt_document_sns(
        &self,
        request: CopyDocumentSnRequest,
    ) -> Result<CopyDocumentSnResponse, String> {
        let request = normalize_copy_request(request)?;
        let actor_id = request.actor_id.as_deref().unwrap_or("operation");
        let passwords = PasswordService::recommended().map_err(|error| error.to_string())?;
        let now = now_utc()?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| format!("无法开始复制收货单 SN 授权事务: {error}"))?;
        verify_offline_password(&mut transaction, &passwords, &request.password).await?;
        let receipt = sqlx::query(
            "SELECT receipt_no FROM inbound_receipts WHERE workspace_id = ?1 AND id = ?2",
        )
        .bind(self.workspace_id())
        .bind(&request.document_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| format!("无法读取收货单: {error}"))?
        .ok_or_else(|| format!("找不到收货单 {}", request.document_id))?;
        let document_no: String = receipt
            .try_get("receipt_no")
            .map_err(|error| error.to_string())?;
        let barcodes: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT iu.barcode
              FROM inbound_receipt_lines rl
              JOIN inventory_units iu ON iu.inbound_receipt_line_id = rl.id
             WHERE rl.workspace_id = ?1 AND rl.receipt_id = ?2
             ORDER BY iu.barcode
            "#,
        )
        .bind(self.workspace_id())
        .bind(&request.document_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| format!("无法读取收货单 SN: {error}"))?;
        let response = CopyDocumentSnResponse {
            document_id: request.document_id.clone(),
            document_no,
            document_kind: "inbound_receipt".to_owned(),
            barcodes,
        };
        insert_sqlite_audit(
            &mut transaction,
            self.workspace_id(),
            actor_id,
            "inbound_receipt.sns_copied",
            "inbound_receipt",
            &request.document_id,
            &request.request_id,
            json!({"barcode_count": response.barcodes.len()}),
            &now,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|error| format!("无法完成收货单 SN 授权: {error}"))?;
        Ok(response)
    }

    pub async fn copy_outbound_order_document_sns(
        &self,
        request: CopyDocumentSnRequest,
    ) -> Result<CopyDocumentSnResponse, String> {
        let request = normalize_copy_request(request)?;
        let actor_id = request.actor_id.as_deref().unwrap_or("operation");
        let passwords = PasswordService::recommended().map_err(|error| error.to_string())?;
        let now = now_utc()?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| format!("无法开始复制出库单 SN 授权事务: {error}"))?;
        verify_offline_password(&mut transaction, &passwords, &request.password).await?;
        let order =
            sqlx::query("SELECT order_no FROM outbound_orders WHERE workspace_id = ?1 AND id = ?2")
                .bind(self.workspace_id())
                .bind(&request.document_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|error| format!("无法读取出库订单: {error}"))?
                .ok_or_else(|| format!("找不到出库订单 {}", request.document_id))?;
        let document_no: String = order
            .try_get("order_no")
            .map_err(|error| error.to_string())?;
        let barcodes: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT DISTINCT iu.barcode
              FROM outbound_order_lines ol
              JOIN outbound_allocations oa ON oa.outbound_order_line_id = ol.id
              JOIN inventory_units iu ON iu.id = oa.inventory_unit_id
             WHERE ol.workspace_id = ?1 AND ol.outbound_order_id = ?2
             ORDER BY iu.barcode
            "#,
        )
        .bind(self.workspace_id())
        .bind(&request.document_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| format!("无法读取出库单 SN: {error}"))?;
        let response = CopyDocumentSnResponse {
            document_id: request.document_id.clone(),
            document_no,
            document_kind: "outbound_order".to_owned(),
            barcodes,
        };
        insert_sqlite_audit(
            &mut transaction,
            self.workspace_id(),
            actor_id,
            "outbound_order.sns_copied",
            "outbound_order",
            &request.document_id,
            &request.request_id,
            json!({"barcode_count": response.barcodes.len()}),
            &now,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|error| format!("无法完成出库单 SN 授权: {error}"))?;
        Ok(response)
    }

    pub async fn change_operation_password(
        &self,
        request: ChangeOperationPasswordRequest,
    ) -> Result<(), String> {
        let current = request.current_password;
        let new_password = request.new_password;
        if !(5..=128).contains(&new_password.as_bytes().len()) {
            return Err("新密码长度必须为 5 到 128 个字节".to_owned());
        }
        if request.actor_id.trim().is_empty() || request.request_id.trim().is_empty() {
            return Err("修改密码缺少操作者或请求编号".to_owned());
        }
        let passwords = PasswordService::recommended().map_err(|error| error.to_string())?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| format!("无法开始密码修改事务: {error}"))?;
        verify_offline_password(&mut transaction, &passwords, &current).await?;
        let hash = passwords
            .hash_password(&new_password)
            .map_err(|error| error.to_string())?;
        let now = now_utc()?;
        sqlx::query(
            "UPDATE operation_credentials SET password_hash = ?1, updated_at = ?2 WHERE id = 1",
        )
        .bind(hash)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(|error| format!("无法保存新操作密码: {error}"))?;
        sqlx::query("INSERT INTO audit_logs (id, workspace_id, actor_id, action, entity_type, entity_id, request_id, result, details_json, occurred_at) VALUES (?1, ?2, ?3, 'operation_password.changed', 'workspace', ?2, ?4, 'success', '{}', ?5)")
            .bind(new_id())
            .bind(self.workspace_id())
            .bind(request.actor_id.trim())
            .bind(request.request_id.trim())
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(|error| format!("无法记录密码修改审计: {error}"))?;
        transaction
            .commit()
            .await
            .map_err(|error| format!("无法提交密码修改: {error}"))
    }

    pub async fn void_receipt_document(
        &self,
        request: VoidDocumentRequest,
    ) -> Result<VoidDocumentResponse, String> {
        let request = normalize_request(request)?;
        let actor_id = request
            .actor_id
            .as_deref()
            .ok_or_else(|| "离线作废缺少操作者".to_owned())?;
        let passwords = PasswordService::recommended().map_err(|error| error.to_string())?;
        let now = now_utc()?;
        let digest = request_digest(&request, actor_id)?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| format!("无法开始收货单作废事务: {error}"))?;
        verify_offline_password(&mut transaction, &passwords, &request.password).await?;
        if let Some(mut replay) = load_sqlite_replay(
            &mut transaction,
            self.workspace_id(),
            RECEIPT_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            transaction
                .commit()
                .await
                .map_err(|error| error.to_string())?;
            return Ok(replay);
        }

        let receipt = sqlx::query(
            "SELECT receipt_no, status FROM inbound_receipts WHERE workspace_id = ?1 AND id = ?2",
        )
        .bind(self.workspace_id())
        .bind(&request.document_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| format!("无法读取收货单: {error}"))?
        .ok_or_else(|| format!("找不到收货单 {}", request.document_id))?;
        let receipt_no: String = receipt
            .try_get("receipt_no")
            .map_err(|error| error.to_string())?;
        let status: String = receipt
            .try_get("status")
            .map_err(|error| error.to_string())?;
        if status != "posted" {
            return Err(format!("收货单 {receipt_no} 当前状态为 {status}，不能作废"));
        }
        let units = sqlx::query(
            r#"
            SELECT iu.id, iu.barcode, iu.inventory_status, iu.location_id,
                   EXISTS (
                       SELECT 1 FROM outbound_allocations oa
                        WHERE oa.workspace_id = iu.workspace_id
                          AND oa.inventory_unit_id = iu.id
                   ) AS has_outbound
              FROM inbound_receipt_lines rl
              JOIN inventory_units iu ON iu.inbound_receipt_line_id = rl.id
             WHERE rl.workspace_id = ?1 AND rl.receipt_id = ?2
             ORDER BY iu.barcode
            "#,
        )
        .bind(self.workspace_id())
        .bind(&request.document_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| format!("无法检查收货单商品: {error}"))?;
        let mut blockers = Vec::new();
        for row in &units {
            let barcode: String = row.try_get("barcode").map_err(|error| error.to_string())?;
            let unit_status: String = row
                .try_get("inventory_status")
                .map_err(|error| error.to_string())?;
            let has_outbound: bool = row
                .try_get("has_outbound")
                .map_err(|error| error.to_string())?;
            if has_outbound
                || !matches!(
                    unit_status.as_str(),
                    "received" | "available" | "quarantined"
                )
            {
                blockers.push(format!(
                    "{barcode}（{unit_status}{}）",
                    if has_outbound {
                        "，已有出库关联"
                    } else {
                        ""
                    }
                ));
            }
        }
        if !blockers.is_empty() {
            return Err(format!("收货单存在不能作废的商品：{}", blockers.join("、")));
        }

        let void_id = new_id();
        insert_sqlite_document_void(
            &mut transaction,
            self.workspace_id(),
            &void_id,
            "inbound_receipt",
            Some(&request.document_id),
            None,
            &request.reason,
            actor_id,
            &now,
            &request.request_id,
            &request.idempotency_key,
        )
        .await?;
        let updated = sqlx::query("UPDATE inbound_receipts SET status = 'voided' WHERE workspace_id = ?1 AND id = ?2 AND status = 'posted'")
            .bind(self.workspace_id())
            .bind(&request.document_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| format!("无法作废收货单: {error}"))?;
        if updated.rows_affected() != 1 {
            return Err("收货单状态已发生变化，请刷新后重试".to_owned());
        }
        for row in &units {
            let unit_id: String = row.try_get("id").map_err(|error| error.to_string())?;
            let location_id: String = row
                .try_get("location_id")
                .map_err(|error| error.to_string())?;
            let changed = sqlx::query("UPDATE inventory_units SET inventory_status = 'voided', version = version + 1, updated_at = ?1 WHERE workspace_id = ?2 AND id = ?3 AND inventory_status IN ('received','available','quarantined')")
                .bind(&now)
                .bind(self.workspace_id())
                .bind(&unit_id)
                .execute(&mut *transaction)
                .await
                .map_err(|error| format!("无法作废收货商品: {error}"))?;
            if changed.rows_affected() != 1 {
                return Err("收货商品状态已发生变化，请刷新后重试".to_owned());
            }
            insert_sqlite_movement(
                &mut transaction,
                self.workspace_id(),
                &unit_id,
                "voided",
                Some(&location_id),
                Some(&location_id),
                &void_id,
                actor_id,
                &now,
            )
            .await?;
        }
        let response = VoidDocumentResponse {
            document_id: request.document_id.clone(),
            document_no: receipt_no,
            document_kind: "inbound_receipt".to_owned(),
            status: "voided".to_owned(),
            voided_at: now.clone(),
            voided_inventory_count: units.len() as u32,
            released_inventory_count: 0,
            quarantined_inventory_count: 0,
            idempotent_replay: false,
        };
        insert_sqlite_audit(&mut transaction, self.workspace_id(), actor_id, "inbound_receipt.voided", "inbound_receipt", &request.document_id, &request.request_id, json!({"void_id": void_id, "reason": request.reason, "voided_inventory_count": response.voided_inventory_count}), &now).await?;
        save_sqlite_replay(
            &mut transaction,
            self.workspace_id(),
            RECEIPT_SCOPE,
            &request.idempotency_key,
            &digest,
            &response,
            &now,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|error| format!("无法提交收货单作废: {error}"))?;
        Ok(response)
    }

    pub async fn void_outbound_order_document(
        &self,
        request: VoidDocumentRequest,
    ) -> Result<VoidDocumentResponse, String> {
        let request = normalize_request(request)?;
        let actor_id = request
            .actor_id
            .as_deref()
            .ok_or_else(|| "离线作废缺少操作者".to_owned())?;
        let passwords = PasswordService::recommended().map_err(|error| error.to_string())?;
        let now = now_utc()?;
        let digest = request_digest(&request, actor_id)?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| format!("无法开始出库单作废事务: {error}"))?;
        verify_offline_password(&mut transaction, &passwords, &request.password).await?;
        if let Some(mut replay) = load_sqlite_replay(
            &mut transaction,
            self.workspace_id(),
            OUTBOUND_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            transaction
                .commit()
                .await
                .map_err(|error| error.to_string())?;
            return Ok(replay);
        }
        let order = sqlx::query(
            "SELECT order_no, status FROM outbound_orders WHERE workspace_id = ?1 AND id = ?2",
        )
        .bind(self.workspace_id())
        .bind(&request.document_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| format!("无法读取出库订单: {error}"))?
        .ok_or_else(|| format!("找不到出库订单 {}", request.document_id))?;
        let order_no: String = order
            .try_get("order_no")
            .map_err(|error| error.to_string())?;
        let status: String = order.try_get("status").map_err(|error| error.to_string())?;
        if status == "voided" {
            return Err(format!("出库订单 {order_no} 已经作废"));
        }
        let blockers: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT sl.scanned_barcode_snapshot
              FROM outbound_shipments ship
              JOIN outbound_shipment_lines sl ON sl.outbound_shipment_id = ship.id
              LEFT JOIN outbound_return_lines ret ON ret.outbound_shipment_line_id = sl.id
             WHERE ship.workspace_id = ?1 AND ship.outbound_order_id = ?2 AND ret.id IS NULL
             ORDER BY sl.scanned_barcode_snapshot
            "#,
        )
        .bind(self.workspace_id())
        .bind(&request.document_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| format!("无法检查未退回商品: {error}"))?;
        if !blockers.is_empty() {
            return Err(format!(
                "仍有 {} 件已出库商品未退回，不能作废：{}",
                blockers.len(),
                blockers.join("、")
            ));
        }
        let allocations = sqlx::query(
            r#"
            SELECT oa.id AS allocation_id, iu.id AS unit_id, iu.barcode,
                   iu.inventory_status, iu.location_id,
                   (SELECT sm.from_location_id FROM stock_movements sm
                     WHERE sm.workspace_id = iu.workspace_id
                       AND sm.inventory_unit_id = iu.id
                       AND sm.movement_type = 'reserved'
                       AND sm.source_type = 'outbound_order_line'
                       AND sm.source_id = ol.id
                     ORDER BY sm.occurred_at DESC, sm.id DESC LIMIT 1) AS restore_location_id
              FROM outbound_order_lines ol
              JOIN outbound_allocations oa ON oa.outbound_order_line_id = ol.id AND oa.status = 'active'
              JOIN inventory_units iu ON iu.id = oa.inventory_unit_id
             WHERE ol.workspace_id = ?1 AND ol.outbound_order_id = ?2
             ORDER BY iu.barcode
            "#,
        ).bind(self.workspace_id()).bind(&request.document_id).fetch_all(&mut *transaction).await
            .map_err(|error| format!("无法读取待释放预留: {error}"))?;
        for row in &allocations {
            let barcode: String = row.try_get("barcode").map_err(|error| error.to_string())?;
            let unit_status: String = row
                .try_get("inventory_status")
                .map_err(|error| error.to_string())?;
            let restore: Option<String> = row
                .try_get("restore_location_id")
                .map_err(|error| error.to_string())?;
            if unit_status != "reserved" || restore.is_none() {
                return Err(format!("SN {barcode} 的预留状态或原库位不一致，不能作废"));
            }
        }
        let quarantined_count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(DISTINCT ret.inventory_unit_id)
                  FROM outbound_order_lines ol
                  JOIN outbound_allocations oa ON oa.outbound_order_line_id = ol.id
                  JOIN outbound_shipment_lines sl ON sl.outbound_allocation_id = oa.id
                  JOIN outbound_return_lines ret ON ret.outbound_shipment_line_id = sl.id
                 WHERE ol.workspace_id = ?1 AND ol.outbound_order_id = ?2"#,
        )
        .bind(self.workspace_id())
        .bind(&request.document_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| format!("无法统计已退回商品: {error}"))?;
        let void_id = new_id();
        insert_sqlite_document_void(
            &mut transaction,
            self.workspace_id(),
            &void_id,
            "outbound_order",
            None,
            Some(&request.document_id),
            &request.reason,
            actor_id,
            &now,
            &request.request_id,
            &request.idempotency_key,
        )
        .await?;
        for row in &allocations {
            let allocation_id: String = row
                .try_get("allocation_id")
                .map_err(|error| error.to_string())?;
            let unit_id: String = row.try_get("unit_id").map_err(|error| error.to_string())?;
            let current_location: String = row
                .try_get("location_id")
                .map_err(|error| error.to_string())?;
            let restore_location: String = row
                .try_get("restore_location_id")
                .map_err(|error| error.to_string())?;
            let changed = sqlx::query("UPDATE outbound_allocations SET status = 'voided', released_at = ?1 WHERE workspace_id = ?2 AND id = ?3 AND status = 'active'")
                .bind(&now).bind(self.workspace_id()).bind(&allocation_id).execute(&mut *transaction).await
                .map_err(|error| format!("无法作废出库分配: {error}"))?;
            if changed.rows_affected() != 1 {
                return Err("出库分配状态已发生变化，请刷新后重试".to_owned());
            }
            let changed = sqlx::query("UPDATE inventory_units SET inventory_status = 'available', location_id = ?1, version = version + 1, updated_at = ?2 WHERE workspace_id = ?3 AND id = ?4 AND inventory_status = 'reserved'")
                .bind(&restore_location).bind(&now).bind(self.workspace_id()).bind(&unit_id).execute(&mut *transaction).await
                .map_err(|error| format!("无法释放预留商品: {error}"))?;
            if changed.rows_affected() != 1 {
                return Err("预留商品状态已发生变化，请刷新后重试".to_owned());
            }
            insert_sqlite_movement(
                &mut transaction,
                self.workspace_id(),
                &unit_id,
                "reservation_released",
                Some(&current_location),
                Some(&restore_location),
                &void_id,
                actor_id,
                &now,
            )
            .await?;
        }
        sqlx::query("UPDATE outbound_shipments SET status = 'voided' WHERE workspace_id = ?1 AND outbound_order_id = ?2 AND status <> 'voided'")
            .bind(self.workspace_id()).bind(&request.document_id).execute(&mut *transaction).await
            .map_err(|error| format!("无法作废出库批次: {error}"))?;
        let changed = sqlx::query("UPDATE outbound_orders SET status = 'voided' WHERE workspace_id = ?1 AND id = ?2 AND status <> 'voided'")
            .bind(self.workspace_id()).bind(&request.document_id).execute(&mut *transaction).await
            .map_err(|error| format!("无法作废出库订单: {error}"))?;
        if changed.rows_affected() != 1 {
            return Err("出库订单状态已发生变化，请刷新后重试".to_owned());
        }
        let response = VoidDocumentResponse {
            document_id: request.document_id.clone(),
            document_no: order_no,
            document_kind: "outbound_order".to_owned(),
            status: "voided".to_owned(),
            voided_at: now.clone(),
            voided_inventory_count: 0,
            released_inventory_count: allocations.len() as u32,
            quarantined_inventory_count: u32::try_from(quarantined_count).unwrap_or_default(),
            idempotent_replay: false,
        };
        insert_sqlite_audit(&mut transaction, self.workspace_id(), actor_id, "outbound_order.voided", "outbound_order", &request.document_id, &request.request_id, json!({"void_id": void_id, "reason": request.reason, "released_inventory_count": response.released_inventory_count, "quarantined_inventory_count": response.quarantined_inventory_count}), &now).await?;
        save_sqlite_replay(
            &mut transaction,
            self.workspace_id(),
            OUTBOUND_SCOPE,
            &request.idempotency_key,
            &digest,
            &response,
            &now,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|error| format!("无法提交出库单作废: {error}"))?;
        Ok(response)
    }
}

impl NetworkService {
    pub async fn copy_receipt_document_sns_network(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: CopyDocumentSnRequest,
    ) -> NetworkResult<CopyDocumentSnResponse> {
        let request = normalize_network_copy_request(request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_INVENTORY_READ)
            .await?;
        let actor_id = authorized.session().identity.user_id;
        let membership_id = authorized.session().identity.membership_id;
        let device_id = authorized.session().device_id;
        let session_id = authorized.session().session_id;
        let auth_result = reauthenticate_user_in_transaction(
            authorized.sqlx_transaction(),
            self.password_service(),
            tenant_id,
            actor_id,
            &request.password,
            super::auth::LockoutPolicy::default(),
        )
        .await;
        if let Err(error) = auth_result {
            authorized.commit().await?;
            return Err(NetworkServiceError::Auth(error));
        }
        let now = now_utc().map_err(NetworkServiceError::Invalid)?;
        let transaction = authorized.sqlx_transaction();
        let document_id = Uuid::parse_str(&request.document_id)
            .map_err(|_| NetworkServiceError::Invalid("invalid receipt id".to_owned()))?;
        let receipt =
            sqlx::query("SELECT receipt_no FROM inbound_receipts WHERE tenant_id=$1 AND id=$2")
                .bind(tenant_id)
                .bind(document_id)
                .fetch_optional(&mut **transaction)
                .await?
                .ok_or_else(|| {
                    NetworkServiceError::Invalid(format!("unknown receipt {document_id}"))
                })?;
        let document_no: String = receipt.try_get("receipt_no")?;
        let barcodes: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT iu.barcode
              FROM inbound_receipt_lines rl
              JOIN inventory_units iu ON iu.tenant_id=rl.tenant_id
                                     AND iu.inbound_receipt_line_id=rl.id
             WHERE rl.tenant_id=$1 AND rl.receipt_id=$2
             ORDER BY iu.barcode
            "#,
        )
        .bind(tenant_id)
        .bind(document_id)
        .fetch_all(&mut **transaction)
        .await?;
        let response = CopyDocumentSnResponse {
            document_id: document_id.to_string(),
            document_no,
            document_kind: "inbound_receipt".to_owned(),
            barcodes,
        };
        insert_postgres_audit(
            transaction,
            tenant_id,
            actor_id,
            membership_id,
            device_id,
            session_id,
            "inbound_receipt.sns_copied",
            "inbound_receipt",
            document_id,
            &request.request_id,
            json!({"barcode_count": response.barcodes.len()}),
            &now,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn copy_outbound_order_document_sns_network(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: CopyDocumentSnRequest,
    ) -> NetworkResult<CopyDocumentSnResponse> {
        let request = normalize_network_copy_request(request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_INVENTORY_READ)
            .await?;
        let actor_id = authorized.session().identity.user_id;
        let membership_id = authorized.session().identity.membership_id;
        let device_id = authorized.session().device_id;
        let session_id = authorized.session().session_id;
        let auth_result = reauthenticate_user_in_transaction(
            authorized.sqlx_transaction(),
            self.password_service(),
            tenant_id,
            actor_id,
            &request.password,
            super::auth::LockoutPolicy::default(),
        )
        .await;
        if let Err(error) = auth_result {
            authorized.commit().await?;
            return Err(NetworkServiceError::Auth(error));
        }
        let now = now_utc().map_err(NetworkServiceError::Invalid)?;
        let transaction = authorized.sqlx_transaction();
        let document_id = Uuid::parse_str(&request.document_id)
            .map_err(|_| NetworkServiceError::Invalid("invalid outbound order id".to_owned()))?;
        let order =
            sqlx::query("SELECT order_no FROM outbound_orders WHERE tenant_id=$1 AND id=$2")
                .bind(tenant_id)
                .bind(document_id)
                .fetch_optional(&mut **transaction)
                .await?
                .ok_or_else(|| {
                    NetworkServiceError::Invalid(format!("unknown outbound order {document_id}"))
                })?;
        let document_no: String = order.try_get("order_no")?;
        let barcodes: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT DISTINCT iu.barcode
              FROM outbound_order_lines ol
              JOIN outbound_allocations oa ON oa.tenant_id=ol.tenant_id
                                           AND oa.outbound_order_line_id=ol.id
              JOIN inventory_units iu ON iu.tenant_id=oa.tenant_id
                                     AND iu.id=oa.inventory_unit_id
             WHERE ol.tenant_id=$1 AND ol.outbound_order_id=$2
             ORDER BY iu.barcode
            "#,
        )
        .bind(tenant_id)
        .bind(document_id)
        .fetch_all(&mut **transaction)
        .await?;
        let response = CopyDocumentSnResponse {
            document_id: document_id.to_string(),
            document_no,
            document_kind: "outbound_order".to_owned(),
            barcodes,
        };
        insert_postgres_audit(
            transaction,
            tenant_id,
            actor_id,
            membership_id,
            device_id,
            session_id,
            "outbound_order.sns_copied",
            "outbound_order",
            document_id,
            &request.request_id,
            json!({"barcode_count": response.barcodes.len()}),
            &now,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn void_receipt_document_network(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: VoidDocumentRequest,
    ) -> NetworkResult<VoidDocumentResponse> {
        let request = normalize_network_request(request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_DOCUMENT_VOID)
            .await?;
        let actor_id = authorized.session().identity.user_id;
        let membership_id = authorized.session().identity.membership_id;
        let device_id = authorized.session().device_id;
        let session_id = authorized.session().session_id;
        let auth_result = reauthenticate_user_in_transaction(
            authorized.sqlx_transaction(),
            self.password_service(),
            tenant_id,
            actor_id,
            &request.password,
            super::auth::LockoutPolicy::default(),
        )
        .await;
        if let Err(error) = auth_result {
            authorized.commit().await?;
            return Err(NetworkServiceError::Auth(error));
        }
        let digest = request_digest(&request, &actor_id.to_string())
            .map_err(NetworkServiceError::Invalid)?;
        let now = now_utc().map_err(NetworkServiceError::Invalid)?;
        let transaction = authorized.sqlx_transaction();
        if let Some(mut replay) = load_postgres_replay(
            transaction,
            tenant_id,
            RECEIPT_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            authorized.commit().await?;
            return Ok(replay);
        }
        let document_id = Uuid::parse_str(&request.document_id)
            .map_err(|_| NetworkServiceError::Invalid("invalid receipt id".to_owned()))?;
        let receipt = sqlx::query("SELECT receipt_no, status FROM inbound_receipts WHERE tenant_id = $1 AND id = $2 FOR UPDATE")
            .bind(tenant_id).bind(document_id).fetch_optional(&mut **transaction).await?
            .ok_or_else(|| NetworkServiceError::Invalid(format!("unknown receipt {document_id}")))?;
        let receipt_no: String = receipt.try_get("receipt_no")?;
        let status: String = receipt.try_get("status")?;
        if status != "posted" {
            return Err(NetworkServiceError::Conflict {
                entity: "inbound_receipt_status".to_owned(),
                key: status,
            });
        }
        let units = sqlx::query(r#"SELECT iu.id, iu.barcode, iu.inventory_status, iu.location_id,
                   EXISTS (SELECT 1 FROM outbound_allocations oa WHERE oa.tenant_id=iu.tenant_id AND oa.inventory_unit_id=iu.id) AS has_outbound
              FROM inbound_receipt_lines rl JOIN inventory_units iu ON iu.tenant_id=rl.tenant_id AND iu.inbound_receipt_line_id=rl.id
             WHERE rl.tenant_id=$1 AND rl.receipt_id=$2 ORDER BY iu.barcode FOR UPDATE OF iu"#)
            .bind(tenant_id).bind(document_id).fetch_all(&mut **transaction).await?;
        for row in &units {
            let unit_status: String = row.try_get("inventory_status")?;
            if row.try_get::<bool, _>("has_outbound")?
                || !matches!(
                    unit_status.as_str(),
                    "received" | "available" | "quarantined"
                )
            {
                return Err(NetworkServiceError::Conflict {
                    entity: "inbound_receipt_inventory".to_owned(),
                    key: row.try_get::<String, _>("barcode")?,
                });
            }
        }
        let void_id = Uuid::now_v7();
        insert_postgres_document_void(
            transaction,
            tenant_id,
            void_id,
            "inbound_receipt",
            Some(document_id),
            None,
            &request.reason,
            actor_id,
            &now,
            &request.request_id,
            &request.idempotency_key,
        )
        .await?;
        let changed = sqlx::query("UPDATE inbound_receipts SET status='voided' WHERE tenant_id=$1 AND id=$2 AND status='posted'").bind(tenant_id).bind(document_id).execute(&mut **transaction).await?;
        if changed.rows_affected() != 1 {
            return Err(NetworkServiceError::Conflict {
                entity: "inbound_receipt".to_owned(),
                key: document_id.to_string(),
            });
        }
        for row in &units {
            let unit_id: Uuid = row.try_get("id")?;
            let location_id: Uuid = row.try_get("location_id")?;
            let changed = sqlx::query("UPDATE inventory_units SET inventory_status='voided', version=version+1, updated_at=CURRENT_TIMESTAMP WHERE tenant_id=$1 AND id=$2 AND inventory_status IN ('received','available','quarantined')").bind(tenant_id).bind(unit_id).execute(&mut **transaction).await?;
            if changed.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "inventory_unit".to_owned(),
                    key: unit_id.to_string(),
                });
            }
            insert_postgres_movement(
                transaction,
                tenant_id,
                unit_id,
                "voided",
                Some(location_id),
                Some(location_id),
                void_id,
                actor_id,
                &now,
            )
            .await?;
        }
        let response = VoidDocumentResponse {
            document_id: document_id.to_string(),
            document_no: receipt_no,
            document_kind: "inbound_receipt".to_owned(),
            status: "voided".to_owned(),
            voided_at: now.clone(),
            voided_inventory_count: units.len() as u32,
            released_inventory_count: 0,
            quarantined_inventory_count: 0,
            idempotent_replay: false,
        };
        insert_postgres_audit(transaction, tenant_id, actor_id, membership_id, device_id, session_id, "inbound_receipt.voided", "inbound_receipt", document_id, &request.request_id, json!({"void_id":void_id,"reason":request.reason,"voided_inventory_count":response.voided_inventory_count}), &now).await?;
        save_postgres_replay(
            transaction,
            tenant_id,
            RECEIPT_SCOPE,
            &request.idempotency_key,
            &digest,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn void_outbound_order_document_network(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: VoidDocumentRequest,
    ) -> NetworkResult<VoidDocumentResponse> {
        let request = normalize_network_request(request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_DOCUMENT_VOID)
            .await?;
        let actor_id = authorized.session().identity.user_id;
        let membership_id = authorized.session().identity.membership_id;
        let device_id = authorized.session().device_id;
        let session_id = authorized.session().session_id;
        let auth_result = reauthenticate_user_in_transaction(
            authorized.sqlx_transaction(),
            self.password_service(),
            tenant_id,
            actor_id,
            &request.password,
            super::auth::LockoutPolicy::default(),
        )
        .await;
        if let Err(error) = auth_result {
            authorized.commit().await?;
            return Err(NetworkServiceError::Auth(error));
        }
        let digest = request_digest(&request, &actor_id.to_string())
            .map_err(NetworkServiceError::Invalid)?;
        let now = now_utc().map_err(NetworkServiceError::Invalid)?;
        let transaction = authorized.sqlx_transaction();
        if let Some(mut replay) = load_postgres_replay(
            transaction,
            tenant_id,
            OUTBOUND_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            authorized.commit().await?;
            return Ok(replay);
        }
        let document_id = Uuid::parse_str(&request.document_id)
            .map_err(|_| NetworkServiceError::Invalid("invalid outbound order id".to_owned()))?;
        let order = sqlx::query(
            "SELECT order_no,status FROM outbound_orders WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
        )
        .bind(tenant_id)
        .bind(document_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| {
            NetworkServiceError::Invalid(format!("unknown outbound order {document_id}"))
        })?;
        let order_no: String = order.try_get("order_no")?;
        let status: String = order.try_get("status")?;
        if status == "voided" {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_order_status".to_owned(),
                key: status,
            });
        }
        let blockers:Vec<String>=sqlx::query_scalar(r#"SELECT sl.scanned_barcode_snapshot FROM outbound_shipments ship JOIN outbound_shipment_lines sl ON sl.tenant_id=ship.tenant_id AND sl.outbound_shipment_id=ship.id LEFT JOIN outbound_return_lines ret ON ret.tenant_id=sl.tenant_id AND ret.outbound_shipment_line_id=sl.id WHERE ship.tenant_id=$1 AND ship.outbound_order_id=$2 AND ret.id IS NULL ORDER BY sl.scanned_barcode_snapshot"#).bind(tenant_id).bind(document_id).fetch_all(&mut **transaction).await?;
        if !blockers.is_empty() {
            return Err(NetworkServiceError::Conflict {
                entity: "unreturned_outbound_inventory".to_owned(),
                key: blockers.join(","),
            });
        }
        let allocations=sqlx::query(r#"SELECT oa.id AS allocation_id,iu.id AS unit_id,iu.barcode,iu.inventory_status,iu.location_id,
                   (SELECT sm.from_location_id FROM stock_movements sm WHERE sm.tenant_id=iu.tenant_id AND sm.inventory_unit_id=iu.id AND sm.movement_type='reserved' AND sm.source_type='outbound_order_line' AND sm.source_id=ol.id ORDER BY sm.occurred_at DESC,sm.id DESC LIMIT 1) AS restore_location_id
              FROM outbound_order_lines ol JOIN outbound_allocations oa ON oa.tenant_id=ol.tenant_id AND oa.outbound_order_line_id=ol.id AND oa.status='active' JOIN inventory_units iu ON iu.tenant_id=oa.tenant_id AND iu.id=oa.inventory_unit_id
             WHERE ol.tenant_id=$1 AND ol.outbound_order_id=$2 ORDER BY iu.barcode FOR UPDATE OF oa,iu"#).bind(tenant_id).bind(document_id).fetch_all(&mut **transaction).await?;
        for row in &allocations {
            let unit_status: String = row.try_get("inventory_status")?;
            let restore: Option<Uuid> = row.try_get("restore_location_id")?;
            if unit_status != "reserved" || restore.is_none() {
                return Err(NetworkServiceError::Conflict {
                    entity: "reserved_inventory".to_owned(),
                    key: row.try_get::<String, _>("barcode")?,
                });
            }
        }
        let quarantined_count:i64=sqlx::query_scalar(r#"SELECT COUNT(DISTINCT ret.inventory_unit_id) FROM outbound_order_lines ol JOIN outbound_allocations oa ON oa.tenant_id=ol.tenant_id AND oa.outbound_order_line_id=ol.id JOIN outbound_shipment_lines sl ON sl.tenant_id=oa.tenant_id AND sl.outbound_allocation_id=oa.id JOIN outbound_return_lines ret ON ret.tenant_id=sl.tenant_id AND ret.outbound_shipment_line_id=sl.id WHERE ol.tenant_id=$1 AND ol.outbound_order_id=$2"#).bind(tenant_id).bind(document_id).fetch_one(&mut **transaction).await?;
        let void_id = Uuid::now_v7();
        insert_postgres_document_void(
            transaction,
            tenant_id,
            void_id,
            "outbound_order",
            None,
            Some(document_id),
            &request.reason,
            actor_id,
            &now,
            &request.request_id,
            &request.idempotency_key,
        )
        .await?;
        for row in &allocations {
            let allocation_id: Uuid = row.try_get("allocation_id")?;
            let unit_id: Uuid = row.try_get("unit_id")?;
            let current: Uuid = row.try_get("location_id")?;
            let restore: Uuid = row.try_get("restore_location_id")?;
            let changed=sqlx::query("UPDATE outbound_allocations SET status='voided',released_at=CURRENT_TIMESTAMP WHERE tenant_id=$1 AND id=$2 AND status='active'").bind(tenant_id).bind(allocation_id).execute(&mut **transaction).await?;
            if changed.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "outbound_allocation".to_owned(),
                    key: allocation_id.to_string(),
                });
            }
            let changed=sqlx::query("UPDATE inventory_units SET inventory_status='available',location_id=$1,version=version+1,updated_at=CURRENT_TIMESTAMP WHERE tenant_id=$2 AND id=$3 AND inventory_status='reserved'").bind(restore).bind(tenant_id).bind(unit_id).execute(&mut **transaction).await?;
            if changed.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "inventory_unit".to_owned(),
                    key: unit_id.to_string(),
                });
            }
            insert_postgres_movement(
                transaction,
                tenant_id,
                unit_id,
                "reservation_released",
                Some(current),
                Some(restore),
                void_id,
                actor_id,
                &now,
            )
            .await?;
        }
        sqlx::query("UPDATE outbound_shipments SET status='voided' WHERE tenant_id=$1 AND outbound_order_id=$2 AND status<>'voided'").bind(tenant_id).bind(document_id).execute(&mut **transaction).await?;
        let changed=sqlx::query("UPDATE outbound_orders SET status='voided' WHERE tenant_id=$1 AND id=$2 AND status<>'voided'").bind(tenant_id).bind(document_id).execute(&mut **transaction).await?;
        if changed.rows_affected() != 1 {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_order".to_owned(),
                key: document_id.to_string(),
            });
        }
        let response = VoidDocumentResponse {
            document_id: document_id.to_string(),
            document_no: order_no,
            document_kind: "outbound_order".to_owned(),
            status: "voided".to_owned(),
            voided_at: now.clone(),
            voided_inventory_count: 0,
            released_inventory_count: allocations.len() as u32,
            quarantined_inventory_count: u32::try_from(quarantined_count).unwrap_or_default(),
            idempotent_replay: false,
        };
        insert_postgres_audit(transaction,tenant_id,actor_id,membership_id,device_id,session_id,"outbound_order.voided","outbound_order",document_id,&request.request_id,json!({"void_id":void_id,"reason":request.reason,"released_inventory_count":response.released_inventory_count,"quarantined_inventory_count":response.quarantined_inventory_count}),&now).await?;
        save_postgres_replay(
            transaction,
            tenant_id,
            OUTBOUND_SCOPE,
            &request.idempotency_key,
            &digest,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }
}

fn normalize_request(mut request: VoidDocumentRequest) -> Result<VoidDocumentRequest, String> {
    request.document_id = required("单据 ID", request.document_id)?;
    request.reason = required("作废原因", request.reason)?;
    request.password = required("操作密码", request.password)?;
    request.request_id = required("请求编号", request.request_id)?;
    request.idempotency_key = required("幂等键", request.idempotency_key)?;
    request.actor_id = request
        .actor_id
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    Ok(request)
}

fn normalize_network_request(
    mut request: VoidDocumentRequest,
) -> NetworkResult<VoidDocumentRequest> {
    request.actor_id = None;
    normalize_request(request).map_err(NetworkServiceError::Invalid)
}

fn normalize_copy_request(
    mut request: CopyDocumentSnRequest,
) -> Result<CopyDocumentSnRequest, String> {
    request.document_id = required("单据 ID", request.document_id)?;
    request.password = required("操作密码", request.password)?;
    request.request_id = required("请求编号", request.request_id)?;
    request.actor_id = request
        .actor_id
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    Ok(request)
}

fn normalize_network_copy_request(
    mut request: CopyDocumentSnRequest,
) -> NetworkResult<CopyDocumentSnRequest> {
    request.actor_id = None;
    normalize_copy_request(request).map_err(NetworkServiceError::Invalid)
}

fn required(label: &str, value: String) -> Result<String, String> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        Err(format!("{label}不能为空"))
    } else {
        Ok(value)
    }
}
fn new_id() -> String {
    Uuid::now_v7().to_string()
}
fn now_utc() -> Result<String, String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| error.to_string())
}
fn request_digest(request: &VoidDocumentRequest, actor_id: &str) -> Result<String, String> {
    let payload =
        json!({"document_id":request.document_id,"reason":request.reason,"actor_id":actor_id});
    let bytes = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

async fn verify_offline_password(
    transaction: &mut Transaction<'_, Sqlite>,
    passwords: &PasswordService,
    password: &str,
) -> Result<(), String> {
    let hash: String =
        sqlx::query_scalar("SELECT password_hash FROM operation_credentials WHERE id=1")
            .fetch_one(&mut **transaction)
            .await
            .map_err(|error| format!("无法读取操作密码: {error}"))?;
    if passwords
        .verify_password(password, &hash)
        .map_err(|error| error.to_string())?
    {
        Ok(())
    } else {
        Err("操作密码错误".to_owned())
    }
}

async fn insert_sqlite_document_void(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    id: &str,
    kind: &str,
    receipt_id: Option<&str>,
    order_id: Option<&str>,
    reason: &str,
    actor_id: &str,
    now: &str,
    request_id: &str,
    idempotency_key: &str,
) -> Result<(), String> {
    sqlx::query("INSERT INTO document_voids (id,workspace_id,document_kind,inbound_receipt_id,outbound_order_id,reason,actor_id,voided_at,request_id,idempotency_key,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?8)").bind(id).bind(workspace_id).bind(kind).bind(receipt_id).bind(order_id).bind(reason).bind(actor_id).bind(now).bind(request_id).bind(idempotency_key).execute(&mut **transaction).await.map_err(|error|format!("无法保存作废记录: {error}"))?;
    Ok(())
}
async fn insert_sqlite_movement(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    unit_id: &str,
    movement_type: &str,
    from: Option<&str>,
    to: Option<&str>,
    void_id: &str,
    actor_id: &str,
    now: &str,
) -> Result<(), String> {
    sqlx::query("INSERT INTO stock_movements (id,workspace_id,inventory_unit_id,movement_type,from_location_id,to_location_id,source_type,source_id,actor_id,occurred_at,created_at) VALUES (?1,?2,?3,?4,?5,?6,'document_void',?7,?8,?9,?9)").bind(new_id()).bind(workspace_id).bind(unit_id).bind(movement_type).bind(from).bind(to).bind(void_id).bind(actor_id).bind(now).execute(&mut **transaction).await.map_err(|error|format!("无法记录作废库存流水: {error}"))?;
    Ok(())
}
async fn insert_sqlite_audit(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    actor_id: &str,
    action: &str,
    entity_type: &str,
    entity_id: &str,
    request_id: &str,
    details: serde_json::Value,
    now: &str,
) -> Result<(), String> {
    sqlx::query("INSERT INTO audit_logs (id,workspace_id,actor_id,action,entity_type,entity_id,request_id,result,details_json,occurred_at) VALUES (?1,?2,?3,?4,?5,?6,?7,'success',?8,?9)").bind(new_id()).bind(workspace_id).bind(actor_id).bind(action).bind(entity_type).bind(entity_id).bind(request_id).bind(details.to_string()).bind(now).execute(&mut **transaction).await.map_err(|error|format!("无法记录作废审计: {error}"))?;
    Ok(())
}
async fn load_sqlite_replay(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    scope: &str,
    key: &str,
    digest: &str,
) -> Result<Option<VoidDocumentResponse>, String> {
    let row=sqlx::query("SELECT request_hash,response_json FROM idempotency_records WHERE workspace_id=?1 AND scope=?2 AND idempotency_key=?3").bind(workspace_id).bind(scope).bind(key).fetch_optional(&mut **transaction).await.map_err(|error|error.to_string())?;
    let Some(row) = row else { return Ok(None) };
    let stored: String = row
        .try_get("request_hash")
        .map_err(|error| error.to_string())?;
    if stored != digest {
        return Err("幂等键已用于不同的作废请求".to_owned());
    }
    let json: String = row
        .try_get("response_json")
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&json)
        .map(Some)
        .map_err(|error| error.to_string())
}
async fn save_sqlite_replay(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    scope: &str,
    key: &str,
    digest: &str,
    response: &VoidDocumentResponse,
    now: &str,
) -> Result<(), String> {
    sqlx::query("INSERT INTO idempotency_records (id,workspace_id,scope,idempotency_key,request_hash,response_json,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)").bind(new_id()).bind(workspace_id).bind(scope).bind(key).bind(digest).bind(serde_json::to_string(response).map_err(|error|error.to_string())?).bind(now).execute(&mut **transaction).await.map_err(|error|error.to_string())?;
    Ok(())
}

async fn insert_postgres_document_void(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    id: Uuid,
    kind: &str,
    receipt_id: Option<Uuid>,
    order_id: Option<Uuid>,
    reason: &str,
    actor_id: Uuid,
    now: &str,
    request_id: &str,
    idempotency_key: &str,
) -> NetworkResult<()> {
    sqlx::query("INSERT INTO document_voids (tenant_id,id,document_kind,inbound_receipt_id,outbound_order_id,reason,actor_id,voided_at,request_id,idempotency_key) VALUES ($1,$2,$3,$4,$5,$6,$7,$8::timestamptz,$9,$10)").bind(tenant_id).bind(id).bind(kind).bind(receipt_id).bind(order_id).bind(reason).bind(actor_id).bind(now).bind(request_id).bind(idempotency_key).execute(&mut **transaction).await?;
    Ok(())
}
async fn insert_postgres_movement(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    unit_id: Uuid,
    movement_type: &str,
    from: Option<Uuid>,
    to: Option<Uuid>,
    void_id: Uuid,
    actor_id: Uuid,
    now: &str,
) -> NetworkResult<()> {
    sqlx::query("INSERT INTO stock_movements (tenant_id,id,inventory_unit_id,movement_type,from_location_id,to_location_id,source_type,source_id,actor_id,occurred_at) VALUES ($1,$2,$3,$4,$5,$6,'document_void',$7,$8,$9::timestamptz)").bind(tenant_id).bind(Uuid::now_v7()).bind(unit_id).bind(movement_type).bind(from).bind(to).bind(void_id).bind(actor_id).bind(now).execute(&mut **transaction).await?;
    Ok(())
}
#[allow(clippy::too_many_arguments)]
async fn insert_postgres_audit(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor_id: Uuid,
    membership_id: Uuid,
    device_id: Uuid,
    session_id: Uuid,
    action: &str,
    entity_type: &str,
    entity_id: Uuid,
    request_id: &str,
    details: serde_json::Value,
    now: &str,
) -> NetworkResult<()> {
    sqlx::query("INSERT INTO audit_logs (tenant_id,id,actor_id,action,entity_type,entity_id,request_id,result,details_json,occurred_at,membership_id,device_id,session_id) VALUES ($1,$2,$3,$4,$5,$6,$7,'success',$8,$9::timestamptz,$10,$11,$12)").bind(tenant_id).bind(Uuid::now_v7()).bind(actor_id).bind(action).bind(entity_type).bind(entity_id).bind(request_id).bind(details).bind(now).bind(membership_id).bind(device_id).bind(session_id).execute(&mut **transaction).await?;
    Ok(())
}
async fn load_postgres_replay(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    scope: &str,
    key: &str,
    digest: &str,
) -> NetworkResult<Option<VoidDocumentResponse>> {
    let row=sqlx::query("SELECT request_hash,response_json FROM idempotency_records WHERE tenant_id=$1 AND scope=$2 AND idempotency_key=$3 FOR UPDATE").bind(tenant_id).bind(scope).bind(key).fetch_optional(&mut **transaction).await?;
    let Some(row) = row else { return Ok(None) };
    let stored: String = row.try_get("request_hash")?;
    if stored != digest {
        return Err(NetworkServiceError::Conflict {
            entity: "idempotency_key".to_owned(),
            key: key.to_owned(),
        });
    }
    let value: serde_json::Value = row.try_get("response_json")?;
    Ok(Some(serde_json::from_value(value)?))
}
async fn save_postgres_replay(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    scope: &str,
    key: &str,
    digest: &str,
    response: &VoidDocumentResponse,
) -> NetworkResult<()> {
    sqlx::query("INSERT INTO idempotency_records (tenant_id,id,scope,idempotency_key,request_hash,response_json) VALUES ($1,$2,$3,$4,$5,$6)").bind(tenant_id).bind(Uuid::now_v7()).bind(scope).bind(key).bind(digest).bind(serde_json::to_value(response)?).execute(&mut **transaction).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::application::{
        CatalogPartyRole, CreateCatalogPartyRequest, CreateCatalogProductRequest,
        PostReceiptRequest, PostReceiptResponse,
    };
    use crate::v2::outbound::{
        AllocateOutboundRequest, CreateOutboundOrderRequest, ReturnOutboundShipmentRequest,
        ShipOutboundRequest,
    };

    async fn test_database(label: &str) -> (OfflineDatabase, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("voiding-{label}-{}.sqlite3", Uuid::now_v7()));
        let database = OfflineDatabase::open(&path)
            .await
            .expect("open test database");
        database
            .create_catalog_product(CreateCatalogProductRequest {
                code: "RAM-TEST".to_owned(),
                name: "测试内存".to_owned(),
                serial_prefix: None,
                serial_forbidden_chars: String::new(),
            })
            .await
            .expect("create product");
        for (display_name, role) in [
            ("测试货主", CatalogPartyRole::GoodsOwner),
            ("测试供应商", CatalogPartyRole::Supplier),
        ] {
            database
                .create_catalog_party(CreateCatalogPartyRequest {
                    display_name: display_name.to_owned(),
                    role,
                })
                .await
                .expect("create party");
        }
        (database, path)
    }

    fn receipt_request(label: &str, barcode: &str) -> PostReceiptRequest {
        PostReceiptRequest {
            request_id: format!("receipt-request-{label}"),
            idempotency_key: format!("receipt-key-{label}"),
            receipt_no: format!("RK-{label}"),
            owner_name: "测试货主".to_owned(),
            supplier_name: "测试供应商".to_owned(),
            sku_code: "RAM-TEST".to_owned(),
            sku_name: "测试内存".to_owned(),
            source_reference: None,
            received_at: "2026-08-15T01:00:00Z".to_owned(),
            actor_id: "operator".to_owned(),
            barcodes: vec![barcode.to_owned()],
            notes: None,
            warranty: None,
        }
    }

    fn void_request(document_id: &str, key: &str, password: &str) -> VoidDocumentRequest {
        VoidDocumentRequest {
            document_id: document_id.to_owned(),
            reason: "录入单据有误".to_owned(),
            password: password.to_owned(),
            actor_id: Some("operator".to_owned()),
            request_id: format!("void-request-{key}"),
            idempotency_key: format!("void-key-{key}"),
        }
    }

    async fn make_available(database: &OfflineDatabase, receipt: &PostReceiptResponse) {
        sqlx::query("UPDATE inventory_units SET inventory_status='available',quality_status='passed',location_id=?1 WHERE id=?2")
            .bind(database.storage_location_id())
            .bind(&receipt.units[0].inventory_unit_id)
            .execute(database.pool())
            .await
            .expect("make inventory available");
    }

    async fn create_allocated_order(
        database: &OfflineDatabase,
        receipt: &PostReceiptResponse,
        label: &str,
    ) -> (
        crate::v2::outbound::CreateOutboundOrderResponse,
        crate::v2::outbound::AllocateOutboundResponse,
    ) {
        let order = database
            .create_outbound_order(CreateOutboundOrderRequest {
                request_id: format!("order-request-{label}"),
                idempotency_key: format!("order-key-{label}"),
                order_no: format!("DD-{label}"),
                upstream_receiver_name: "测试客户".to_owned(),
                sku_code: "RAM-TEST".to_owned(),
                sku_name: "测试内存".to_owned(),
                required_quantity: 1,
                required_at: None,
                actor_id: "operator".to_owned(),
            })
            .await
            .expect("create order");
        let allocation = database
            .allocate_outbound_order(AllocateOutboundRequest {
                request_id: format!("allocation-request-{label}"),
                idempotency_key: format!("allocation-key-{label}"),
                order_id: order.order_id.clone(),
                order_line_id: order.order_line_id.clone(),
                barcodes: vec![receipt.units[0].barcode.clone()],
                allow_mixed_skus: false,
                actor_id: "operator".to_owned(),
            })
            .await
            .expect("allocate order");
        (order, allocation)
    }

    async fn close_database(database: OfflineDatabase, path: std::path::PathBuf) {
        database.pool().close().await;
        for candidate in [
            path.clone(),
            std::path::PathBuf::from(format!("{}-wal", path.display())),
            std::path::PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = std::fs::remove_file(candidate);
        }
    }

    #[test]
    fn normalization_requires_reason_and_password() {
        let request = VoidDocumentRequest {
            document_id: "receipt-1".to_owned(),
            reason: " ".to_owned(),
            password: "admin".to_owned(),
            actor_id: Some("operator".to_owned()),
            request_id: "request-1".to_owned(),
            idempotency_key: "key-1".to_owned(),
        };
        assert!(normalize_request(request).is_err());
    }

    #[tokio::test]
    async fn copying_document_sns_requires_password_and_returns_the_full_document() {
        let (database, path) = test_database("copy-sns").await;
        let receipt = database
            .post_receipt(PostReceiptRequest {
                barcodes: vec!["COPY-SN-002".to_owned(), "COPY-SN-001".to_owned()],
                ..receipt_request("copy-sns", "COPY-SN-001")
            })
            .await
            .expect("post receipt");
        let request = CopyDocumentSnRequest {
            document_id: receipt.receipt_id.clone(),
            password: "wrong".to_owned(),
            actor_id: Some("operator".to_owned()),
            request_id: "copy-receipt-wrong".to_owned(),
        };
        assert!(database.copy_receipt_document_sns(request).await.is_err());
        let response = database
            .copy_receipt_document_sns(CopyDocumentSnRequest {
                document_id: receipt.receipt_id.clone(),
                password: "admin".to_owned(),
                actor_id: Some("operator".to_owned()),
                request_id: "copy-receipt-ok".to_owned(),
            })
            .await
            .expect("copy receipt SN");
        assert_eq!(response.barcodes, ["COPY-SN-001", "COPY-SN-002"]);
        let audit_actor: String = sqlx::query_scalar(
            "SELECT actor_id FROM audit_logs WHERE action='inbound_receipt.sns_copied'",
        )
        .fetch_one(database.pool())
        .await
        .expect("copy audit");
        assert_eq!(audit_actor, "operator");

        make_available(&database, &receipt).await;
        let (order, _) = create_allocated_order(&database, &receipt, "copy-sns").await;
        let outbound = database
            .copy_outbound_order_document_sns(CopyDocumentSnRequest {
                document_id: order.order_id,
                password: "admin".to_owned(),
                actor_id: Some("operator".to_owned()),
                request_id: "copy-outbound-ok".to_owned(),
            })
            .await
            .expect("copy outbound SN");
        assert_eq!(outbound.barcodes, [receipt.units[0].barcode.as_str()]);
        close_database(database, path).await;
    }

    #[tokio::test]
    async fn receipt_void_requires_password_is_atomic_and_replays_idempotently() {
        let (database, path) = test_database("receipt").await;
        let receipt = database
            .post_receipt(receipt_request("001", "VOID001"))
            .await
            .expect("post receipt");
        let error = database
            .void_receipt_document(void_request(&receipt.receipt_id, "wrong", "wrong"))
            .await
            .expect_err("wrong password rejected");
        assert!(error.contains("密码错误"));
        let status: String = sqlx::query_scalar("SELECT status FROM inbound_receipts WHERE id=?1")
            .bind(&receipt.receipt_id)
            .fetch_one(database.pool())
            .await
            .expect("receipt status");
        assert_eq!(status, "posted");

        let request = void_request(&receipt.receipt_id, "receipt", "admin");
        let response = database
            .void_receipt_document(request.clone())
            .await
            .expect("void receipt");
        assert_eq!(response.voided_inventory_count, 1);
        assert!(!response.idempotent_replay);
        let replay = database
            .void_receipt_document(request)
            .await
            .expect("replay void");
        assert!(replay.idempotent_replay);
        let unit_status: String =
            sqlx::query_scalar("SELECT inventory_status FROM inventory_units WHERE id=?1")
                .bind(&receipt.units[0].inventory_unit_id)
                .fetch_one(database.pool())
                .await
                .expect("unit status");
        assert_eq!(unit_status, "voided");
        let movements: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stock_movements WHERE inventory_unit_id=?1 AND movement_type='voided'").bind(&receipt.units[0].inventory_unit_id).fetch_one(database.pool()).await.expect("void movement");
        assert_eq!(movements, 1);
        close_database(database, path).await;
    }

    #[tokio::test]
    async fn outbound_void_releases_unshipped_inventory_and_blocks_receipt_void() {
        let (database, path) = test_database("allocated").await;
        let receipt = database
            .post_receipt(receipt_request("002", "VOID002"))
            .await
            .expect("post receipt");
        make_available(&database, &receipt).await;
        let original_location = database.storage_location_id().to_owned();
        let (order, allocation) = create_allocated_order(&database, &receipt, "002").await;

        let receipt_error = database
            .void_receipt_document(void_request(
                &receipt.receipt_id,
                "receipt-blocked",
                "admin",
            ))
            .await
            .expect_err("receipt with allocation rejected");
        assert!(receipt_error.contains("已有出库关联"));
        let response = database
            .void_outbound_order_document(void_request(&order.order_id, "order-release", "admin"))
            .await
            .expect("void outbound order");
        assert_eq!(response.released_inventory_count, 1);
        let row =
            sqlx::query("SELECT inventory_status,location_id FROM inventory_units WHERE id=?1")
                .bind(&receipt.units[0].inventory_unit_id)
                .fetch_one(database.pool())
                .await
                .expect("released unit");
        assert_eq!(
            row.try_get::<String, _>("inventory_status").unwrap(),
            "available"
        );
        assert_eq!(
            row.try_get::<String, _>("location_id").unwrap(),
            original_location
        );
        let allocation_status: String =
            sqlx::query_scalar("SELECT status FROM outbound_allocations WHERE id=?1")
                .bind(&allocation.allocations[0].allocation_id)
                .fetch_one(database.pool())
                .await
                .expect("allocation status");
        assert_eq!(allocation_status, "voided");
        let receipt_error = database
            .void_receipt_document(void_request(
                &receipt.receipt_id,
                "receipt-still-blocked",
                "admin",
            ))
            .await
            .expect_err("historical allocation still blocks receipt");
        assert!(receipt_error.contains("已有出库关联"));
        close_database(database, path).await;
    }

    #[tokio::test]
    async fn shipped_inventory_must_be_returned_before_order_void() {
        let (database, path) = test_database("shipped").await;
        let receipt = database
            .post_receipt(receipt_request("003", "VOID003"))
            .await
            .expect("post receipt");
        make_available(&database, &receipt).await;
        let (order, allocation) = create_allocated_order(&database, &receipt, "003").await;
        database
            .ship_outbound_order(ShipOutboundRequest {
                request_id: "ship-request-003".to_owned(),
                idempotency_key: "ship-key-003".to_owned(),
                order_id: order.order_id.clone(),
                shipment_no: "CK-003".to_owned(),
                allocation_ids: vec![allocation.allocations[0].allocation_id.clone()],
                barcodes: Vec::new(),
                shipped_at: "2026-08-15T02:00:00Z".to_owned(),
                actor_id: "operator".to_owned(),
                warranty: None,
            })
            .await
            .expect("ship order");
        let error = database
            .void_outbound_order_document(void_request(&order.order_id, "shipped-blocked", "admin"))
            .await
            .expect_err("unreturned shipment rejected");
        assert!(error.contains("未退回") && error.contains("VOID003"));
        let order_status: String =
            sqlx::query_scalar("SELECT status FROM outbound_orders WHERE id=?1")
                .bind(&order.order_id)
                .fetch_one(database.pool())
                .await
                .expect("order status");
        assert_eq!(order_status, "shipped");
        close_database(database, path).await;
    }

    #[tokio::test]
    async fn fully_returned_inventory_stays_quarantined_when_order_is_voided() {
        let (database, path) = test_database("returned").await;
        let receipt = database
            .post_receipt(receipt_request("005", "VOID005"))
            .await
            .expect("post receipt");
        make_available(&database, &receipt).await;
        let (order, allocation) = create_allocated_order(&database, &receipt, "005").await;
        let shipment = database
            .ship_outbound_order(ShipOutboundRequest {
                request_id: "ship-request-005".to_owned(),
                idempotency_key: "ship-key-005".to_owned(),
                order_id: order.order_id.clone(),
                shipment_no: "CK-005".to_owned(),
                allocation_ids: vec![allocation.allocations[0].allocation_id.clone()],
                barcodes: Vec::new(),
                shipped_at: "2026-08-15T02:00:00Z".to_owned(),
                actor_id: "operator".to_owned(),
                warranty: None,
            })
            .await
            .expect("ship order");
        database
            .return_outbound_shipment(ReturnOutboundShipmentRequest {
                request_id: "return-request-005".to_owned(),
                idempotency_key: "return-key-005".to_owned(),
                shipment_id: shipment.shipment_id.clone(),
                shipment_line_ids: vec![shipment.items[0].shipment_line_id.clone()],
                return_no: "TH-005".to_owned(),
                returned_at: "2026-08-15T03:00:00Z".to_owned(),
                reason: "客户退回".to_owned(),
                actor_id: "operator".to_owned(),
            })
            .await
            .expect("return shipment");

        let response = database
            .void_outbound_order_document(void_request(&order.order_id, "returned-order", "admin"))
            .await
            .expect("void fully returned order");
        assert_eq!(response.released_inventory_count, 0);
        assert_eq!(response.quarantined_inventory_count, 1);
        let inventory_status: String =
            sqlx::query_scalar("SELECT inventory_status FROM inventory_units WHERE id=?1")
                .bind(&receipt.units[0].inventory_unit_id)
                .fetch_one(database.pool())
                .await
                .expect("returned inventory status");
        assert_eq!(inventory_status, "quarantined");
        let allocation_status: String =
            sqlx::query_scalar("SELECT status FROM outbound_allocations WHERE id=?1")
                .bind(&allocation.allocations[0].allocation_id)
                .fetch_one(database.pool())
                .await
                .expect("returned allocation status");
        assert_eq!(allocation_status, "released");
        let shipment_status: String =
            sqlx::query_scalar("SELECT status FROM outbound_shipments WHERE id=?1")
                .bind(&shipment.shipment_id)
                .fetch_one(database.pool())
                .await
                .expect("shipment status");
        assert_eq!(shipment_status, "voided");
        close_database(database, path).await;
    }

    #[tokio::test]
    async fn operation_password_can_be_changed_without_storing_plaintext() {
        let (database, path) = test_database("password").await;
        database
            .change_operation_password(ChangeOperationPasswordRequest {
                current_password: "admin".to_owned(),
                new_password: "new-secure-password".to_owned(),
                actor_id: "operator".to_owned(),
                request_id: "password-change".to_owned(),
            })
            .await
            .expect("change password");
        let hash: String =
            sqlx::query_scalar("SELECT password_hash FROM operation_credentials WHERE id=1")
                .fetch_one(database.pool())
                .await
                .expect("password hash");
        assert!(hash.starts_with("$argon2id$") && !hash.contains("new-secure-password"));
        let receipt = database
            .post_receipt(receipt_request("004", "VOID004"))
            .await
            .expect("post receipt");
        assert!(database
            .void_receipt_document(void_request(&receipt.receipt_id, "old-password", "admin"))
            .await
            .is_err());
        database
            .void_receipt_document(void_request(
                &receipt.receipt_id,
                "new-password",
                "new-secure-password",
            ))
            .await
            .expect("new password accepted");
        close_database(database, path).await;
    }
}
