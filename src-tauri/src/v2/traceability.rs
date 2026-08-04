//! Read-only inventory provenance projected from immutable business facts.

use super::network::{NetworkResult, NetworkService, PERMISSION_INVENTORY_READ};
use super::sqlite::OfflineDatabase;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InventoryTrace {
    pub inventory_unit_id: String,
    pub barcode: String,
    pub owner_party_id: String,
    pub owner_name: String,
    pub sku_id: String,
    pub sku_code: String,
    pub sku_name: String,
    pub receipt_id: String,
    pub receipt_no: String,
    pub received_at: String,
    pub inventory_status: String,
    pub quality_status: String,
    pub inspections: Vec<InspectionTrace>,
    pub outbound: Vec<OutboundTrace>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InspectionTrace {
    pub inspection_no: String,
    pub inspection_type: String,
    pub result: String,
    pub inspected_at: String,
    pub defect_code: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutboundTrace {
    pub allocation_id: String,
    pub allocation_status: String,
    pub allocated_at: String,
    pub order_id: String,
    pub order_no: String,
    pub upstream_receiver_name: String,
    pub shipment_id: Option<String>,
    pub shipment_no: Option<String>,
    pub shipped_at: Option<String>,
    pub confirmation_code: Option<String>,
    pub confirmed_at: Option<String>,
    pub delivery_result: Option<String>,
    pub return_no: Option<String>,
    pub returned_at: Option<String>,
    pub return_reason: Option<String>,
    pub return_disposition: Option<String>,
}

impl OfflineDatabase {
    pub async fn inventory_trace(&self, barcode: &str) -> Result<InventoryTrace, String> {
        let barcode = normalized_barcode(barcode)?;
        let row = sqlx::query(
            r#"
            SELECT iu.id AS inventory_unit_id, iu.barcode,
                   iu.owner_party_id, owner.display_name AS owner_name,
                   iu.sku_id, sku.code AS sku_code, sku.name AS sku_name,
                   receipt.id AS receipt_id, receipt.receipt_no, iu.received_at,
                   iu.inventory_status, iu.quality_status
              FROM inventory_units iu
              JOIN business_parties owner ON owner.id = iu.owner_party_id
              JOIN skus sku ON sku.id = iu.sku_id
              JOIN inbound_receipt_lines line ON line.id = iu.inbound_receipt_line_id
              JOIN inbound_receipts receipt ON receipt.id = line.receipt_id
             WHERE iu.workspace_id = ?1 AND iu.barcode = ?2
            "#,
        )
        .bind(self.workspace_id())
        .bind(&barcode)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| format!("读取库存追溯失败: {error}"))?
        .ok_or_else(|| format!("找不到条码 {barcode}"))?;
        let inventory_unit_id: String = row
            .try_get("inventory_unit_id")
            .map_err(|error| error.to_string())?;

        let inspections = sqlx::query(
            r#"
            SELECT inspection.inspection_no, inspection.inspection_type,
                   result.result, inspection.inspected_at,
                   result.defect_code, result.notes
              FROM quality_inspection_results result
              JOIN quality_inspections inspection ON inspection.id = result.inspection_id
             WHERE result.workspace_id = ?1 AND result.inventory_unit_id = ?2
             ORDER BY inspection.inspected_at, inspection.id
            "#,
        )
        .bind(self.workspace_id())
        .bind(&inventory_unit_id)
        .fetch_all(self.pool())
        .await
        .map_err(|error| format!("读取质检追溯失败: {error}"))?
        .into_iter()
        .map(|row| {
            Ok(InspectionTrace {
                inspection_no: row.try_get("inspection_no")?,
                inspection_type: row.try_get("inspection_type")?,
                result: row.try_get("result")?,
                inspected_at: row.try_get("inspected_at")?,
                defect_code: row.try_get("defect_code")?,
                notes: row.try_get("notes")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(|error| error.to_string())?;

        let outbound = sqlx::query(
            r#"
            SELECT allocation.id AS allocation_id,
                   allocation.status AS allocation_status,
                   allocation.allocated_at,
                   orders.id AS order_id, orders.order_no,
                   receiver.display_name AS upstream_receiver_name,
                   shipment.id AS shipment_id, shipment.shipment_no,
                   shipment.shipped_at,
                   confirmation.confirmation_code, confirmation.confirmed_at,
                   confirmation_line.result AS delivery_result,
                   return_batch.return_no, return_batch.returned_at,
                   return_line.reason AS return_reason,
                   return_line.disposition AS return_disposition
              FROM outbound_allocations allocation
              JOIN outbound_order_lines order_line
                ON order_line.id = allocation.outbound_order_line_id
              JOIN outbound_orders orders ON orders.id = order_line.outbound_order_id
              JOIN business_parties receiver ON receiver.id = orders.upstream_receiver_id
              LEFT JOIN outbound_shipment_lines shipment_line
                ON shipment_line.outbound_allocation_id = allocation.id
              LEFT JOIN outbound_shipments shipment
                ON shipment.id = shipment_line.outbound_shipment_id
              LEFT JOIN delivery_confirmation_lines confirmation_line
                ON confirmation_line.outbound_shipment_line_id = shipment_line.id
              LEFT JOIN delivery_confirmations confirmation
                ON confirmation.id = confirmation_line.delivery_confirmation_id
              LEFT JOIN outbound_return_lines return_line
                ON return_line.outbound_shipment_line_id = shipment_line.id
              LEFT JOIN outbound_return_batches return_batch
                ON return_batch.id = return_line.return_batch_id
             WHERE allocation.workspace_id = ?1
               AND allocation.inventory_unit_id = ?2
             ORDER BY allocation.allocated_at, allocation.id
            "#,
        )
        .bind(self.workspace_id())
        .bind(&inventory_unit_id)
        .fetch_all(self.pool())
        .await
        .map_err(|error| format!("读取出库追溯失败: {error}"))?
        .into_iter()
        .map(outbound_from_sqlite_row)
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(|error| error.to_string())?;

        Ok(InventoryTrace {
            inventory_unit_id,
            barcode: row.try_get("barcode").map_err(|error| error.to_string())?,
            owner_party_id: row
                .try_get("owner_party_id")
                .map_err(|error| error.to_string())?,
            owner_name: row
                .try_get("owner_name")
                .map_err(|error| error.to_string())?,
            sku_id: row.try_get("sku_id").map_err(|error| error.to_string())?,
            sku_code: row.try_get("sku_code").map_err(|error| error.to_string())?,
            sku_name: row.try_get("sku_name").map_err(|error| error.to_string())?,
            receipt_id: row
                .try_get("receipt_id")
                .map_err(|error| error.to_string())?,
            receipt_no: row
                .try_get("receipt_no")
                .map_err(|error| error.to_string())?,
            received_at: row
                .try_get("received_at")
                .map_err(|error| error.to_string())?,
            inventory_status: row
                .try_get("inventory_status")
                .map_err(|error| error.to_string())?,
            quality_status: row
                .try_get("quality_status")
                .map_err(|error| error.to_string())?,
            inspections,
            outbound,
        })
    }
}

impl NetworkService {
    pub async fn inventory_trace(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        barcode: &str,
    ) -> NetworkResult<InventoryTrace> {
        let barcode =
            normalized_barcode(barcode).map_err(super::network::NetworkServiceError::Invalid)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_INVENTORY_READ)
            .await?;
        let transaction = authorized.sqlx_transaction();
        let row = sqlx::query(
            r#"
            SELECT iu.id AS inventory_unit_id, iu.barcode,
                   iu.owner_party_id, owner.display_name AS owner_name,
                   iu.sku_id, sku.code AS sku_code, sku.name AS sku_name,
                   receipt.id AS receipt_id, receipt.receipt_no,
                   iu.received_at::text AS received_at,
                   iu.inventory_status, iu.quality_status
              FROM inventory_units iu
              JOIN business_parties owner
                ON owner.tenant_id = iu.tenant_id AND owner.id = iu.owner_party_id
              JOIN skus sku ON sku.tenant_id = iu.tenant_id AND sku.id = iu.sku_id
              JOIN inbound_receipt_lines line
                ON line.tenant_id = iu.tenant_id AND line.id = iu.inbound_receipt_line_id
              JOIN inbound_receipts receipt
                ON receipt.tenant_id = line.tenant_id AND receipt.id = line.receipt_id
             WHERE iu.tenant_id = $1 AND iu.barcode = $2
            "#,
        )
        .bind(tenant_id)
        .bind(&barcode)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| {
            super::network::NetworkServiceError::Invalid(format!("unknown barcode {barcode}"))
        })?;
        let inventory_unit_id: Uuid = row.try_get("inventory_unit_id")?;

        let inspections = sqlx::query(
            r#"
            SELECT inspection.inspection_no, inspection.inspection_type,
                   result.result, inspection.inspected_at::text AS inspected_at,
                   result.defect_code, result.notes
              FROM quality_inspection_results result
              JOIN quality_inspections inspection
                ON inspection.tenant_id = result.tenant_id
               AND inspection.id = result.inspection_id
             WHERE result.tenant_id = $1 AND result.inventory_unit_id = $2
             ORDER BY inspection.inspected_at, inspection.id
            "#,
        )
        .bind(tenant_id)
        .bind(inventory_unit_id)
        .fetch_all(&mut **transaction)
        .await?
        .into_iter()
        .map(|row| {
            Ok(InspectionTrace {
                inspection_no: row.try_get("inspection_no")?,
                inspection_type: row.try_get("inspection_type")?,
                result: row.try_get("result")?,
                inspected_at: row.try_get("inspected_at")?,
                defect_code: row.try_get("defect_code")?,
                notes: row.try_get("notes")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let outbound = sqlx::query(
            r#"
            SELECT allocation.id AS allocation_id,
                   allocation.status AS allocation_status,
                   allocation.allocated_at::text AS allocated_at,
                   orders.id AS order_id, orders.order_no,
                   receiver.display_name AS upstream_receiver_name,
                   shipment.id AS shipment_id, shipment.shipment_no,
                   shipment.shipped_at::text AS shipped_at,
                   confirmation.confirmation_code,
                   confirmation.confirmed_at::text AS confirmed_at,
                   confirmation_line.result AS delivery_result,
                   return_batch.return_no,
                   return_batch.returned_at::text AS returned_at,
                   return_line.reason AS return_reason,
                   return_line.disposition AS return_disposition
              FROM outbound_allocations allocation
              JOIN outbound_order_lines order_line
                ON order_line.tenant_id = allocation.tenant_id
               AND order_line.id = allocation.outbound_order_line_id
              JOIN outbound_orders orders
                ON orders.tenant_id = order_line.tenant_id
               AND orders.id = order_line.outbound_order_id
              JOIN business_parties receiver
                ON receiver.tenant_id = orders.tenant_id
               AND receiver.id = orders.upstream_receiver_id
              LEFT JOIN outbound_shipment_lines shipment_line
                ON shipment_line.tenant_id = allocation.tenant_id
               AND shipment_line.outbound_allocation_id = allocation.id
              LEFT JOIN outbound_shipments shipment
                ON shipment.tenant_id = shipment_line.tenant_id
               AND shipment.id = shipment_line.outbound_shipment_id
              LEFT JOIN delivery_confirmation_lines confirmation_line
                ON confirmation_line.tenant_id = shipment_line.tenant_id
               AND confirmation_line.outbound_shipment_line_id = shipment_line.id
              LEFT JOIN delivery_confirmations confirmation
                ON confirmation.tenant_id = confirmation_line.tenant_id
               AND confirmation.id = confirmation_line.delivery_confirmation_id
              LEFT JOIN outbound_return_lines return_line
                ON return_line.tenant_id = shipment_line.tenant_id
               AND return_line.outbound_shipment_line_id = shipment_line.id
              LEFT JOIN outbound_return_batches return_batch
                ON return_batch.tenant_id = return_line.tenant_id
               AND return_batch.id = return_line.return_batch_id
             WHERE allocation.tenant_id = $1
               AND allocation.inventory_unit_id = $2
             ORDER BY allocation.allocated_at, allocation.id
            "#,
        )
        .bind(tenant_id)
        .bind(inventory_unit_id)
        .fetch_all(&mut **transaction)
        .await?
        .into_iter()
        .map(outbound_from_postgres_row)
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let trace = InventoryTrace {
            inventory_unit_id: inventory_unit_id.to_string(),
            barcode: row.try_get("barcode")?,
            owner_party_id: row.try_get::<Uuid, _>("owner_party_id")?.to_string(),
            owner_name: row.try_get("owner_name")?,
            sku_id: row.try_get::<Uuid, _>("sku_id")?.to_string(),
            sku_code: row.try_get("sku_code")?,
            sku_name: row.try_get("sku_name")?,
            receipt_id: row.try_get::<Uuid, _>("receipt_id")?.to_string(),
            receipt_no: row.try_get("receipt_no")?,
            received_at: row.try_get("received_at")?,
            inventory_status: row.try_get("inventory_status")?,
            quality_status: row.try_get("quality_status")?,
            inspections,
            outbound,
        };
        authorized.commit().await?;
        Ok(trace)
    }
}

fn normalized_barcode(barcode: &str) -> Result<String, String> {
    let barcode = barcode.trim().to_uppercase();
    if barcode.is_empty() {
        Err("barcode must not be empty".to_owned())
    } else {
        Ok(barcode)
    }
}

fn outbound_from_sqlite_row(row: sqlx::sqlite::SqliteRow) -> Result<OutboundTrace, sqlx::Error> {
    Ok(OutboundTrace {
        allocation_id: row.try_get("allocation_id")?,
        allocation_status: row.try_get("allocation_status")?,
        allocated_at: row.try_get("allocated_at")?,
        order_id: row.try_get("order_id")?,
        order_no: row.try_get("order_no")?,
        upstream_receiver_name: row.try_get("upstream_receiver_name")?,
        shipment_id: row.try_get("shipment_id")?,
        shipment_no: row.try_get("shipment_no")?,
        shipped_at: row.try_get("shipped_at")?,
        confirmation_code: row.try_get("confirmation_code")?,
        confirmed_at: row.try_get("confirmed_at")?,
        delivery_result: row.try_get("delivery_result")?,
        return_no: row.try_get("return_no")?,
        returned_at: row.try_get("returned_at")?,
        return_reason: row.try_get("return_reason")?,
        return_disposition: row.try_get("return_disposition")?,
    })
}

fn outbound_from_postgres_row(row: sqlx::postgres::PgRow) -> Result<OutboundTrace, sqlx::Error> {
    Ok(OutboundTrace {
        allocation_id: row.try_get::<Uuid, _>("allocation_id")?.to_string(),
        allocation_status: row.try_get("allocation_status")?,
        allocated_at: row.try_get("allocated_at")?,
        order_id: row.try_get::<Uuid, _>("order_id")?.to_string(),
        order_no: row.try_get("order_no")?,
        upstream_receiver_name: row.try_get("upstream_receiver_name")?,
        shipment_id: row
            .try_get::<Option<Uuid>, _>("shipment_id")?
            .map(|value| value.to_string()),
        shipment_no: row.try_get("shipment_no")?,
        shipped_at: row.try_get("shipped_at")?,
        confirmation_code: row.try_get("confirmation_code")?,
        confirmed_at: row.try_get("confirmed_at")?,
        delivery_result: row.try_get("delivery_result")?,
        return_no: row.try_get("return_no")?,
        returned_at: row.try_get("returned_at")?,
        return_reason: row.try_get("return_reason")?,
        return_disposition: row.try_get("return_disposition")?,
    })
}
