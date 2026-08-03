//! Transactional multi-owner order allocation and outbound workflows.
//!
//! The outbound path always re-reads inventory state inside its write
//! transaction.  A frontend preview is advisory only: allocation, shipment,
//! delivery and return each validate the current database projection before
//! writing facts and append-only stock movements.

use super::domain::{InventoryStatus, InventoryUnit, OutboundOrderLine, QualityStatus};
use super::sqlite::{now_utc, OfflineDatabase};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};
use std::collections::{HashMap, HashSet};
use thiserror::Error;
use uuid::Uuid;

const ORDER_SCOPE: &str = "create_outbound_order";
const ALLOCATION_SCOPE: &str = "allocate_outbound_order";
const SHIPMENT_SCOPE: &str = "ship_outbound_order";
const DELIVERY_SCOPE: &str = "confirm_outbound_delivery";
const RETURN_SCOPE: &str = "return_outbound_shipment";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateOutboundOrderRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub order_no: String,
    pub upstream_receiver_name: String,
    pub sku_code: String,
    pub sku_name: String,
    pub required_quantity: u32,
    pub required_at: Option<String>,
    pub actor_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateOutboundOrderResponse {
    pub order_id: String,
    pub order_line_id: String,
    pub order_no: String,
    pub upstream_receiver_id: String,
    pub sku_id: String,
    pub required_quantity: u32,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocateOutboundRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub order_id: String,
    pub order_line_id: String,
    /// Empty means automatic FIFO allocation. Non-empty means explicit SNs.
    #[serde(default)]
    pub barcodes: Vec<String>,
    pub actor_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationItem {
    pub allocation_id: String,
    pub barcode: String,
    pub owner_party_id: String,
    pub sku_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocateOutboundResponse {
    pub order_id: String,
    pub order_line_id: String,
    pub allocated_count: u32,
    pub order_status: String,
    pub allocations: Vec<AllocationItem>,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShipOutboundRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub order_id: String,
    pub shipment_no: String,
    #[serde(default)]
    pub allocation_ids: Vec<String>,
    #[serde(default)]
    pub barcodes: Vec<String>,
    pub shipped_at: String,
    pub actor_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShipmentItem {
    pub shipment_line_id: String,
    pub allocation_id: String,
    pub barcode: String,
    pub owner_party_id: String,
    pub sku_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShipOutboundResponse {
    pub shipment_id: String,
    pub shipment_no: String,
    pub shipped_count: u32,
    pub order_status: String,
    pub items: Vec<ShipmentItem>,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmOutboundDeliveryRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub shipment_id: String,
    pub confirmation_code: String,
    #[serde(default)]
    pub shipment_line_ids: Vec<String>,
    pub confirmed_at: String,
    pub confirmed_by: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmOutboundDeliveryResponse {
    pub confirmation_id: String,
    pub confirmation_code: String,
    pub delivered_count: u32,
    pub shipment_status: String,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnOutboundShipmentRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub shipment_id: String,
    #[serde(default)]
    pub shipment_line_ids: Vec<String>,
    pub return_no: String,
    pub returned_at: String,
    pub reason: String,
    pub actor_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnOutboundShipmentResponse {
    pub return_batch_id: String,
    pub return_no: String,
    pub quarantined_count: u32,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundOrderDetails {
    pub order_id: String,
    pub order_no: String,
    pub receiver_name: String,
    pub status: String,
    pub lines: Vec<OutboundOrderDetailLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundOrderDetailLine {
    pub order_line_id: String,
    pub sku_code: String,
    pub sku_name: String,
    pub required_quantity: u32,
    pub allocated_quantity: u32,
    pub shipped_quantity: u32,
    pub delivered_quantity: u32,
    pub allocations: Vec<AllocationItem>,
}

#[derive(Debug, Error)]
pub enum OutboundError {
    #[error("invalid outbound request: {0}")]
    Invalid(String),
    #[error("outbound entity not found: {0}")]
    NotFound(String),
    #[error("outbound conflict: {0}")]
    Conflict(String),
    #[error("outbound domain rule rejected: {0}")]
    Domain(String),
    #[error("outbound storage failed: {0}")]
    Storage(String),
}

impl From<super::domain::DomainError> for OutboundError {
    fn from(error: super::domain::DomainError) -> Self {
        Self::Domain(error.to_string())
    }
}

type OutboundResult<T> = Result<T, OutboundError>;

impl OfflineDatabase {
    pub async fn create_outbound_order(
        &self,
        request: CreateOutboundOrderRequest,
    ) -> OutboundResult<CreateOutboundOrderResponse> {
        let request = normalize_create_order(request)?;
        let digest = request_digest(&request)?;
        let workspace_id = self.workspace_id().to_owned();
        let now = now_utc().map_err(OutboundError::Storage)?;
        let mut tx = begin_write(self, &workspace_id).await?;
        if let Some(mut response) = load_idempotent::<CreateOutboundOrderResponse>(
            &mut tx,
            &workspace_id,
            ORDER_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            response.idempotent_replay = true;
            tx.commit().await.map_err(commit_error)?;
            return Ok(response);
        }

        let receiver_id = upsert_party(
            &mut tx,
            &workspace_id,
            &request.upstream_receiver_name,
            "upstream_receiver",
            &now,
        )
        .await?;
        let sku_id = upsert_sku(
            &mut tx,
            &workspace_id,
            &request.sku_code,
            &request.sku_name,
            &now,
        )
        .await?;
        let order_id = new_id();
        let line_id = new_id();
        sqlx::query(
            r#"
            INSERT INTO outbound_orders
                (id, workspace_id, order_no, upstream_receiver_id, required_at,
                 status, actor_id, idempotency_key, request_id, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7, ?8, ?9)
            "#,
        )
        .bind(&order_id)
        .bind(&workspace_id)
        .bind(&request.order_no)
        .bind(&receiver_id)
        .bind(&request.required_at)
        .bind(&request.actor_id)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .map_err(|error| unique_or_storage("insert outbound order", error))?;
        sqlx::query(
            r#"
            INSERT INTO outbound_order_lines
                (id, workspace_id, outbound_order_id, sku_id, required_quantity,
                 allocated_quantity, shipped_quantity, delivered_quantity, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0, ?6)
            "#,
        )
        .bind(&line_id)
        .bind(&workspace_id)
        .bind(&order_id)
        .bind(&sku_id)
        .bind(i64::from(request.required_quantity))
        .bind(&now)
        .execute(&mut *tx)
        .await
        .map_err(|error| storage("insert outbound order line", error))?;

        let response = CreateOutboundOrderResponse {
            order_id: order_id.clone(),
            order_line_id: line_id,
            order_no: request.order_no.clone(),
            upstream_receiver_id: receiver_id,
            sku_id,
            required_quantity: request.required_quantity,
            idempotent_replay: false,
        };
        write_audit(
            &mut tx,
            &workspace_id,
            &request.actor_id,
            "outbound_order.created",
            "outbound_order",
            &order_id,
            &request.request_id,
            json!({"order_no": request.order_no, "required_quantity": request.required_quantity}),
            &now,
        )
        .await?;
        save_idempotent(
            &mut tx,
            &workspace_id,
            ORDER_SCOPE,
            &request.idempotency_key,
            &digest,
            &response,
            &now,
        )
        .await?;
        tx.commit().await.map_err(commit_error)?;
        Ok(response)
    }

    pub async fn allocate_outbound_order(
        &self,
        request: AllocateOutboundRequest,
    ) -> OutboundResult<AllocateOutboundResponse> {
        let request = normalize_allocate(request)?;
        let digest = request_digest(&request)?;
        let workspace_id = self.workspace_id().to_owned();
        let now = now_utc().map_err(OutboundError::Storage)?;
        let mut tx = begin_write(self, &workspace_id).await?;
        if let Some(mut response) = load_idempotent::<AllocateOutboundResponse>(
            &mut tx,
            &workspace_id,
            ALLOCATION_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            response.idempotent_replay = true;
            tx.commit().await.map_err(commit_error)?;
            return Ok(response);
        }
        let line = sqlx::query(
            "SELECT l.outbound_order_id, l.sku_id, l.required_quantity, l.allocated_quantity, o.status AS order_status FROM outbound_order_lines l JOIN outbound_orders o ON o.id = l.outbound_order_id AND o.workspace_id = l.workspace_id WHERE l.workspace_id = ?1 AND l.id = ?2",
        )
        .bind(&workspace_id)
        .bind(&request.order_line_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| storage("load outbound order line", error))?
        .ok_or_else(|| OutboundError::NotFound(request.order_line_id.clone()))?;
        let order_id: String = line.try_get("outbound_order_id").map_err(row_error)?;
        if order_id != request.order_id {
            return Err(OutboundError::Conflict(
                "order line does not belong to order".to_owned(),
            ));
        }
        let order_status: String = line.try_get("order_status").map_err(row_error)?;
        if matches!(order_status.as_str(), "voided" | "shipped" | "completed") {
            return Err(OutboundError::Conflict(format!(
                "order cannot be allocated in status {order_status}"
            )));
        }
        let sku_id: String = line.try_get("sku_id").map_err(row_error)?;
        let required: i64 = line.try_get("required_quantity").map_err(row_error)?;
        let allocated: i64 = line.try_get("allocated_quantity").map_err(row_error)?;
        let remaining = required.saturating_sub(allocated);
        if remaining == 0 {
            return Err(OutboundError::Conflict(
                "order line is already fully allocated".to_owned(),
            ));
        }

        let candidates = if request.barcodes.is_empty() {
            sqlx::query(
                "SELECT id, barcode, owner_party_id, sku_id, inbound_receipt_line_id, location_id, version, received_at, quality_status FROM inventory_units WHERE workspace_id = ?1 AND sku_id = ?2 AND inventory_status = 'available' AND quality_status IN ('passed', 'waived') ORDER BY received_at, id LIMIT ?3",
            )
            .bind(&workspace_id)
            .bind(&sku_id)
            .bind(remaining)
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| storage("select FIFO allocation candidates", error))?
        } else {
            let mut rows = Vec::with_capacity(request.barcodes.len());
            for barcode in &request.barcodes {
                let row = sqlx::query(
                    "SELECT id, barcode, owner_party_id, sku_id, inbound_receipt_line_id, location_id, version, received_at, quality_status FROM inventory_units WHERE workspace_id = ?1 AND barcode = ?2",
                )
                .bind(&workspace_id)
                .bind(barcode)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| storage("load selected allocation candidate", error))?
                .ok_or_else(|| OutboundError::NotFound(format!("inventory barcode {barcode}")))?;
                rows.push(row);
            }
            rows
        };
        if candidates.is_empty() {
            return Err(OutboundError::Conflict(
                "no quality-passed inventory is available".to_owned(),
            ));
        }
        if candidates.len() as i64 > remaining {
            return Err(OutboundError::Conflict(format!(
                "requested {} units but only {} remain on the order line",
                candidates.len(),
                remaining
            )));
        }

        let mut seen = HashSet::new();
        let mut allocation_items = Vec::with_capacity(candidates.len());
        for row in candidates {
            let unit_id: String = row.try_get("id").map_err(row_error)?;
            let barcode: String = row.try_get("barcode").map_err(row_error)?;
            if !seen.insert(barcode.clone()) {
                return Err(OutboundError::Invalid(format!(
                    "duplicate barcode {barcode}"
                )));
            }
            let candidate_sku: String = row.try_get("sku_id").map_err(row_error)?;
            let owner_party_id: String = row.try_get("owner_party_id").map_err(row_error)?;
            let inbound_line_id: String =
                row.try_get("inbound_receipt_line_id").map_err(row_error)?;
            let location_id: String = row.try_get("location_id").map_err(row_error)?;
            let version: i64 = row.try_get("version").map_err(row_error)?;
            let received_at: String = row.try_get("received_at").map_err(row_error)?;
            let quality_status = parse_quality_status(
                row.try_get::<String, _>("quality_status")
                    .map_err(row_error)?
                    .as_str(),
            )?;
            let unit = InventoryUnit {
                id: unit_id.clone(),
                barcode: barcode.clone(),
                inbound_receipt_line_id: inbound_line_id.clone(),
                owner_party_id: owner_party_id.clone(),
                sku_id: candidate_sku.clone(),
                location_id,
                received_at,
                inventory_status: InventoryStatus::Available,
                quality_status,
                active_allocation_id: None,
                latest_shipment_line_id: None,
                version: u64::try_from(version)
                    .map_err(|_| OutboundError::Storage("invalid inventory version".to_owned()))?,
            };
            unit.ensure_allocation_eligible(&sku_id)?;
            let allocation_id = new_id();
            let mut order_line = OutboundOrderLine::new(
                request.order_line_id.clone(),
                request.order_id.clone(),
                sku_id.clone(),
                u32::try_from(required)
                    .map_err(|_| OutboundError::Storage("invalid required quantity".to_owned()))?,
            )?;
            order_line.allocated_quantity =
                u32::try_from(allocated + allocation_items.len() as i64)
                    .map_err(|_| OutboundError::Storage("invalid allocated quantity".to_owned()))?;
            let mut unit_for_domain = unit.clone();
            let _allocation = order_line.allocate_unit(
                &mut unit_for_domain,
                allocation_id.clone(),
                now.clone(),
            )?;
            let updated = sqlx::query(
                "UPDATE inventory_units SET inventory_status = 'reserved', location_id = ?1, version = version + 1, updated_at = ?2 WHERE workspace_id = ?3 AND id = ?4 AND version = ?5 AND inventory_status = 'available' AND quality_status IN ('passed', 'waived')",
            )
            .bind(self.shipping_location_id())
            .bind(&now)
            .bind(&workspace_id)
            .bind(&unit_id)
            .bind(version)
            .execute(&mut *tx)
            .await
            .map_err(|error| storage("reserve inventory unit", error))?;
            if updated.rows_affected() != 1 {
                return Err(OutboundError::Conflict(format!(
                    "inventory barcode {barcode} was concurrently claimed"
                )));
            }
            sqlx::query(
                "INSERT INTO outbound_allocations (id, workspace_id, outbound_order_line_id, inventory_unit_id, status, allocated_by, allocated_at) VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6)",
            )
            .bind(&allocation_id)
            .bind(&workspace_id)
            .bind(&request.order_line_id)
            .bind(&unit_id)
            .bind(&request.actor_id)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(|error| unique_or_storage("insert outbound allocation", error))?;
            sqlx::query(
                "INSERT INTO stock_movements (id, workspace_id, inventory_unit_id, movement_type, from_location_id, to_location_id, source_type, source_id, actor_id, occurred_at, created_at) VALUES (?1, ?2, ?3, 'reserved', ?4, ?5, 'outbound_order_line', ?6, ?7, ?8, ?9)",
            )
            .bind(new_id())
            .bind(&workspace_id)
            .bind(&unit_id)
            .bind(&unit.location_id)
            .bind(self.shipping_location_id())
            .bind(&request.order_line_id)
            .bind(&request.actor_id)
            .bind(&now)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(|error| storage("insert reservation movement", error))?;
            allocation_items.push(AllocationItem {
                allocation_id,
                barcode,
                owner_party_id,
                sku_id: candidate_sku,
            });
        }
        let new_allocated = allocated + allocation_items.len() as i64;
        sqlx::query("UPDATE outbound_order_lines SET allocated_quantity = ?1 WHERE workspace_id = ?2 AND id = ?3")
            .bind(new_allocated)
            .bind(&workspace_id)
            .bind(&request.order_line_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| storage("update allocated quantity", error))?;
        let order_status = if new_allocated >= required {
            "allocated"
        } else {
            "partially_allocated"
        };
        sqlx::query("UPDATE outbound_orders SET status = ?1 WHERE workspace_id = ?2 AND id = ?3")
            .bind(order_status)
            .bind(&workspace_id)
            .bind(&request.order_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| storage("update outbound order allocation status", error))?;
        let response = AllocateOutboundResponse {
            order_id: request.order_id.clone(),
            order_line_id: request.order_line_id.clone(),
            allocated_count: allocation_items.len() as u32,
            order_status: order_status.to_owned(),
            allocations: allocation_items,
            idempotent_replay: false,
        };
        write_audit(
            &mut tx,
            &workspace_id,
            &request.actor_id,
            "outbound_order.allocated",
            "outbound_order",
            &request.order_id,
            &request.request_id,
            json!({"order_line_id": request.order_line_id, "allocated_count": response.allocated_count}),
            &now,
        )
        .await?;
        save_idempotent(
            &mut tx,
            &workspace_id,
            ALLOCATION_SCOPE,
            &request.idempotency_key,
            &digest,
            &response,
            &now,
        )
        .await?;
        tx.commit().await.map_err(commit_error)?;
        Ok(response)
    }

    pub async fn ship_outbound_order(
        &self,
        request: ShipOutboundRequest,
    ) -> OutboundResult<ShipOutboundResponse> {
        let request = normalize_ship(request)?;
        let digest = request_digest(&request)?;
        let workspace_id = self.workspace_id().to_owned();
        let now = now_utc().map_err(OutboundError::Storage)?;
        let mut tx = begin_write(self, &workspace_id).await?;
        if let Some(mut response) = load_idempotent::<ShipOutboundResponse>(
            &mut tx,
            &workspace_id,
            SHIPMENT_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            response.idempotent_replay = true;
            tx.commit().await.map_err(commit_error)?;
            return Ok(response);
        }
        let allocation_rows = load_allocation_rows(&mut tx, &workspace_id, &request).await?;
        if allocation_rows.is_empty() {
            return Err(OutboundError::Conflict(
                "no active allocations selected".to_owned(),
            ));
        }
        let shipment_id = new_id();
        sqlx::query("INSERT INTO outbound_shipments (id, workspace_id, shipment_no, outbound_order_id, status, shipped_at, actor_id, idempotency_key, request_id, created_at) VALUES (?1, ?2, ?3, ?4, 'posted', ?5, ?6, ?7, ?8, ?9)")
            .bind(&shipment_id).bind(&workspace_id).bind(&request.shipment_no).bind(&request.order_id)
            .bind(&request.shipped_at).bind(&request.actor_id).bind(&request.idempotency_key).bind(&request.request_id).bind(&now)
            .execute(&mut *tx).await.map_err(|error| unique_or_storage("insert outbound shipment", error))?;
        let mut items = Vec::with_capacity(allocation_rows.len());
        let mut shipped_by_line: HashMap<String, i64> = HashMap::new();
        for row in allocation_rows {
            let allocation_id: String = row.try_get("allocation_id").map_err(row_error)?;
            let line_id: String = row.try_get("order_line_id").map_err(row_error)?;
            let unit_id: String = row.try_get("unit_id").map_err(row_error)?;
            let barcode: String = row.try_get("barcode").map_err(row_error)?;
            let owner_party_id: String = row.try_get("owner_party_id").map_err(row_error)?;
            let sku_id: String = row.try_get("sku_id").map_err(row_error)?;
            let version: i64 = row.try_get("version").map_err(row_error)?;
            let quality_status: String = row.try_get("quality_status").map_err(row_error)?;
            let inventory_status: String = row.try_get("inventory_status").map_err(row_error)?;
            if inventory_status != "reserved"
                || !matches!(quality_status.as_str(), "passed" | "waived")
            {
                return Err(OutboundError::Conflict(format!(
                    "inventory barcode {barcode} is no longer shippable"
                )));
            }
            let shipment_line_id = new_id();
            let allocation_updated = sqlx::query("UPDATE outbound_allocations SET status = 'shipped' WHERE workspace_id = ?1 AND id = ?2 AND status = 'active'")
                .bind(&workspace_id).bind(&allocation_id).execute(&mut *tx).await
                .map_err(|error| storage("mark allocation shipped", error))?;
            if allocation_updated.rows_affected() != 1 {
                return Err(OutboundError::Conflict(format!(
                    "allocation {allocation_id} was already shipped or released"
                )));
            }
            let updated = sqlx::query("UPDATE inventory_units SET inventory_status = 'shipped', version = version + 1, updated_at = ?1 WHERE workspace_id = ?2 AND id = ?3 AND version = ?4 AND inventory_status = 'reserved'")
                .bind(&now).bind(&workspace_id).bind(&unit_id).bind(version).execute(&mut *tx).await
                .map_err(|error| storage("ship inventory unit", error))?;
            if updated.rows_affected() != 1 {
                return Err(OutboundError::Conflict(format!(
                    "inventory barcode {barcode} changed during shipment"
                )));
            }
            sqlx::query("INSERT INTO outbound_shipment_lines (id, workspace_id, outbound_shipment_id, outbound_allocation_id, inventory_unit_id, scanned_barcode_snapshot, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")
                .bind(&shipment_line_id).bind(&workspace_id).bind(&shipment_id).bind(&allocation_id).bind(&unit_id).bind(&barcode).bind(&now)
                .execute(&mut *tx).await.map_err(|error| unique_or_storage("insert outbound shipment line", error))?;
            sqlx::query("INSERT INTO stock_movements (id, workspace_id, inventory_unit_id, movement_type, source_type, source_id, actor_id, occurred_at, created_at) VALUES (?1, ?2, ?3, 'shipped', 'outbound_shipment', ?4, ?5, ?6, ?7)")
                .bind(new_id()).bind(&workspace_id).bind(&unit_id).bind(&shipment_id).bind(&request.actor_id).bind(&request.shipped_at).bind(&now)
                .execute(&mut *tx).await.map_err(|error| storage("insert shipment movement", error))?;
            *shipped_by_line.entry(line_id).or_default() += 1;
            items.push(ShipmentItem {
                shipment_line_id,
                allocation_id,
                barcode,
                owner_party_id,
                sku_id,
            });
        }
        for (line_id, count) in shipped_by_line {
            let updated = sqlx::query("UPDATE outbound_order_lines SET shipped_quantity = shipped_quantity + ?1 WHERE workspace_id = ?2 AND id = ?3 AND shipped_quantity + ?1 <= allocated_quantity")
                .bind(count)
                .bind(&workspace_id)
                .bind(line_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| storage("update shipped quantity", error))?;
            if updated.rows_affected() != 1 {
                return Err(OutboundError::Conflict(
                    "shipment exceeds allocated quantity".to_owned(),
                ));
            }
        }
        let order_status =
            order_status_after_ship(&mut tx, &workspace_id, &request.order_id).await?;
        sqlx::query("UPDATE outbound_orders SET status = ?1 WHERE workspace_id = ?2 AND id = ?3")
            .bind(&order_status)
            .bind(&workspace_id)
            .bind(&request.order_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| storage("update order shipment status", error))?;
        let response = ShipOutboundResponse {
            shipment_id: shipment_id.clone(),
            shipment_no: request.shipment_no.clone(),
            shipped_count: items.len() as u32,
            order_status,
            items,
            idempotent_replay: false,
        };
        write_audit(
            &mut tx,
            &workspace_id,
            &request.actor_id,
            "outbound_shipment.posted",
            "outbound_shipment",
            &shipment_id,
            &request.request_id,
            json!({"shipment_no": request.shipment_no, "shipped_count": response.shipped_count}),
            &now,
        )
        .await?;
        save_idempotent(
            &mut tx,
            &workspace_id,
            SHIPMENT_SCOPE,
            &request.idempotency_key,
            &digest,
            &response,
            &now,
        )
        .await?;
        tx.commit().await.map_err(commit_error)?;
        Ok(response)
    }

    pub async fn confirm_outbound_delivery(
        &self,
        request: ConfirmOutboundDeliveryRequest,
    ) -> OutboundResult<ConfirmOutboundDeliveryResponse> {
        let request = normalize_delivery(request)?;
        let digest = request_digest(&request)?;
        let workspace_id = self.workspace_id().to_owned();
        let now = now_utc().map_err(OutboundError::Storage)?;
        let mut tx = begin_write(self, &workspace_id).await?;
        if let Some(mut response) = load_idempotent::<ConfirmOutboundDeliveryResponse>(
            &mut tx,
            &workspace_id,
            DELIVERY_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            response.idempotent_replay = true;
            tx.commit().await.map_err(commit_error)?;
            return Ok(response);
        }
        let rows = load_shipment_lines(
            &mut tx,
            &workspace_id,
            &request.shipment_id,
            &request.shipment_line_ids,
        )
        .await?;
        if rows.is_empty() {
            return Err(OutboundError::Conflict(
                "no undelivered shipment lines selected".to_owned(),
            ));
        }
        let confirmation_id = new_id();
        sqlx::query("INSERT INTO delivery_confirmations (id, workspace_id, outbound_shipment_id, confirmation_code, confirmed_by, confirmed_at, notes, idempotency_key, request_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)")
            .bind(&confirmation_id).bind(&workspace_id).bind(&request.shipment_id).bind(&request.confirmation_code).bind(&request.confirmed_by).bind(&request.confirmed_at).bind(&request.notes).bind(&request.idempotency_key).bind(&request.request_id).bind(&now)
            .execute(&mut *tx).await.map_err(|error| unique_or_storage("insert delivery confirmation", error))?;
        for row in &rows {
            let unit_id: String = row.try_get("unit_id").map_err(row_error)?;
            let line_id: String = row.try_get("shipment_line_id").map_err(row_error)?;
            let line_order_id: String = row.try_get("order_line_id").map_err(row_error)?;
            let version: i64 = row.try_get("version").map_err(row_error)?;
            let status: String = row.try_get("inventory_status").map_err(row_error)?;
            if status != "shipped" {
                return Err(OutboundError::Conflict(
                    "shipment line is no longer pending delivery".to_owned(),
                ));
            }
            sqlx::query("INSERT INTO delivery_confirmation_lines (id, workspace_id, delivery_confirmation_id, outbound_shipment_line_id, result, created_at) VALUES (?1, ?2, ?3, ?4, 'accepted', ?5)")
                .bind(new_id()).bind(&workspace_id).bind(&confirmation_id).bind(&line_id).bind(&now).execute(&mut *tx).await.map_err(|error| unique_or_storage("insert delivery confirmation line", error))?;
            let updated = sqlx::query("UPDATE inventory_units SET inventory_status = 'delivered', version = version + 1, updated_at = ?1 WHERE workspace_id = ?2 AND id = ?3 AND version = ?4 AND inventory_status = 'shipped'")
                .bind(&now).bind(&workspace_id).bind(&unit_id).bind(version).execute(&mut *tx).await.map_err(|error| storage("mark unit delivered", error))?;
            if updated.rows_affected() != 1 {
                return Err(OutboundError::Conflict(
                    "inventory changed during delivery confirmation".to_owned(),
                ));
            }
            let line_updated = sqlx::query("UPDATE outbound_order_lines SET delivered_quantity = delivered_quantity + 1 WHERE workspace_id = ?1 AND id = ?2 AND delivered_quantity + 1 <= shipped_quantity")
                .bind(&workspace_id)
                .bind(&line_order_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| storage("update delivered quantity", error))?;
            if line_updated.rows_affected() != 1 {
                return Err(OutboundError::Conflict(
                    "delivery exceeds shipped quantity".to_owned(),
                ));
            }
            sqlx::query("INSERT INTO stock_movements (id, workspace_id, inventory_unit_id, movement_type, source_type, source_id, actor_id, occurred_at, created_at) VALUES (?1, ?2, ?3, 'delivered', 'delivery_confirmation', ?4, ?5, ?6, ?7)")
                .bind(new_id()).bind(&workspace_id).bind(&unit_id).bind(&confirmation_id).bind(&request.confirmed_by).bind(&request.confirmed_at).bind(&now).execute(&mut *tx).await.map_err(|error| storage("insert delivery movement", error))?;
        }
        let shipment_status =
            shipment_status_after_delivery(&mut tx, &workspace_id, &request.shipment_id).await?;
        sqlx::query(
            "UPDATE outbound_shipments SET status = ?1 WHERE workspace_id = ?2 AND id = ?3",
        )
        .bind(&shipment_status)
        .bind(&workspace_id)
        .bind(&request.shipment_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| storage("update shipment delivery status", error))?;
        let order_status =
            order_status_after_delivery(&mut tx, &workspace_id, &request.shipment_id).await?;
        let order_updated = sqlx::query(
            "UPDATE outbound_orders SET status = ?1 WHERE workspace_id = ?2 AND id = (SELECT outbound_order_id FROM outbound_shipments WHERE workspace_id = ?2 AND id = ?3)",
        )
        .bind(&order_status)
        .bind(&workspace_id)
        .bind(&request.shipment_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| storage("update order delivery status", error))?;
        if order_updated.rows_affected() != 1 {
            return Err(OutboundError::NotFound(request.shipment_id.clone()));
        }
        let response = ConfirmOutboundDeliveryResponse {
            confirmation_id: confirmation_id.clone(),
            confirmation_code: request.confirmation_code.clone(),
            delivered_count: rows.len() as u32,
            shipment_status,
            idempotent_replay: false,
        };
        write_audit(&mut tx, &workspace_id, &request.confirmed_by, "delivery_confirmation.created", "delivery_confirmation", &confirmation_id, &request.request_id, json!({"confirmation_code": request.confirmation_code, "delivered_count": response.delivered_count}), &now).await?;
        save_idempotent(
            &mut tx,
            &workspace_id,
            DELIVERY_SCOPE,
            &request.idempotency_key,
            &digest,
            &response,
            &now,
        )
        .await?;
        tx.commit().await.map_err(commit_error)?;
        Ok(response)
    }

    pub async fn return_outbound_shipment(
        &self,
        request: ReturnOutboundShipmentRequest,
    ) -> OutboundResult<ReturnOutboundShipmentResponse> {
        let request = normalize_return(request)?;
        let digest = request_digest(&request)?;
        let workspace_id = self.workspace_id().to_owned();
        let now = now_utc().map_err(OutboundError::Storage)?;
        let mut tx = begin_write(self, &workspace_id).await?;
        if let Some(mut response) = load_idempotent::<ReturnOutboundShipmentResponse>(
            &mut tx,
            &workspace_id,
            RETURN_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            response.idempotent_replay = true;
            tx.commit().await.map_err(commit_error)?;
            return Ok(response);
        }
        let rows = load_returnable_lines(
            &mut tx,
            &workspace_id,
            &request.shipment_id,
            &request.shipment_line_ids,
        )
        .await?;
        if rows.is_empty() {
            return Err(OutboundError::Conflict(
                "no returnable shipment lines selected".to_owned(),
            ));
        }
        let batch_id = new_id();
        sqlx::query("INSERT INTO outbound_return_batches (id, workspace_id, return_no, returned_at, actor_id, idempotency_key, request_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")
            .bind(&batch_id).bind(&workspace_id).bind(&request.return_no).bind(&request.returned_at).bind(&request.actor_id).bind(&request.idempotency_key).bind(&request.request_id).bind(&now).execute(&mut *tx).await.map_err(|error| unique_or_storage("insert outbound return batch", error))?;
        for row in &rows {
            let unit_id: String = row.try_get("unit_id").map_err(row_error)?;
            let line_id: String = row.try_get("shipment_line_id").map_err(row_error)?;
            let version: i64 = row.try_get("version").map_err(row_error)?;
            let status: String = row.try_get("inventory_status").map_err(row_error)?;
            if !matches!(status.as_str(), "shipped" | "delivered") {
                return Err(OutboundError::Conflict(
                    "shipment line is not in a returnable state".to_owned(),
                ));
            }
            sqlx::query("INSERT INTO outbound_return_lines (id, workspace_id, return_batch_id, outbound_shipment_line_id, inventory_unit_id, reason, disposition, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'quarantine', ?7)")
                .bind(new_id()).bind(&workspace_id).bind(&batch_id).bind(&line_id).bind(&unit_id).bind(&request.reason).bind(&now).execute(&mut *tx).await.map_err(|error| unique_or_storage("insert outbound return line", error))?;
            let updated = sqlx::query("UPDATE inventory_units SET inventory_status = 'quarantined', location_id = ?1, version = version + 1, updated_at = ?2 WHERE workspace_id = ?3 AND id = ?4 AND version = ?5 AND inventory_status IN ('shipped', 'delivered')")
                .bind(self.quarantine_location_id()).bind(&now).bind(&workspace_id).bind(&unit_id).bind(version).execute(&mut *tx).await.map_err(|error| storage("quarantine returned unit", error))?;
            if updated.rows_affected() != 1 {
                return Err(OutboundError::Conflict(
                    "inventory changed during return".to_owned(),
                ));
            }
            sqlx::query("INSERT INTO stock_movements (id, workspace_id, inventory_unit_id, movement_type, from_location_id, to_location_id, source_type, source_id, actor_id, occurred_at, created_at) VALUES (?1, ?2, ?3, 'returned', NULL, ?4, 'outbound_return_batch', ?5, ?6, ?7, ?8)")
                .bind(new_id()).bind(&workspace_id).bind(&unit_id).bind(self.quarantine_location_id()).bind(&batch_id).bind(&request.actor_id).bind(&request.returned_at).bind(&now).execute(&mut *tx).await.map_err(|error| storage("insert return movement", error))?;
        }
        let response = ReturnOutboundShipmentResponse {
            return_batch_id: batch_id.clone(),
            return_no: request.return_no.clone(),
            quarantined_count: rows.len() as u32,
            idempotent_replay: false,
        };
        write_audit(&mut tx, &workspace_id, &request.actor_id, "outbound_return.created", "outbound_return_batch", &batch_id, &request.request_id, json!({"return_no": request.return_no, "quarantined_count": response.quarantined_count}), &now).await?;
        save_idempotent(
            &mut tx,
            &workspace_id,
            RETURN_SCOPE,
            &request.idempotency_key,
            &digest,
            &response,
            &now,
        )
        .await?;
        tx.commit().await.map_err(commit_error)?;
        Ok(response)
    }

    pub async fn outbound_order_details(
        &self,
        order_id: &str,
    ) -> OutboundResult<OutboundOrderDetails> {
        let workspace_id = self.workspace_id();
        let order = sqlx::query("SELECT o.order_no, o.status, p.display_name FROM outbound_orders o JOIN business_parties p ON p.id = o.upstream_receiver_id AND p.workspace_id = o.workspace_id WHERE o.workspace_id = ?1 AND o.id = ?2")
            .bind(workspace_id).bind(order_id).fetch_optional(self.pool()).await.map_err(|error| storage("load outbound order", error))?
            .ok_or_else(|| OutboundError::NotFound(order_id.to_owned()))?;
        let order_no: String = order.try_get("order_no").map_err(row_error)?;
        let status: String = order.try_get("status").map_err(row_error)?;
        let receiver_name: String = order.try_get("display_name").map_err(row_error)?;
        let lines = sqlx::query("SELECT l.id, s.code, s.name, l.required_quantity, l.allocated_quantity, l.shipped_quantity, l.delivered_quantity FROM outbound_order_lines l JOIN skus s ON s.id = l.sku_id AND s.workspace_id = l.workspace_id WHERE l.workspace_id = ?1 AND l.outbound_order_id = ?2 ORDER BY l.id")
            .bind(workspace_id).bind(order_id).fetch_all(self.pool()).await.map_err(|error| storage("load outbound order lines", error))?;
        let mut details = Vec::with_capacity(lines.len());
        for line in lines {
            let line_id: String = line.try_get("id").map_err(row_error)?;
            let allocations = sqlx::query("SELECT oa.id AS allocation_id, iu.barcode, iu.owner_party_id, iu.sku_id FROM outbound_allocations oa JOIN inventory_units iu ON iu.id = oa.inventory_unit_id AND iu.workspace_id = oa.workspace_id WHERE oa.workspace_id = ?1 AND oa.outbound_order_line_id = ?2 AND oa.status IN ('active', 'shipped') ORDER BY oa.allocated_at, oa.id")
                .bind(workspace_id).bind(&line_id).fetch_all(self.pool()).await.map_err(|error| storage("load outbound allocations", error))?
                .into_iter().map(|row| Ok(AllocationItem { allocation_id: row.try_get("allocation_id").map_err(row_error)?, barcode: row.try_get("barcode").map_err(row_error)?, owner_party_id: row.try_get("owner_party_id").map_err(row_error)?, sku_id: row.try_get("sku_id").map_err(row_error)? })).collect::<OutboundResult<Vec<_>>>()?;
            details.push(OutboundOrderDetailLine {
                order_line_id: line_id,
                sku_code: line.try_get("code").map_err(row_error)?,
                sku_name: line.try_get("name").map_err(row_error)?,
                required_quantity: to_u32(line.try_get("required_quantity").map_err(row_error)?)?,
                allocated_quantity: to_u32(line.try_get("allocated_quantity").map_err(row_error)?)?,
                shipped_quantity: to_u32(line.try_get("shipped_quantity").map_err(row_error)?)?,
                delivered_quantity: to_u32(line.try_get("delivered_quantity").map_err(row_error)?)?,
                allocations,
            });
        }
        Ok(OutboundOrderDetails {
            order_id: order_id.to_owned(),
            order_no,
            receiver_name,
            status,
            lines: details,
        })
    }
}

async fn load_allocation_rows(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    request: &ShipOutboundRequest,
) -> OutboundResult<Vec<sqlx::sqlite::SqliteRow>> {
    if !request.allocation_ids.is_empty() {
        let mut rows = Vec::with_capacity(request.allocation_ids.len());
        for id in &request.allocation_ids {
            let row = sqlx::query("SELECT oa.id AS allocation_id, oa.outbound_order_line_id AS order_line_id, oa.inventory_unit_id AS unit_id, oa.status AS allocation_status, oo.outbound_order_id, iu.barcode, iu.owner_party_id, iu.sku_id, iu.inventory_status, iu.quality_status, iu.version FROM outbound_allocations oa JOIN outbound_order_lines oo ON oo.id = oa.outbound_order_line_id AND oo.workspace_id = oa.workspace_id JOIN inventory_units iu ON iu.id = oa.inventory_unit_id AND iu.workspace_id = oa.workspace_id WHERE oa.workspace_id = ?1 AND oa.id = ?2 AND oa.status = 'active'")
                .bind(workspace_id).bind(id).fetch_optional(&mut **tx).await.map_err(|error| storage("load selected allocation", error))?.ok_or_else(|| OutboundError::NotFound(format!("allocation {id}")))?;
            rows.push(row);
        }
        ensure_allocation_rows_belong_to_order(&rows, &request.order_id)?;
        return Ok(rows);
    }
    let mut rows = Vec::new();
    for barcode in &request.barcodes {
        let row = sqlx::query("SELECT oa.id AS allocation_id, oa.outbound_order_line_id AS order_line_id, oa.inventory_unit_id AS unit_id, oa.status AS allocation_status, oo.outbound_order_id, iu.barcode, iu.owner_party_id, iu.sku_id, iu.inventory_status, iu.quality_status, iu.version FROM outbound_allocations oa JOIN outbound_order_lines oo ON oo.id = oa.outbound_order_line_id AND oo.workspace_id = oa.workspace_id JOIN inventory_units iu ON iu.id = oa.inventory_unit_id AND iu.workspace_id = oa.workspace_id WHERE oa.workspace_id = ?1 AND iu.barcode = ?2 AND oa.status = 'active'")
            .bind(workspace_id).bind(barcode).fetch_optional(&mut **tx).await.map_err(|error| storage("load barcode allocation", error))?.ok_or_else(|| OutboundError::NotFound(format!("active allocation for {barcode}")))?;
        rows.push(row);
    }
    ensure_allocation_rows_belong_to_order(&rows, &request.order_id)?;
    Ok(rows)
}

fn ensure_allocation_rows_belong_to_order(
    rows: &[sqlx::sqlite::SqliteRow],
    order_id: &str,
) -> OutboundResult<()> {
    for row in rows {
        let row_order_id: String = row.try_get("outbound_order_id").map_err(row_error)?;
        if row_order_id != order_id {
            return Err(OutboundError::Conflict(
                "allocation does not belong to the requested order".to_owned(),
            ));
        }
    }
    Ok(())
}

async fn load_shipment_lines(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    shipment_id: &str,
    selected: &[String],
) -> OutboundResult<Vec<sqlx::sqlite::SqliteRow>> {
    let query = "SELECT osl.id AS shipment_line_id, osl.inventory_unit_id AS unit_id, osl.outbound_allocation_id AS allocation_id, oa.outbound_order_line_id AS order_line_id, iu.inventory_status, iu.version FROM outbound_shipment_lines osl JOIN outbound_allocations oa ON oa.id = osl.outbound_allocation_id AND oa.workspace_id = osl.workspace_id JOIN inventory_units iu ON iu.id = osl.inventory_unit_id AND iu.workspace_id = osl.workspace_id WHERE osl.workspace_id = ?1 AND osl.outbound_shipment_id = ?2 AND iu.inventory_status = 'shipped' AND NOT EXISTS (SELECT 1 FROM delivery_confirmation_lines dcl WHERE dcl.outbound_shipment_line_id = osl.id AND dcl.workspace_id = osl.workspace_id)";
    let rows = sqlx::query(query)
        .bind(workspace_id)
        .bind(shipment_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| storage("load delivery shipment lines", error))?;
    if selected.is_empty() {
        return Ok(rows);
    }
    let selected_set: HashSet<&str> = selected.iter().map(String::as_str).collect();
    let filtered: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            row.try_get::<String, _>("shipment_line_id")
                .map(|id| selected_set.contains(id.as_str()))
                .unwrap_or(false)
        })
        .collect();
    if filtered.len() != selected_set.len() {
        return Err(OutboundError::NotFound(
            "one or more selected shipment lines are not pending delivery".to_owned(),
        ));
    }
    Ok(filtered)
}

async fn load_returnable_lines(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    shipment_id: &str,
    selected: &[String],
) -> OutboundResult<Vec<sqlx::sqlite::SqliteRow>> {
    let rows = sqlx::query("SELECT osl.id AS shipment_line_id, osl.inventory_unit_id AS unit_id, iu.inventory_status, iu.version FROM outbound_shipment_lines osl JOIN inventory_units iu ON iu.id = osl.inventory_unit_id AND iu.workspace_id = osl.workspace_id WHERE osl.workspace_id = ?1 AND osl.outbound_shipment_id = ?2 AND iu.inventory_status IN ('shipped', 'delivered') AND NOT EXISTS (SELECT 1 FROM outbound_return_lines rl WHERE rl.outbound_shipment_line_id = osl.id AND rl.workspace_id = osl.workspace_id)")
        .bind(workspace_id).bind(shipment_id).fetch_all(&mut **tx).await.map_err(|error| storage("load returnable shipment lines", error))?;
    if selected.is_empty() {
        return Ok(rows);
    }
    let selected_set: HashSet<&str> = selected.iter().map(String::as_str).collect();
    let filtered: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            row.try_get::<String, _>("shipment_line_id")
                .map(|id| selected_set.contains(id.as_str()))
                .unwrap_or(false)
        })
        .collect();
    if filtered.len() != selected_set.len() {
        return Err(OutboundError::NotFound(
            "one or more selected shipment lines are not returnable".to_owned(),
        ));
    }
    Ok(filtered)
}

async fn order_status_after_ship(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    order_id: &str,
) -> OutboundResult<String> {
    let row = sqlx::query("SELECT COUNT(*) AS total, SUM(CASE WHEN shipped_quantity >= required_quantity THEN 1 ELSE 0 END) AS shipped FROM outbound_order_lines WHERE workspace_id = ?1 AND outbound_order_id = ?2")
        .bind(workspace_id).bind(order_id).fetch_one(&mut **tx).await.map_err(|error| storage("calculate order shipment status", error))?;
    let total: i64 = row.try_get("total").map_err(row_error)?;
    let shipped: i64 = row
        .try_get::<Option<i64>, _>("shipped")
        .map_err(row_error)?
        .unwrap_or_default();
    Ok(if total > 0 && shipped >= total {
        "shipped"
    } else {
        "partially_shipped"
    }
    .to_owned())
}

async fn shipment_status_after_delivery(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    shipment_id: &str,
) -> OutboundResult<String> {
    let row = sqlx::query("SELECT COUNT(*) AS total, (SELECT COUNT(*) FROM delivery_confirmation_lines dcl JOIN outbound_shipment_lines osl ON osl.id = dcl.outbound_shipment_line_id WHERE dcl.workspace_id = ?1 AND osl.outbound_shipment_id = ?2) AS delivered FROM outbound_shipment_lines WHERE workspace_id = ?1 AND outbound_shipment_id = ?2")
        .bind(workspace_id).bind(shipment_id).fetch_one(&mut **tx).await.map_err(|error| storage("calculate shipment delivery status", error))?;
    let total: i64 = row.try_get("total").map_err(row_error)?;
    let delivered: i64 = row.try_get("delivered").map_err(row_error)?;
    Ok(if total > 0 && delivered >= total {
        "delivered"
    } else {
        "partially_delivered"
    }
    .to_owned())
}

async fn order_status_after_delivery(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    shipment_id: &str,
) -> OutboundResult<String> {
    let row = sqlx::query(
        "SELECT o.status, COUNT(l.id) AS total, SUM(CASE WHEN l.delivered_quantity >= l.required_quantity THEN 1 ELSE 0 END) AS delivered FROM outbound_orders o JOIN outbound_shipments s ON s.outbound_order_id = o.id AND s.workspace_id = o.workspace_id JOIN outbound_order_lines l ON l.outbound_order_id = o.id AND l.workspace_id = o.workspace_id WHERE o.workspace_id = ?1 AND s.id = ?2 GROUP BY o.id, o.status",
    )
    .bind(workspace_id)
    .bind(shipment_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| storage("calculate order delivery status", error))?
    .ok_or_else(|| OutboundError::NotFound(shipment_id.to_owned()))?;
    let current: String = row.try_get("status").map_err(row_error)?;
    let total: i64 = row.try_get("total").map_err(row_error)?;
    let delivered: i64 = row
        .try_get::<Option<i64>, _>("delivered")
        .map_err(row_error)?
        .unwrap_or_default();
    if total > 0 && delivered >= total {
        Ok("completed".to_owned())
    } else {
        Ok(current)
    }
}

async fn begin_write<'a>(
    db: &'a OfflineDatabase,
    workspace_id: &str,
) -> OutboundResult<Transaction<'a, Sqlite>> {
    let mut tx = db
        .pool()
        .begin()
        .await
        .map_err(|error| storage("begin outbound transaction", error))?;
    let read_only: i64 = sqlx::query_scalar("SELECT read_only FROM workspaces WHERE id = ?1")
        .bind(workspace_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| storage("read workspace write mode", error))?;
    if read_only != 0 {
        return Err(OutboundError::Conflict(
            "offline workspace is archived and read-only".to_owned(),
        ));
    }
    Ok(tx)
}

