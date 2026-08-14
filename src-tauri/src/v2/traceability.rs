//! Read-only inventory provenance projected from immutable business facts.

use super::network::{NetworkResult, NetworkService, PERMISSION_INVENTORY_READ};
use super::sqlite::OfflineDatabase;
use super::warranty::WarrantyTerms;
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
    pub supplier_name: Option<String>,
    pub source_reference: Option<String>,
    pub received_at: String,
    pub inbound_warranty: Option<WarrantyTerms>,
    pub inventory_status: String,
    pub quality_status: String,
    pub inspections: Vec<InspectionTrace>,
    pub movements: Vec<MovementTrace>,
    pub outbound: Vec<OutboundTrace>,
    pub latest_related_order: Option<OutboundTrace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct InventoryBarcodeExistsResponse {
    pub barcode: String,
    pub exists: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InspectionTrace {
    pub inspection_no: String,
    pub inspection_type: String,
    pub result: String,
    pub quality_label_id: Option<String>,
    pub quality_label_snapshot: Option<String>,
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
    pub order_status: String,
    pub upstream_receiver_name: String,
    pub shipment_line_id: Option<String>,
    pub shipment_id: Option<String>,
    pub shipment_no: Option<String>,
    pub shipped_at: Option<String>,
    pub warranty: Option<WarrantyTerms>,
    pub confirmation_code: Option<String>,
    pub confirmed_at: Option<String>,
    pub delivery_result: Option<String>,
    pub return_no: Option<String>,
    pub returned_at: Option<String>,
    pub return_reason: Option<String>,
    pub return_disposition: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MovementTrace {
    pub movement_id: String,
    pub movement_type: String,
    pub source_type: String,
    pub source_id: String,
    pub occurred_at: String,
}

impl OfflineDatabase {
    pub async fn inventory_barcode_exists(
        &self,
        barcode: &str,
    ) -> Result<InventoryBarcodeExistsResponse, String> {
        let barcode = normalized_barcode(barcode)?;
        let exists = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM inventory_units WHERE workspace_id = ?1 AND barcode = ?2)",
        )
        .bind(self.workspace_id())
        .bind(&barcode)
        .fetch_one(self.pool())
        .await
        .map_err(|error| format!("检查库存条码失败: {error}"))?;
        Ok(InventoryBarcodeExistsResponse { barcode, exists })
    }

    pub async fn inventory_trace(&self, barcode: &str) -> Result<InventoryTrace, String> {
        let barcode = normalized_barcode(barcode)?;
        let row = sqlx::query(
            r#"
            SELECT iu.id AS inventory_unit_id, iu.barcode,
                   iu.owner_party_id, owner.display_name AS owner_name,
                   iu.sku_id, sku.code AS sku_code, sku.name AS sku_name,
                   receipt.id AS receipt_id, receipt.receipt_no,
                   supplier.display_name AS supplier_name, receipt.source_reference,
                   receipt.warranty_duration_days AS inbound_warranty_duration_days,
                   receipt.warranty_label_snapshot AS inbound_warranty_label_snapshot,
                   receipt.warranty_started_at AS inbound_warranty_started_at,
                   receipt.warranty_expires_at AS inbound_warranty_expires_at,
                   iu.received_at,
                   iu.inventory_status, iu.quality_status
              FROM inventory_units iu
              JOIN business_parties owner ON owner.id = iu.owner_party_id
              JOIN skus sku ON sku.id = iu.sku_id
              JOIN inbound_receipt_lines line ON line.id = iu.inbound_receipt_line_id
              JOIN inbound_receipts receipt ON receipt.id = line.receipt_id
              LEFT JOIN business_parties supplier ON supplier.id = receipt.supplier_party_id
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
                   result.result, result.quality_label_id,
                   result.quality_label_snapshot, inspection.inspected_at,
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
                quality_label_id: row.try_get("quality_label_id")?,
                quality_label_snapshot: row.try_get("quality_label_snapshot")?,
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
                   orders.id AS order_id, orders.order_no, orders.status AS order_status,
                   receiver.display_name AS upstream_receiver_name,
                   shipment_line.id AS shipment_line_id,
                   shipment.id AS shipment_id, shipment.shipment_no,
                   shipment.shipped_at,
                   shipment.warranty_duration_days, shipment.warranty_label_snapshot,
                   shipment.warranty_started_at, shipment.warranty_expires_at,
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

        let movements = sqlx::query(
            "SELECT id, movement_type, source_type, source_id, occurred_at FROM stock_movements WHERE workspace_id = ?1 AND inventory_unit_id = ?2 ORDER BY occurred_at, id",
        )
        .bind(self.workspace_id())
        .bind(&inventory_unit_id)
        .fetch_all(self.pool())
        .await
        .map_err(|error| format!("读取库存流水失败: {error}"))?
        .into_iter()
        .map(|row| {
            Ok(MovementTrace {
                movement_id: row.try_get("id")?,
                movement_type: row.try_get("movement_type")?,
                source_type: row.try_get("source_type")?,
                source_id: row.try_get("source_id")?,
                occurred_at: row.try_get("occurred_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(|error| error.to_string())?;
        let latest_related_order = outbound.last().cloned();

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
            supplier_name: row
                .try_get("supplier_name")
                .map_err(|error| error.to_string())?,
            source_reference: row
                .try_get("source_reference")
                .map_err(|error| error.to_string())?,
            received_at: row
                .try_get("received_at")
                .map_err(|error| error.to_string())?,
            inbound_warranty: warranty_from_sqlite_row(&row, "inbound_warranty")
                .map_err(|error| error.to_string())?,
            inventory_status: row
                .try_get("inventory_status")
                .map_err(|error| error.to_string())?,
            quality_status: row
                .try_get("quality_status")
                .map_err(|error| error.to_string())?,
            inspections,
            movements,
            outbound,
            latest_related_order,
        })
    }
}

impl NetworkService {
    pub async fn inventory_barcode_exists(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        barcode: &str,
    ) -> NetworkResult<InventoryBarcodeExistsResponse> {
        let barcode =
            normalized_barcode(barcode).map_err(super::network::NetworkServiceError::Invalid)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_INVENTORY_READ)
            .await?;
        let exists = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM inventory_units WHERE tenant_id = $1 AND barcode = $2)",
        )
        .bind(tenant_id)
        .bind(&barcode)
        .fetch_one(&mut **authorized.sqlx_transaction())
        .await?;
        authorized.commit().await?;
        Ok(InventoryBarcodeExistsResponse { barcode, exists })
    }

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
                   supplier.display_name AS supplier_name, receipt.source_reference,
                   receipt.warranty_duration_days AS inbound_warranty_duration_days,
                   receipt.warranty_label_snapshot AS inbound_warranty_label_snapshot,
                   receipt.warranty_started_at::text AS inbound_warranty_started_at,
                   receipt.warranty_expires_at::text AS inbound_warranty_expires_at,
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
              LEFT JOIN business_parties supplier
                ON supplier.tenant_id = receipt.tenant_id AND supplier.id = receipt.supplier_party_id
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
                   result.result, result.quality_label_id::text AS quality_label_id,
                   result.quality_label_snapshot,
                   inspection.inspected_at::text AS inspected_at,
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
                quality_label_id: row.try_get("quality_label_id")?,
                quality_label_snapshot: row.try_get("quality_label_snapshot")?,
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
                   orders.id AS order_id, orders.order_no, orders.status AS order_status,
                   receiver.display_name AS upstream_receiver_name,
                   shipment_line.id AS shipment_line_id,
                   shipment.id AS shipment_id, shipment.shipment_no,
                   shipment.shipped_at::text AS shipped_at,
                   shipment.warranty_duration_days, shipment.warranty_label_snapshot,
                   shipment.warranty_started_at::text AS warranty_started_at,
                   shipment.warranty_expires_at::text AS warranty_expires_at,
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

        let movements = sqlx::query(
            "SELECT id, movement_type, source_type, source_id, occurred_at::text AS occurred_at FROM stock_movements WHERE tenant_id = $1 AND inventory_unit_id = $2 ORDER BY occurred_at, id",
        )
        .bind(tenant_id)
        .bind(inventory_unit_id)
        .fetch_all(&mut **transaction)
        .await?
        .into_iter()
        .map(|row| {
            Ok(MovementTrace {
                movement_id: row.try_get::<Uuid, _>("id")?.to_string(),
                movement_type: row.try_get("movement_type")?,
                source_type: row.try_get("source_type")?,
                source_id: row.try_get::<Uuid, _>("source_id")?.to_string(),
                occurred_at: row.try_get("occurred_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let latest_related_order = outbound.last().cloned();

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
            supplier_name: row.try_get("supplier_name")?,
            source_reference: row.try_get("source_reference")?,
            received_at: row.try_get("received_at")?,
            inbound_warranty: warranty_from_postgres_row(&row, "inbound_warranty")?,
            inventory_status: row.try_get("inventory_status")?,
            quality_status: row.try_get("quality_status")?,
            inspections,
            movements,
            outbound,
            latest_related_order,
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
        order_status: row.try_get("order_status")?,
        upstream_receiver_name: row.try_get("upstream_receiver_name")?,
        shipment_line_id: row.try_get("shipment_line_id")?,
        shipment_id: row.try_get("shipment_id")?,
        shipment_no: row.try_get("shipment_no")?,
        shipped_at: row.try_get("shipped_at")?,
        warranty: warranty_from_sqlite_row(&row, "warranty")?,
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
        order_status: row.try_get("order_status")?,
        upstream_receiver_name: row.try_get("upstream_receiver_name")?,
        shipment_line_id: row
            .try_get::<Option<Uuid>, _>("shipment_line_id")?
            .map(|value| value.to_string()),
        shipment_id: row
            .try_get::<Option<Uuid>, _>("shipment_id")?
            .map(|value| value.to_string()),
        shipment_no: row.try_get("shipment_no")?,
        shipped_at: row.try_get("shipped_at")?,
        warranty: warranty_from_postgres_row(&row, "warranty")?,
        confirmation_code: row.try_get("confirmation_code")?,
        confirmed_at: row.try_get("confirmed_at")?,
        delivery_result: row.try_get("delivery_result")?,
        return_no: row.try_get("return_no")?,
        returned_at: row.try_get("returned_at")?,
        return_reason: row.try_get("return_reason")?,
        return_disposition: row.try_get("return_disposition")?,
    })
}

fn warranty_column(prefix: &str, field: &str) -> String {
    if prefix.is_empty() {
        field.to_owned()
    } else {
        format!("{prefix}_{field}")
    }
}

fn warranty_from_sqlite_row(
    row: &sqlx::sqlite::SqliteRow,
    prefix: &str,
) -> Result<Option<WarrantyTerms>, sqlx::Error> {
    let duration_column = warranty_column(prefix, "duration_days");
    let Some(duration) = row.try_get::<Option<i64>, _>(duration_column.as_str())? else {
        return Ok(None);
    };
    Ok(Some(WarrantyTerms {
        duration_days: u32::try_from(duration).unwrap_or_default(),
        label_snapshot: row.try_get(warranty_column(prefix, "label_snapshot").as_str())?,
        starts_at: row.try_get(warranty_column(prefix, "started_at").as_str())?,
        expires_at: row.try_get(warranty_column(prefix, "expires_at").as_str())?,
    }))
}

fn warranty_from_postgres_row(
    row: &sqlx::postgres::PgRow,
    prefix: &str,
) -> Result<Option<WarrantyTerms>, sqlx::Error> {
    let duration_column = warranty_column(prefix, "duration_days");
    let Some(duration) = row.try_get::<Option<i32>, _>(duration_column.as_str())? else {
        return Ok(None);
    };
    Ok(Some(WarrantyTerms {
        duration_days: u32::try_from(duration).unwrap_or_default(),
        label_snapshot: row.try_get(warranty_column(prefix, "label_snapshot").as_str())?,
        starts_at: row.try_get(warranty_column(prefix, "started_at").as_str())?,
        expires_at: row.try_get(warranty_column(prefix, "expires_at").as_str())?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::application::{
        CatalogPartyRole, CreateCatalogPartyRequest, CreateCatalogProductRequest,
        PostReceiptRequest,
    };
    use std::path::{Path, PathBuf};

    async fn remove_test_database(database: OfflineDatabase, path: &Path) {
        database.pool().close().await;
        for candidate in [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = std::fs::remove_file(candidate);
        }
    }

    #[test]
    fn barcode_normalization_matches_scanner_storage_rules() {
        assert_eq!(
            normalized_barcode("  sn-exact-001\r\n").expect("normalized barcode"),
            "SN-EXACT-001"
        );
        assert_eq!(
            normalized_barcode(" \t ").expect_err("blank barcode must fail"),
            "barcode must not be empty"
        );
    }

    #[tokio::test]
    async fn offline_barcode_exists_is_normalized_exact_and_workspace_scoped() {
        let path = std::env::temp_dir().join(format!(
            "inventory-barcode-exists-{}.sqlite",
            Uuid::now_v7()
        ));
        let database = OfflineDatabase::open(&path)
            .await
            .expect("open isolated offline database");
        for (display_name, role) in [
            ("Owner A", CatalogPartyRole::GoodsOwner),
            ("Supplier A", CatalogPartyRole::Supplier),
        ] {
            database
                .create_catalog_party(CreateCatalogPartyRequest {
                    display_name: display_name.to_owned(),
                    role,
                })
                .await
                .expect("create catalog party");
        }
        database
            .create_catalog_product(CreateCatalogProductRequest {
                code: "SKU-EXACT".to_owned(),
                name: "Exact Lookup Product".to_owned(),
                serial_prefix: None,
                serial_forbidden_chars: String::new(),
            })
            .await
            .expect("create catalog product");
        database
            .post_receipt(PostReceiptRequest {
                request_id: "barcode-exists-request".to_owned(),
                idempotency_key: "barcode-exists-key".to_owned(),
                receipt_no: "RK-BARCODE-EXISTS".to_owned(),
                owner_name: "Owner A".to_owned(),
                supplier_name: "Supplier A".to_owned(),
                sku_code: "SKU-EXACT".to_owned(),
                sku_name: "Exact Lookup Product".to_owned(),
                source_reference: None,
                received_at: "2026-08-08T01:00:00Z".to_owned(),
                actor_id: "operator-1".to_owned(),
                barcodes: vec!["sn-exact-001".to_owned()],
                notes: None,
                warranty: None,
            })
            .await
            .expect("post receipt");

        assert_eq!(
            database
                .inventory_barcode_exists("  sn-exact-001\r\n")
                .await
                .expect("lookup existing barcode"),
            InventoryBarcodeExistsResponse {
                barcode: "SN-EXACT-001".to_owned(),
                exists: true,
            }
        );
        assert_eq!(
            database
                .inventory_barcode_exists("SN-EXACT-001-EXTRA")
                .await
                .expect("lookup similar missing barcode"),
            InventoryBarcodeExistsResponse {
                barcode: "SN-EXACT-001-EXTRA".to_owned(),
                exists: false,
            }
        );

        remove_test_database(database, &path).await;
    }
}