async fn upsert_party(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    display_name: &str,
    role: &str,
    now: &str,
) -> OutboundResult<String> {
    let normalized = display_name
        .trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if normalized.is_empty() {
        return Err(OutboundError::Invalid(
            "party name must not be empty".to_owned(),
        ));
    }
    if let Some(id) = sqlx::query_scalar::<_, String>(
        "SELECT id FROM business_parties WHERE workspace_id = ?1 AND normalized_name = ?2",
    )
    .bind(workspace_id)
    .bind(&normalized)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| storage("lookup outbound party", error))?
    {
        sqlx::query("INSERT OR IGNORE INTO party_roles (workspace_id, party_id, role, created_at) VALUES (?1, ?2, ?3, ?4)").bind(workspace_id).bind(&id).bind(role).bind(now).execute(&mut **tx).await.map_err(|error| storage("ensure outbound party role", error))?;
        return Ok(id);
    }
    let id = new_id();
    sqlx::query("INSERT INTO business_parties (id, workspace_id, normalized_name, display_name, created_at) VALUES (?1, ?2, ?3, ?4, ?5)").bind(&id).bind(workspace_id).bind(&normalized).bind(display_name.trim()).bind(now).execute(&mut **tx).await.map_err(|error| storage("insert outbound party", error))?;
    sqlx::query("INSERT INTO party_roles (workspace_id, party_id, role, created_at) VALUES (?1, ?2, ?3, ?4)").bind(workspace_id).bind(&id).bind(role).bind(now).execute(&mut **tx).await.map_err(|error| storage("insert outbound party role", error))?;
    Ok(id)
}

async fn upsert_sku(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    code: &str,
    name: &str,
    now: &str,
) -> OutboundResult<String> {
    let code = code.trim();
    let name = name.trim();
    if code.is_empty() || name.is_empty() {
        return Err(OutboundError::Invalid(
            "SKU code and name must not be empty".to_owned(),
        ));
    }
    if let Some(id) =
        sqlx::query_scalar::<_, String>("SELECT id FROM skus WHERE workspace_id = ?1 AND code = ?2")
            .bind(workspace_id)
            .bind(code)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| storage("lookup outbound SKU", error))?
    {
        return Ok(id);
    }
    let id = new_id();
    sqlx::query("INSERT INTO skus (id, workspace_id, code, name, tracking_mode, active, created_at) VALUES (?1, ?2, ?3, ?4, 'serial', 1, ?5)").bind(&id).bind(workspace_id).bind(code).bind(name).bind(now).execute(&mut **tx).await.map_err(|error| storage("insert outbound SKU", error))?;
    Ok(id)
}

async fn write_audit(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    actor_id: &str,
    action: &str,
    entity_type: &str,
    entity_id: &str,
    request_id: &str,
    details: Value,
    now: &str,
) -> OutboundResult<()> {
    sqlx::query("INSERT INTO audit_logs (id, workspace_id, actor_id, action, entity_type, entity_id, request_id, result, details_json, occurred_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'success', ?8, ?9)").bind(new_id()).bind(workspace_id).bind(actor_id).bind(action).bind(entity_type).bind(entity_id).bind(request_id).bind(serde_json::to_string(&details).map_err(|error| OutboundError::Storage(error.to_string()))?).bind(now).execute(&mut **tx).await.map_err(|error| storage("write outbound audit", error))?;
    Ok(())
}

async fn load_idempotent<T: DeserializeOwned>(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    scope: &str,
    key: &str,
    digest: &str,
) -> OutboundResult<Option<T>> {
    let row = sqlx::query("SELECT request_hash, response_json FROM idempotency_records WHERE workspace_id = ?1 AND scope = ?2 AND idempotency_key = ?3").bind(workspace_id).bind(scope).bind(key).fetch_optional(&mut **tx).await.map_err(|error| storage("load outbound idempotency record", error))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored: String = row.try_get("request_hash").map_err(row_error)?;
    if stored != digest {
        return Err(OutboundError::Conflict(
            "idempotency key was reused with different request data".to_owned(),
        ));
    }
    let body: String = row.try_get("response_json").map_err(row_error)?;
    serde_json::from_str(&body)
        .map(Some)
        .map_err(|error| OutboundError::Storage(format!("decode idempotent response: {error}")))
}

async fn save_idempotent<T: Serialize>(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    scope: &str,
    key: &str,
    digest: &str,
    response: &T,
    now: &str,
) -> OutboundResult<()> {
    let body = serde_json::to_string(response)
        .map_err(|error| OutboundError::Storage(error.to_string()))?;
    sqlx::query("INSERT INTO idempotency_records (id, workspace_id, scope, idempotency_key, request_hash, response_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)").bind(new_id()).bind(workspace_id).bind(scope).bind(key).bind(digest).bind(body).bind(now).execute(&mut **tx).await.map_err(|error| unique_or_storage("save outbound idempotency record", error))?;
    Ok(())
}

fn request_digest<T: Serialize>(request: &T) -> OutboundResult<String> {
    let bytes =
        serde_json::to_vec(request).map_err(|error| OutboundError::Storage(error.to_string()))?;
    Ok(hex_digest(&bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn new_id() -> String {
    Uuid::now_v7().to_string()
}
fn to_u32(value: i64) -> OutboundResult<u32> {
    u32::try_from(value)
        .map_err(|_| OutboundError::Storage("negative or overflowing quantity".to_owned()))
}
fn parse_quality_status(value: &str) -> OutboundResult<QualityStatus> {
    match value {
        "untested" => Ok(QualityStatus::Untested),
        "testing" => Ok(QualityStatus::Testing),
        "passed" => Ok(QualityStatus::Passed),
        "failed" => Ok(QualityStatus::Failed),
        "waived" => Ok(QualityStatus::Waived),
        other => Err(OutboundError::Storage(format!(
            "unknown quality status {other}"
        ))),
    }
}
fn row_error(error: sqlx::Error) -> OutboundError {
    storage("decode outbound row", error)
}
fn storage(context: &str, error: impl std::fmt::Display) -> OutboundError {
    OutboundError::Storage(format!("{context}: {error}"))
}
fn commit_error(error: sqlx::Error) -> OutboundError {
    storage("commit outbound transaction", error)
}
fn unique_or_storage(context: &str, error: sqlx::Error) -> OutboundError {
    if error
        .as_database_error()
        .is_some_and(|db| db.is_unique_violation())
    {
        OutboundError::Conflict(format!("{context}: unique constraint violation"))
    } else {
        storage(context, error)
    }
}

fn required_text(field: &str, value: String) -> OutboundResult<String> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        Err(OutboundError::Invalid(format!("{field} must not be empty")))
    } else {
        Ok(value)
    }
}
fn normalize_create_order(
    mut request: CreateOutboundOrderRequest,
) -> OutboundResult<CreateOutboundOrderRequest> {
    request.request_id = required_text("request_id", request.request_id)?;
    request.idempotency_key = required_text("idempotency_key", request.idempotency_key)?;
    request.order_no = required_text("order_no", request.order_no)?;
    request.upstream_receiver_name =
        required_text("upstream_receiver_name", request.upstream_receiver_name)?;
    request.sku_code = required_text("sku_code", request.sku_code)?;
    request.sku_name = required_text("sku_name", request.sku_name)?;
    request.actor_id = required_text("actor_id", request.actor_id)?;
    if request.required_quantity == 0 {
        return Err(OutboundError::Invalid(
            "required_quantity must be greater than zero".to_owned(),
        ));
    }
    Ok(request)
}
fn normalize_allocate(
    mut request: AllocateOutboundRequest,
) -> OutboundResult<AllocateOutboundRequest> {
    request.request_id = required_text("request_id", request.request_id)?;
    request.idempotency_key = required_text("idempotency_key", request.idempotency_key)?;
    request.order_id = required_text("order_id", request.order_id)?;
    request.order_line_id = required_text("order_line_id", request.order_line_id)?;
    request.actor_id = required_text("actor_id", request.actor_id)?;
    request.barcodes = request
        .barcodes
        .into_iter()
        .map(|value| required_text("barcode", value))
        .collect::<OutboundResult<Vec<_>>>()?;
    Ok(request)
}
fn normalize_ship(mut request: ShipOutboundRequest) -> OutboundResult<ShipOutboundRequest> {
    request.request_id = required_text("request_id", request.request_id)?;
    request.idempotency_key = required_text("idempotency_key", request.idempotency_key)?;
    request.order_id = required_text("order_id", request.order_id)?;
    request.shipment_no = required_text("shipment_no", request.shipment_no)?;
    request.actor_id = required_text("actor_id", request.actor_id)?;
    request.shipped_at = required_text("shipped_at", request.shipped_at)?;
    if request.allocation_ids.is_empty() && request.barcodes.is_empty() {
        return Err(OutboundError::Invalid(
            "select allocations or barcodes".to_owned(),
        ));
    }
    Ok(request)
}
fn normalize_delivery(
    mut request: ConfirmOutboundDeliveryRequest,
) -> OutboundResult<ConfirmOutboundDeliveryRequest> {
    request.request_id = required_text("request_id", request.request_id)?;
    request.idempotency_key = required_text("idempotency_key", request.idempotency_key)?;
    request.shipment_id = required_text("shipment_id", request.shipment_id)?;
    request.confirmation_code = required_text("confirmation_code", request.confirmation_code)?;
    request.confirmed_at = required_text("confirmed_at", request.confirmed_at)?;
    request.confirmed_by = required_text("confirmed_by", request.confirmed_by)?;
    Ok(request)
}
fn normalize_return(
    mut request: ReturnOutboundShipmentRequest,
) -> OutboundResult<ReturnOutboundShipmentRequest> {
    request.request_id = required_text("request_id", request.request_id)?;
    request.idempotency_key = required_text("idempotency_key", request.idempotency_key)?;
    request.shipment_id = required_text("shipment_id", request.shipment_id)?;
    request.return_no = required_text("return_no", request.return_no)?;
    request.returned_at = required_text("returned_at", request.returned_at)?;
    request.reason = required_text("reason", request.reason)?;
    request.actor_id = required_text("actor_id", request.actor_id)?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::application::{
        CompleteInspectionRequest, InspectionResultInput, PostReceiptRequest,
    };
    use crate::v2::domain::{InspectionKind, QualityOutcome};
    use serde_json::json;

    fn receipt(request_id: &str, key: &str, owner: &str, barcode: &str) -> PostReceiptRequest {
        PostReceiptRequest {
            request_id: request_id.to_owned(),
            idempotency_key: key.to_owned(),
            receipt_no: format!("R-{request_id}"),
            owner_name: owner.to_owned(),
            sku_code: "SKU-X".to_owned(),
            sku_name: "Model X".to_owned(),
            source_reference: None,
            received_at: "2026-08-01T01:00:00Z".to_owned(),
            actor_id: "actor".to_owned(),
            barcodes: vec![barcode.to_owned()],
            notes: None,
        }
    }

    #[tokio::test]
    async fn two_owners_can_fill_one_order_and_shipped_units_remain_traceable() {
        let path =
            std::env::temp_dir().join(format!("inventory-outbound-test-{}.sqlite", Uuid::now_v7()));
        let database = OfflineDatabase::open(&path).await.expect("open database");
        database
            .post_receipt(receipt("one", "receipt-one", "Owner A", "A-1"))
            .await
            .expect("receipt a");
        database
            .post_receipt(receipt("two", "receipt-two", "Owner B", "B-1"))
            .await
            .expect("receipt b");
        for (request_id, key, barcode) in
            [("q1", "quality-one", "A-1"), ("q2", "quality-two", "B-1")]
        {
            database
                .complete_inspection(CompleteInspectionRequest {
                    request_id: request_id.to_owned(),
                    idempotency_key: key.to_owned(),
                    inspection_no: format!("Q-{request_id}"),
                    inspection_kind: InspectionKind::Initial,
                    inspector_id: "qc".to_owned(),
                    inspected_at: "2026-08-01T01:01:00Z".to_owned(),
                    results: vec![InspectionResultInput {
                        barcode: barcode.to_owned(),
                        outcome: QualityOutcome::Passed,
                        defect_code: None,
                        measurements: json!({}),
                        notes: None,
                    }],
                })
                .await
                .expect("inspection");
        }
        let order = database
            .create_outbound_order(CreateOutboundOrderRequest {
                request_id: "order".to_owned(),
                idempotency_key: "order-key".to_owned(),
                order_no: "O-1".to_owned(),
                upstream_receiver_name: "Upstream".to_owned(),
                sku_code: "SKU-X".to_owned(),
                sku_name: "Model X".to_owned(),
                required_quantity: 2,
                required_at: None,
                actor_id: "operator".to_owned(),
            })
            .await
            .expect("order");
        let allocated = database
            .allocate_outbound_order(AllocateOutboundRequest {
                request_id: "alloc".to_owned(),
                idempotency_key: "alloc-key".to_owned(),
                order_id: order.order_id.clone(),
                order_line_id: order.order_line_id.clone(),
                barcodes: Vec::new(),
                actor_id: "operator".to_owned(),
            })
            .await
            .expect("allocate");
        assert_eq!(allocated.allocated_count, 2);
        assert_eq!(
            allocated
                .allocations
                .iter()
                .map(|item| item.owner_party_id.clone())
                .collect::<HashSet<_>>()
                .len(),
            2
        );
        let shipment = database
            .ship_outbound_order(ShipOutboundRequest {
                request_id: "ship".to_owned(),
                idempotency_key: "ship-key".to_owned(),
                order_id: order.order_id.clone(),
                shipment_no: "S-1".to_owned(),
                allocation_ids: allocated
                    .allocations
                    .iter()
                    .map(|item| item.allocation_id.clone())
                    .collect(),
                barcodes: Vec::new(),
                shipped_at: "2026-08-01T01:02:00Z".to_owned(),
                actor_id: "operator".to_owned(),
            })
            .await
            .expect("ship");
        assert_eq!(shipment.shipped_count, 2);
        let delivery = database
            .confirm_outbound_delivery(ConfirmOutboundDeliveryRequest {
                request_id: "delivery".to_owned(),
                idempotency_key: "delivery-key".to_owned(),
                shipment_id: shipment.shipment_id.clone(),
                confirmation_code: "UPSTREAM-1".to_owned(),
                shipment_line_ids: Vec::new(),
                confirmed_at: "2026-08-01T01:03:00Z".to_owned(),
                confirmed_by: "receiver".to_owned(),
                notes: None,
            })
            .await
            .expect("delivery");
        assert_eq!(delivery.delivered_count, 2);
        assert_eq!(delivery.shipment_status, "delivered");
        let returned = database
            .return_outbound_shipment(ReturnOutboundShipmentRequest {
                request_id: "return".to_owned(),
                idempotency_key: "return-key".to_owned(),
                shipment_id: shipment.shipment_id.clone(),
                shipment_line_ids: vec![shipment.items[0].shipment_line_id.clone()],
                return_no: "RT-1".to_owned(),
                returned_at: "2026-08-01T01:04:00Z".to_owned(),
                reason: "上游拒收".to_owned(),
                actor_id: "operator".to_owned(),
            })
            .await
            .expect("return");
        assert_eq!(returned.quarantined_count, 1);
        let returned_status: String = sqlx::query_scalar(
            "SELECT inventory_status FROM inventory_units WHERE workspace_id = ?1 AND barcode = ?2",
        )
        .bind(database.workspace_id())
        .bind(&shipment.items[0].barcode)
        .fetch_one(database.pool())
        .await
        .expect("returned unit status");
        assert_eq!(returned_status, "quarantined");
        let details = database
            .outbound_order_details(&order.order_id)
            .await
            .expect("details");
        assert_eq!(details.lines[0].shipped_quantity, 2);
        assert_eq!(details.status, "completed");
        let _ = std::fs::remove_file(path);
    }
}
