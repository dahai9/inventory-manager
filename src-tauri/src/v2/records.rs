use super::network::{
    NetworkResult, NetworkService, NetworkServiceError, PERMISSION_INVENTORY_READ,
};
use super::sqlite::OfflineDatabase;
use super::warranty::WarrantyTerms;
use rust_xlsxwriter::{Color, Format, FormatAlign, FormatBorder, Workbook, XlsxError};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RecordSearchQuery {
    pub search: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReceiptRecord {
    pub receipt_id: String,
    pub receipt_no: String,
    pub supplier_name: Option<String>,
    pub owner_name: String,
    pub source_reference: Option<String>,
    pub received_at: String,
    pub status: String,
    pub item_count: u32,
    pub warranty: Option<WarrantyTerms>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutboundOrderRecord {
    pub order_id: String,
    pub order_no: String,
    pub receiver_name: String,
    pub status: String,
    pub created_at: String,
    pub latest_shipment_no: Option<String>,
    pub latest_shipped_at: Option<String>,
    pub item_count: u32,
    pub returned_count: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReceiptDocument {
    #[serde(flatten)]
    pub receipt: ReceiptRecord,
    pub items: Vec<DocumentItem>,
    pub void_info: Option<DocumentVoidInfo>,
    pub void_eligibility: DocumentVoidEligibility,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutboundOrderDocument {
    #[serde(flatten)]
    pub order: OutboundOrderRecord,
    pub items: Vec<DocumentItem>,
    pub void_info: Option<DocumentVoidInfo>,
    pub void_eligibility: DocumentVoidEligibility,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DocumentVoidInfo {
    pub reason: String,
    pub actor_id: String,
    pub voided_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DocumentVoidEligibility {
    pub can_void: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DocumentItem {
    pub sku_code: String,
    pub sku_name: String,
    pub barcode: String,
    pub inventory_status: String,
    pub allocation_status: Option<String>,
    pub owner_name: Option<String>,
    pub shipment_id: Option<String>,
    pub shipment_line_id: Option<String>,
    pub shipment_no: Option<String>,
    pub shipped_at: Option<String>,
    pub warranty: Option<WarrantyTerms>,
    pub return_no: Option<String>,
    pub returned_at: Option<String>,
    pub return_reason: Option<String>,
    pub return_disposition: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReturnCandidate {
    pub barcode: String,
    pub inventory_unit_id: String,
    pub shipment_id: String,
    pub shipment_line_id: String,
    pub shipment_no: String,
    pub shipped_at: String,
    pub order_id: String,
    pub order_no: String,
    pub receiver_name: String,
    pub warranty: Option<WarrantyTerms>,
}

impl OfflineDatabase {
    pub async fn list_receipt_records(
        &self,
        query: RecordSearchQuery,
    ) -> Result<Vec<ReceiptRecord>, String> {
        let search = search_pattern(query.search);
        let limit = normalized_limit(query.limit);
        sqlx::query(
            r#"
            SELECT r.id, r.receipt_no, supplier.display_name AS supplier_name,
                   owner.display_name AS owner_name, r.source_reference,
                   r.received_at, r.status, COUNT(iu.id) AS item_count,
                   r.warranty_duration_days, r.warranty_label_snapshot,
                   r.warranty_started_at, r.warranty_expires_at
              FROM inbound_receipts r
              JOIN business_parties owner ON owner.id = r.owner_party_id
              LEFT JOIN business_parties supplier ON supplier.id = r.supplier_party_id
              LEFT JOIN inbound_receipt_lines rl ON rl.receipt_id = r.id
              LEFT JOIN inventory_units iu ON iu.inbound_receipt_line_id = rl.id
             WHERE r.workspace_id = ?1
               AND (?2 = '%%' OR r.receipt_no LIKE ?2 OR COALESCE(supplier.display_name, '') LIKE ?2
                    OR owner.display_name LIKE ?2 OR COALESCE(r.source_reference, '') LIKE ?2
                    OR EXISTS (SELECT 1 FROM inbound_receipt_lines sx
                                JOIN inventory_units ux ON ux.inbound_receipt_line_id = sx.id
                               WHERE sx.receipt_id = r.id AND ux.barcode LIKE ?2))
             GROUP BY r.id
             ORDER BY r.received_at DESC, r.id DESC
             LIMIT ?3
            "#,
        )
        .bind(self.workspace_id())
        .bind(search)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(|error| format!("读取收货单历史失败: {error}"))?
        .into_iter()
        .map(receipt_record_sqlite)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
    }

    pub async fn list_outbound_order_records(
        &self,
        query: RecordSearchQuery,
    ) -> Result<Vec<OutboundOrderRecord>, String> {
        let search = search_pattern(query.search);
        let limit = normalized_limit(query.limit);
        sqlx::query(
            r#"
            SELECT o.id, o.order_no, receiver.display_name AS receiver_name,
                   o.status, o.created_at,
                   (SELECT s.shipment_no FROM outbound_shipments s
                     WHERE s.workspace_id = o.workspace_id AND s.outbound_order_id = o.id
                     ORDER BY s.shipped_at DESC, s.id DESC LIMIT 1) AS latest_shipment_no,
                   (SELECT s.shipped_at FROM outbound_shipments s
                     WHERE s.workspace_id = o.workspace_id AND s.outbound_order_id = o.id
                     ORDER BY s.shipped_at DESC, s.id DESC LIMIT 1) AS latest_shipped_at,
                   COUNT(DISTINCT a.id) AS item_count,
                   COUNT(DISTINCT ret.id) AS returned_count
              FROM outbound_orders o
              JOIN business_parties receiver ON receiver.id = o.upstream_receiver_id
              LEFT JOIN outbound_order_lines ol ON ol.outbound_order_id = o.id
              LEFT JOIN outbound_allocations a ON a.outbound_order_line_id = ol.id
              LEFT JOIN outbound_shipment_lines sl ON sl.outbound_allocation_id = a.id
              LEFT JOIN outbound_shipments ship ON ship.id = sl.outbound_shipment_id
              LEFT JOIN outbound_return_lines ret ON ret.outbound_shipment_line_id = sl.id
             WHERE o.workspace_id = ?1
               AND (?2 = '%%' OR o.order_no LIKE ?2 OR receiver.display_name LIKE ?2
                    OR COALESCE(ship.shipment_no, '') LIKE ?2
                    OR COALESCE(sl.scanned_barcode_snapshot, '') LIKE ?2)
             GROUP BY o.id
             ORDER BY o.created_at DESC, o.id DESC
             LIMIT ?3
            "#,
        )
        .bind(self.workspace_id())
        .bind(search)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(|error| format!("读取出库订单历史失败: {error}"))?
        .into_iter()
        .map(outbound_record_sqlite)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
    }

    pub async fn receipt_document(&self, receipt_id: &str) -> Result<ReceiptDocument, String> {
        let receipt = sqlx::query(
            r#"
            SELECT r.id, r.receipt_no, supplier.display_name AS supplier_name,
                   owner.display_name AS owner_name, r.source_reference,
                   r.received_at, r.status, COUNT(iu.id) AS item_count,
                   r.warranty_duration_days, r.warranty_label_snapshot,
                   r.warranty_started_at, r.warranty_expires_at
              FROM inbound_receipts r
              JOIN business_parties owner ON owner.id = r.owner_party_id
              LEFT JOIN business_parties supplier ON supplier.id = r.supplier_party_id
              LEFT JOIN inbound_receipt_lines rl ON rl.receipt_id = r.id
              LEFT JOIN inventory_units iu ON iu.inbound_receipt_line_id = rl.id
             WHERE r.workspace_id = ?1 AND r.id = ?2
             GROUP BY r.id
            "#,
        )
        .bind(self.workspace_id())
        .bind(receipt_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| format!("读取收货单失败: {error}"))?
        .map(receipt_record_sqlite)
        .transpose()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("找不到收货单 {receipt_id}"))?;
        let items = sqlx::query(
            r#"
            SELECT sku.code AS sku_code, sku.name AS sku_name, iu.barcode,
                   iu.inventory_status,
                   owner.display_name AS owner_name
              FROM inbound_receipt_lines rl
              JOIN inventory_units iu ON iu.inbound_receipt_line_id = rl.id
              JOIN skus sku ON sku.id = iu.sku_id
              JOIN business_parties owner ON owner.id = iu.owner_party_id
             WHERE rl.workspace_id = ?1 AND rl.receipt_id = ?2
             ORDER BY sku.code, iu.barcode
            "#,
        )
        .bind(self.workspace_id())
        .bind(receipt_id)
        .fetch_all(self.pool())
        .await
        .map_err(|error| format!("读取收货单明细失败: {error}"))?
        .into_iter()
        .map(document_receipt_item_sqlite)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
        let void_info = load_receipt_void_sqlite(self, receipt_id).await?;
        let void_eligibility =
            receipt_void_eligibility_sqlite(self, receipt_id, &receipt.status).await?;
        Ok(ReceiptDocument {
            receipt,
            items,
            void_info,
            void_eligibility,
        })
    }

    pub async fn outbound_order_document(
        &self,
        order_id: &str,
    ) -> Result<OutboundOrderDocument, String> {
        let order = sqlx::query(
            r#"
            SELECT o.id, o.order_no, receiver.display_name AS receiver_name,
                   o.status, o.created_at,
                   (SELECT s.shipment_no FROM outbound_shipments s
                     WHERE s.workspace_id = o.workspace_id AND s.outbound_order_id = o.id
                     ORDER BY s.shipped_at DESC, s.id DESC LIMIT 1) AS latest_shipment_no,
                   (SELECT s.shipped_at FROM outbound_shipments s
                     WHERE s.workspace_id = o.workspace_id AND s.outbound_order_id = o.id
                     ORDER BY s.shipped_at DESC, s.id DESC LIMIT 1) AS latest_shipped_at,
                   COUNT(DISTINCT a.id) AS item_count,
                   COUNT(DISTINCT ret.id) AS returned_count
              FROM outbound_orders o
              JOIN business_parties receiver ON receiver.id = o.upstream_receiver_id
              LEFT JOIN outbound_order_lines ol ON ol.outbound_order_id = o.id
              LEFT JOIN outbound_allocations a ON a.outbound_order_line_id = ol.id
              LEFT JOIN outbound_shipment_lines sl ON sl.outbound_allocation_id = a.id
              LEFT JOIN outbound_return_lines ret ON ret.outbound_shipment_line_id = sl.id
             WHERE o.workspace_id = ?1 AND o.id = ?2
             GROUP BY o.id
            "#,
        )
        .bind(self.workspace_id())
        .bind(order_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| format!("读取出库订单失败: {error}"))?
        .map(outbound_record_sqlite)
        .transpose()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("找不到出库订单 {order_id}"))?;
        let items = sqlx::query(outbound_document_sqlite())
            .bind(self.workspace_id())
            .bind(order_id)
            .fetch_all(self.pool())
            .await
            .map_err(|error| format!("读取出库订单明细失败: {error}"))?
            .into_iter()
            .map(document_outbound_item_sqlite)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let void_info = load_outbound_void_sqlite(self, order_id).await?;
        let void_eligibility =
            outbound_void_eligibility_sqlite(self, order_id, &order.status).await?;
        Ok(OutboundOrderDocument {
            order,
            items,
            void_info,
            void_eligibility,
        })
    }

    pub async fn lookup_return_candidate(&self, barcode: &str) -> Result<ReturnCandidate, String> {
        let barcode = barcode.trim().to_uppercase();
        if barcode.is_empty() {
            return Err("请输入或扫描退货 SN".to_owned());
        }
        sqlx::query(
            r#"
            SELECT iu.id AS inventory_unit_id, iu.barcode,
                   sl.id AS shipment_line_id, ship.id AS shipment_id,
                   ship.shipment_no, ship.shipped_at,
                   ship.warranty_duration_days, ship.warranty_label_snapshot,
                   ship.warranty_started_at, ship.warranty_expires_at,
                   o.id AS order_id, o.order_no, receiver.display_name AS receiver_name
              FROM inventory_units iu
              JOIN outbound_shipment_lines sl ON sl.inventory_unit_id = iu.id
              JOIN outbound_shipments ship ON ship.id = sl.outbound_shipment_id
              JOIN outbound_orders o ON o.id = ship.outbound_order_id
              JOIN business_parties receiver ON receiver.id = o.upstream_receiver_id
             WHERE iu.workspace_id = ?1 AND iu.barcode = ?2
               AND iu.inventory_status IN ('shipped', 'delivered')
               AND NOT EXISTS (SELECT 1 FROM outbound_return_lines ret
                                WHERE ret.outbound_shipment_line_id = sl.id)
             ORDER BY ship.shipped_at DESC, sl.id DESC
             LIMIT 1
            "#,
        )
        .bind(self.workspace_id())
        .bind(&barcode)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| format!("读取退货来源失败: {error}"))?
        .map(return_candidate_sqlite)
        .transpose()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("SN {barcode} 没有可退回的有效出库记录"))
    }
}

fn normalized_limit(limit: Option<u32>) -> i64 {
    i64::from(limit.unwrap_or(100).clamp(1, 500))
}

fn search_pattern(search: Option<String>) -> String {
    search
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"))
        .unwrap_or_else(|| "%%".to_owned())
}

fn warranty_from_sqlite(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<Option<WarrantyTerms>, sqlx::Error> {
    let Some(duration) = row.try_get::<Option<i64>, _>("warranty_duration_days")? else {
        return Ok(None);
    };
    Ok(Some(WarrantyTerms {
        duration_days: u32::try_from(duration).unwrap_or_default(),
        label_snapshot: row.try_get("warranty_label_snapshot")?,
        starts_at: row.try_get("warranty_started_at")?,
        expires_at: row.try_get("warranty_expires_at")?,
    }))
}

fn receipt_record_sqlite(row: sqlx::sqlite::SqliteRow) -> Result<ReceiptRecord, sqlx::Error> {
    Ok(ReceiptRecord {
        receipt_id: row.try_get("id")?,
        receipt_no: row.try_get("receipt_no")?,
        supplier_name: row.try_get("supplier_name")?,
        owner_name: row.try_get("owner_name")?,
        source_reference: row.try_get("source_reference")?,
        received_at: row.try_get("received_at")?,
        status: row.try_get("status")?,
        item_count: u32::try_from(row.try_get::<i64, _>("item_count")?).unwrap_or_default(),
        warranty: warranty_from_sqlite(&row)?,
    })
}

fn outbound_record_sqlite(
    row: sqlx::sqlite::SqliteRow,
) -> Result<OutboundOrderRecord, sqlx::Error> {
    Ok(OutboundOrderRecord {
        order_id: row.try_get("id")?,
        order_no: row.try_get("order_no")?,
        receiver_name: row.try_get("receiver_name")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        latest_shipment_no: row.try_get("latest_shipment_no")?,
        latest_shipped_at: row.try_get("latest_shipped_at")?,
        item_count: u32::try_from(row.try_get::<i64, _>("item_count")?).unwrap_or_default(),
        returned_count: u32::try_from(row.try_get::<i64, _>("returned_count")?).unwrap_or_default(),
    })
}

fn document_receipt_item_sqlite(row: sqlx::sqlite::SqliteRow) -> Result<DocumentItem, sqlx::Error> {
    Ok(DocumentItem {
        sku_code: row.try_get("sku_code")?,
        sku_name: row.try_get("sku_name")?,
        barcode: row.try_get("barcode")?,
        inventory_status: row.try_get("inventory_status")?,
        allocation_status: None,
        owner_name: row.try_get("owner_name")?,
        shipment_id: None,
        shipment_line_id: None,
        shipment_no: None,
        shipped_at: None,
        warranty: None,
        return_no: None,
        returned_at: None,
        return_reason: None,
        return_disposition: None,
    })
}

fn outbound_document_sqlite() -> &'static str {
    r#"
    SELECT sku.code AS sku_code, sku.name AS sku_name, iu.barcode,
           iu.inventory_status, a.status AS allocation_status,
           owner.display_name AS owner_name,
           ship.id AS shipment_id, sl.id AS shipment_line_id,
           ship.shipment_no, ship.shipped_at,
           ship.warranty_duration_days, ship.warranty_label_snapshot,
           ship.warranty_started_at, ship.warranty_expires_at,
           rb.return_no, rb.returned_at, ret.reason AS return_reason,
           ret.disposition AS return_disposition
      FROM outbound_order_lines ol
      JOIN outbound_allocations a ON a.outbound_order_line_id = ol.id
      JOIN inventory_units iu ON iu.id = a.inventory_unit_id
      JOIN skus sku ON sku.id = iu.sku_id
      JOIN business_parties owner ON owner.id = iu.owner_party_id
      LEFT JOIN outbound_shipment_lines sl ON sl.outbound_allocation_id = a.id
      LEFT JOIN outbound_shipments ship ON ship.id = sl.outbound_shipment_id
      LEFT JOIN outbound_return_lines ret ON ret.outbound_shipment_line_id = sl.id
      LEFT JOIN outbound_return_batches rb ON rb.id = ret.return_batch_id
     WHERE ol.workspace_id = ?1 AND ol.outbound_order_id = ?2
     ORDER BY ship.shipped_at, sku.code, iu.barcode
    "#
}

fn document_outbound_item_sqlite(
    row: sqlx::sqlite::SqliteRow,
) -> Result<DocumentItem, sqlx::Error> {
    Ok(DocumentItem {
        sku_code: row.try_get("sku_code")?,
        sku_name: row.try_get("sku_name")?,
        barcode: row.try_get("barcode")?,
        inventory_status: row.try_get("inventory_status")?,
        allocation_status: row.try_get("allocation_status")?,
        owner_name: row.try_get("owner_name")?,
        shipment_id: row.try_get("shipment_id")?,
        shipment_line_id: row.try_get("shipment_line_id")?,
        shipment_no: row.try_get("shipment_no")?,
        shipped_at: row.try_get("shipped_at")?,
        warranty: warranty_from_sqlite(&row)?,
        return_no: row.try_get("return_no")?,
        returned_at: row.try_get("returned_at")?,
        return_reason: row.try_get("return_reason")?,
        return_disposition: row.try_get("return_disposition")?,
    })
}

fn return_candidate_sqlite(row: sqlx::sqlite::SqliteRow) -> Result<ReturnCandidate, sqlx::Error> {
    Ok(ReturnCandidate {
        barcode: row.try_get("barcode")?,
        inventory_unit_id: row.try_get("inventory_unit_id")?,
        shipment_id: row.try_get("shipment_id")?,
        shipment_line_id: row.try_get("shipment_line_id")?,
        shipment_no: row.try_get("shipment_no")?,
        shipped_at: row.try_get("shipped_at")?,
        order_id: row.try_get("order_id")?,
        order_no: row.try_get("order_no")?,
        receiver_name: row.try_get("receiver_name")?,
        warranty: warranty_from_sqlite(&row)?,
    })
}

async fn load_receipt_void_sqlite(
    database: &OfflineDatabase,
    receipt_id: &str,
) -> Result<Option<DocumentVoidInfo>, String> {
    load_document_void_sqlite(database, "inbound_receipt_id", receipt_id).await
}

async fn load_outbound_void_sqlite(
    database: &OfflineDatabase,
    order_id: &str,
) -> Result<Option<DocumentVoidInfo>, String> {
    load_document_void_sqlite(database, "outbound_order_id", order_id).await
}

async fn load_document_void_sqlite(
    database: &OfflineDatabase,
    field: &str,
    document_id: &str,
) -> Result<Option<DocumentVoidInfo>, String> {
    let query = format!(
        "SELECT reason, actor_id, voided_at FROM document_voids WHERE workspace_id = ?1 AND {field} = ?2"
    );
    sqlx::query(&query)
        .bind(database.workspace_id())
        .bind(document_id)
        .fetch_optional(database.pool())
        .await
        .map_err(|error| format!("读取单据作废信息失败: {error}"))?
        .map(|row| {
            Ok(DocumentVoidInfo {
                reason: row.try_get("reason")?,
                actor_id: row.try_get("actor_id")?,
                voided_at: row.try_get("voided_at")?,
            })
        })
        .transpose()
        .map_err(|error: sqlx::Error| error.to_string())
}

async fn receipt_void_eligibility_sqlite(
    database: &OfflineDatabase,
    receipt_id: &str,
    status: &str,
) -> Result<DocumentVoidEligibility, String> {
    if status == "voided" {
        return Ok(DocumentVoidEligibility {
            can_void: false,
            blockers: vec!["该收货单已经作废".to_owned()],
        });
    }
    if status != "posted" {
        return Ok(DocumentVoidEligibility {
            can_void: false,
            blockers: vec![format!("当前单据状态为 {status}")],
        });
    }
    let rows = sqlx::query(
        r#"
        SELECT iu.barcode, iu.inventory_status,
               EXISTS (SELECT 1 FROM outbound_allocations oa
                        WHERE oa.workspace_id=iu.workspace_id AND oa.inventory_unit_id=iu.id) AS has_outbound
          FROM inbound_receipt_lines rl
          JOIN inventory_units iu ON iu.inbound_receipt_line_id=rl.id
         WHERE rl.workspace_id=?1 AND rl.receipt_id=?2 ORDER BY iu.barcode
        "#,
    )
    .bind(database.workspace_id())
    .bind(receipt_id)
    .fetch_all(database.pool())
    .await
    .map_err(|error| format!("检查收货单作废条件失败: {error}"))?;
    let mut blockers = Vec::new();
    for row in rows {
        let barcode: String = row.try_get("barcode").map_err(|error| error.to_string())?;
        let unit_status: String = row
            .try_get("inventory_status")
            .map_err(|error| error.to_string())?;
        let has_outbound: bool = row
            .try_get("has_outbound")
            .map_err(|error| error.to_string())?;
        if has_outbound {
            blockers.push(format!("SN {barcode} 已关联出库业务"));
        } else if !matches!(
            unit_status.as_str(),
            "received" | "available" | "quarantined"
        ) {
            blockers.push(format!("SN {barcode} 当前状态为 {unit_status}"));
        }
    }
    Ok(DocumentVoidEligibility {
        can_void: blockers.is_empty(),
        blockers,
    })
}

async fn outbound_void_eligibility_sqlite(
    database: &OfflineDatabase,
    order_id: &str,
    status: &str,
) -> Result<DocumentVoidEligibility, String> {
    if status == "voided" {
        return Ok(DocumentVoidEligibility {
            can_void: false,
            blockers: vec!["该出库订单已经作废".to_owned()],
        });
    }
    let barcodes: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT sl.scanned_barcode_snapshot
          FROM outbound_shipments ship
          JOIN outbound_shipment_lines sl ON sl.outbound_shipment_id=ship.id
          LEFT JOIN outbound_return_lines ret ON ret.outbound_shipment_line_id=sl.id
         WHERE ship.workspace_id=?1 AND ship.outbound_order_id=?2 AND ret.id IS NULL
         ORDER BY sl.scanned_barcode_snapshot
        "#,
    )
    .bind(database.workspace_id())
    .bind(order_id)
    .fetch_all(database.pool())
    .await
    .map_err(|error| format!("检查出库单作废条件失败: {error}"))?;
    let blockers = barcodes
        .into_iter()
        .map(|barcode| format!("SN {barcode} 已出库但尚未退回"))
        .collect::<Vec<_>>();
    Ok(DocumentVoidEligibility {
        can_void: blockers.is_empty(),
        blockers,
    })
}

pub fn write_receipt_workbook(
    path: impl AsRef<Path>,
    document: &ReceiptDocument,
) -> Result<(), String> {
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    worksheet.set_name("收货单").map_err(xlsx_error)?;
    let styles = WorkbookStyles::new();
    worksheet
        .merge_range(0, 0, 0, 5, "收货单", &styles.title)
        .map_err(xlsx_error)?;
    write_info(
        worksheet,
        2,
        "收货单号",
        &document.receipt.receipt_no,
        &styles,
    )?;
    write_info(
        worksheet,
        3,
        "入库时间",
        &document.receipt.received_at,
        &styles,
    )?;
    write_info(
        worksheet,
        4,
        "供应商",
        document
            .receipt
            .supplier_name
            .as_deref()
            .unwrap_or("未记录"),
        &styles,
    )?;
    write_info(worksheet, 5, "货主", &document.receipt.owner_name, &styles)?;
    write_info(
        worksheet,
        6,
        "来源单号",
        document
            .receipt
            .source_reference
            .as_deref()
            .unwrap_or("未记录"),
        &styles,
    )?;
    write_info(
        worksheet,
        7,
        "供应方质保",
        warranty_text(document.receipt.warranty.as_ref()).as_str(),
        &styles,
    )?;
    write_info(worksheet, 8, "单据状态", &document.receipt.status, &styles)?;
    let mut next_row = 9;
    if let Some(void_info) = &document.void_info {
        write_info(
            worksheet,
            next_row,
            "作废时间",
            &void_info.voided_at,
            &styles,
        )?;
        write_info(
            worksheet,
            next_row + 1,
            "作废操作人",
            &void_info.actor_id,
            &styles,
        )?;
        write_info(
            worksheet,
            next_row + 2,
            "作废原因",
            &void_info.reason,
            &styles,
        )?;
        next_row += 3;
    }
    write_info(
        worksheet,
        next_row,
        "总数量",
        &document.items.len().to_string(),
        &styles,
    )?;
    let table_row = next_row + 2;
    write_item_table(worksheet, table_row, &document.items, false, &styles)?;
    configure_sheet(worksheet, document.items.len() as u32 + table_row + 1)?;
    workbook.save(path).map_err(xlsx_error)
}

pub fn write_outbound_workbook(
    path: impl AsRef<Path>,
    document: &OutboundOrderDocument,
) -> Result<(), String> {
    let shipped_items: Vec<_> = document
        .items
        .iter()
        .filter(|item| item.shipment_id.is_some())
        .cloned()
        .collect();
    if shipped_items.is_empty() && document.order.status != "voided" {
        return Err("该订单尚无已出库商品，不能导出出库单".to_owned());
    }
    let export_items = if shipped_items.is_empty() {
        document.items.clone()
    } else {
        shipped_items.clone()
    };
    let mut workbook = Workbook::new();
    let styles = WorkbookStyles::new();
    let worksheet = workbook.add_worksheet();
    worksheet.set_name("出库单").map_err(xlsx_error)?;
    worksheet
        .merge_range(0, 0, 0, 5, "出库单", &styles.title)
        .map_err(xlsx_error)?;
    write_info(worksheet, 2, "订单编号", &document.order.order_no, &styles)?;
    write_info(
        worksheet,
        3,
        "出库单号",
        document
            .order
            .latest_shipment_no
            .as_deref()
            .unwrap_or("未记录"),
        &styles,
    )?;
    write_info(worksheet, 4, "客户", &document.order.receiver_name, &styles)?;
    write_info(
        worksheet,
        5,
        "出库时间",
        document
            .order
            .latest_shipped_at
            .as_deref()
            .unwrap_or("未记录"),
        &styles,
    )?;
    write_info(
        worksheet,
        6,
        "客户质保",
        warranty_text(shipped_items.last().and_then(|item| item.warranty.as_ref())).as_str(),
        &styles,
    )?;
    write_info(worksheet, 7, "单据状态", &document.order.status, &styles)?;
    let mut next_row = 8;
    if let Some(void_info) = &document.void_info {
        write_info(
            worksheet,
            next_row,
            "作废时间",
            &void_info.voided_at,
            &styles,
        )?;
        write_info(
            worksheet,
            next_row + 1,
            "作废操作人",
            &void_info.actor_id,
            &styles,
        )?;
        write_info(
            worksheet,
            next_row + 2,
            "作废原因",
            &void_info.reason,
            &styles,
        )?;
        next_row += 3;
    }
    write_info(
        worksheet,
        next_row,
        "总数量",
        &export_items.len().to_string(),
        &styles,
    )?;
    let table_row = next_row + 2;
    write_item_table(worksheet, table_row, &export_items, true, &styles)?;
    configure_sheet(worksheet, export_items.len() as u32 + table_row + 1)?;

    let returned: Vec<_> = shipped_items
        .iter()
        .filter(|item| item.return_no.is_some())
        .collect();
    if !returned.is_empty() {
        let after_sales = workbook.add_worksheet();
        after_sales.set_name("售后记录").map_err(xlsx_error)?;
        let headers = ["SN", "退货单号", "退货时间", "退货原因", "处置状态"];
        for (column, header) in headers.iter().enumerate() {
            after_sales
                .write_with_format(0, column as u16, *header, &styles.header)
                .map_err(xlsx_error)?;
        }
        for (index, item) in returned.iter().enumerate() {
            let row = index as u32 + 1;
            for (column, value) in [
                item.barcode.as_str(),
                item.return_no.as_deref().unwrap_or(""),
                item.returned_at.as_deref().unwrap_or(""),
                item.return_reason.as_deref().unwrap_or(""),
                item.return_disposition.as_deref().unwrap_or(""),
            ]
            .iter()
            .enumerate()
            {
                after_sales
                    .write_with_format(row, column as u16, *value, &styles.cell)
                    .map_err(xlsx_error)?;
            }
        }
        after_sales.set_column_width(0, 24).map_err(xlsx_error)?;
        after_sales.set_column_width(1, 20).map_err(xlsx_error)?;
        after_sales.set_column_width(2, 22).map_err(xlsx_error)?;
        after_sales.set_column_width(3, 34).map_err(xlsx_error)?;
        after_sales.set_column_width(4, 18).map_err(xlsx_error)?;
    }
    workbook.save(path).map_err(xlsx_error)
}

struct WorkbookStyles {
    title: Format,
    label: Format,
    value: Format,
    header: Format,
    cell: Format,
}

impl WorkbookStyles {
    fn new() -> Self {
        Self {
            title: Format::new()
                .set_bold()
                .set_font_size(20)
                .set_align(FormatAlign::Center),
            label: Format::new()
                .set_bold()
                .set_background_color(Color::RGB(0xEAF0EA))
                .set_border(FormatBorder::Thin),
            value: Format::new().set_border(FormatBorder::Thin),
            header: Format::new()
                .set_bold()
                .set_font_color(Color::White)
                .set_background_color(Color::RGB(0x235347))
                .set_border(FormatBorder::Thin)
                .set_align(FormatAlign::Center),
            cell: Format::new().set_border(FormatBorder::Thin),
        }
    }
}

fn write_info(
    worksheet: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    label: &str,
    value: &str,
    styles: &WorkbookStyles,
) -> Result<(), String> {
    worksheet
        .write_with_format(row, 0, label, &styles.label)
        .map_err(xlsx_error)?;
    worksheet
        .merge_range(row, 1, row, 5, value, &styles.value)
        .map_err(xlsx_error)?;
    Ok(())
}

fn write_item_table(
    worksheet: &mut rust_xlsxwriter::Worksheet,
    start_row: u32,
    items: &[DocumentItem],
    outbound: bool,
    styles: &WorkbookStyles,
) -> Result<(), String> {
    let headers = [
        "序号",
        "SKU编码",
        "商品名称",
        "SN / 条码",
        if outbound { "出库单号" } else { "货主" },
        "状态",
    ];
    for (column, header) in headers.iter().enumerate() {
        worksheet
            .write_with_format(start_row, column as u16, *header, &styles.header)
            .map_err(xlsx_error)?;
    }
    for (index, item) in items.iter().enumerate() {
        let row = start_row + index as u32 + 1;
        worksheet
            .write_with_format(row, 0, (index + 1) as u32, &styles.cell)
            .map_err(xlsx_error)?;
        let status = if item.return_no.is_some() {
            "已退货"
        } else if outbound {
            "已出库"
        } else {
            "已收货"
        };
        for (column, value) in [
            item.sku_code.as_str(),
            item.sku_name.as_str(),
            item.barcode.as_str(),
            if outbound {
                item.shipment_no.as_deref().unwrap_or("")
            } else {
                item.owner_name.as_deref().unwrap_or("")
            },
            status,
        ]
        .iter()
        .enumerate()
        {
            worksheet
                .write_with_format(row, column as u16 + 1, *value, &styles.cell)
                .map_err(xlsx_error)?;
        }
    }
    Ok(())
}

fn configure_sheet(
    worksheet: &mut rust_xlsxwriter::Worksheet,
    last_row: u32,
) -> Result<(), String> {
    for (column, width) in [18.0, 18.0, 26.0, 28.0, 20.0, 16.0].into_iter().enumerate() {
        worksheet
            .set_column_width(column as u16, width)
            .map_err(xlsx_error)?;
    }
    worksheet.set_landscape();
    worksheet
        .set_print_area(0, 0, last_row, 5)
        .map_err(xlsx_error)?;
    Ok(())
}

fn warranty_text(warranty: Option<&WarrantyTerms>) -> String {
    warranty
        .map(|terms| {
            format!(
                "{}（{} 至 {}）",
                terms.label_snapshot, terms.starts_at, terms.expires_at
            )
        })
        .unwrap_or_else(|| "无质保".to_owned())
}

fn xlsx_error(error: XlsxError) -> String {
    format!("生成 Excel 单据失败: {error}")
}

// Network implementations live below so both editions return identical DTOs.
impl NetworkService {
    pub async fn list_receipt_records_network(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        query: RecordSearchQuery,
    ) -> NetworkResult<Vec<ReceiptRecord>> {
        let search = search_pattern(query.search);
        let limit = normalized_limit(query.limit);
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_INVENTORY_READ)
            .await?;
        let rows = sqlx::query(
            r#"SELECT r.id, r.receipt_no, supplier.display_name AS supplier_name,
                      owner.display_name AS owner_name, r.source_reference,
                      r.received_at::text AS received_at, r.status, COUNT(iu.id) AS item_count,
                      r.warranty_duration_days, r.warranty_label_snapshot,
                      r.warranty_started_at::text AS warranty_started_at,
                      r.warranty_expires_at::text AS warranty_expires_at
                 FROM inbound_receipts r
                 JOIN business_parties owner ON owner.tenant_id=r.tenant_id AND owner.id=r.owner_party_id
                 LEFT JOIN business_parties supplier ON supplier.tenant_id=r.tenant_id AND supplier.id=r.supplier_party_id
                 LEFT JOIN inbound_receipt_lines rl ON rl.tenant_id=r.tenant_id AND rl.receipt_id=r.id
                 LEFT JOIN inventory_units iu ON iu.tenant_id=rl.tenant_id AND iu.inbound_receipt_line_id=rl.id
                WHERE r.tenant_id=$1 AND ($2='%%' OR r.receipt_no ILIKE $2 OR COALESCE(supplier.display_name,'') ILIKE $2 OR owner.display_name ILIKE $2 OR COALESCE(r.source_reference,'') ILIKE $2 OR EXISTS (SELECT 1 FROM inbound_receipt_lines sx JOIN inventory_units ux ON ux.tenant_id=sx.tenant_id AND ux.inbound_receipt_line_id=sx.id WHERE sx.tenant_id=r.tenant_id AND sx.receipt_id=r.id AND ux.barcode ILIKE $2))
                GROUP BY r.id, supplier.display_name, owner.display_name
                ORDER BY r.received_at DESC, r.id DESC LIMIT $3"#,
        ).bind(tenant_id).bind(search).bind(limit).fetch_all(&mut **authorized.sqlx_transaction()).await?;
        let result = rows
            .into_iter()
            .map(receipt_record_postgres)
            .collect::<Result<Vec<_>, _>>()?;
        authorized.commit().await?;
        Ok(result)
    }

    pub async fn list_outbound_order_records_network(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        query: RecordSearchQuery,
    ) -> NetworkResult<Vec<OutboundOrderRecord>> {
        let search = search_pattern(query.search);
        let limit = normalized_limit(query.limit);
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_INVENTORY_READ)
            .await?;
        let rows = sqlx::query(
            r#"SELECT o.id, o.order_no, receiver.display_name AS receiver_name, o.status,
                      o.created_at::text AS created_at,
                      (SELECT s.shipment_no FROM outbound_shipments s WHERE s.tenant_id=o.tenant_id AND s.outbound_order_id=o.id ORDER BY s.shipped_at DESC,s.id DESC LIMIT 1) AS latest_shipment_no,
                      (SELECT s.shipped_at::text FROM outbound_shipments s WHERE s.tenant_id=o.tenant_id AND s.outbound_order_id=o.id ORDER BY s.shipped_at DESC,s.id DESC LIMIT 1) AS latest_shipped_at,
                      COUNT(DISTINCT a.id) AS item_count, COUNT(DISTINCT ret.id) AS returned_count
                 FROM outbound_orders o
                 JOIN business_parties receiver ON receiver.tenant_id=o.tenant_id AND receiver.id=o.upstream_receiver_id
                 LEFT JOIN outbound_order_lines ol ON ol.tenant_id=o.tenant_id AND ol.outbound_order_id=o.id
                 LEFT JOIN outbound_allocations a ON a.tenant_id=ol.tenant_id AND a.outbound_order_line_id=ol.id
                 LEFT JOIN outbound_shipment_lines sl ON sl.tenant_id=a.tenant_id AND sl.outbound_allocation_id=a.id
                 LEFT JOIN outbound_shipments ship ON ship.tenant_id=sl.tenant_id AND ship.id=sl.outbound_shipment_id
                 LEFT JOIN outbound_return_lines ret ON ret.tenant_id=sl.tenant_id AND ret.outbound_shipment_line_id=sl.id
                WHERE o.tenant_id=$1 AND ($2='%%' OR o.order_no ILIKE $2 OR receiver.display_name ILIKE $2 OR COALESCE(ship.shipment_no,'') ILIKE $2 OR COALESCE(sl.scanned_barcode_snapshot,'') ILIKE $2)
                GROUP BY o.id, receiver.display_name ORDER BY o.created_at DESC,o.id DESC LIMIT $3"#,
        ).bind(tenant_id).bind(search).bind(limit).fetch_all(&mut **authorized.sqlx_transaction()).await?;
        let result = rows
            .into_iter()
            .map(outbound_record_postgres)
            .collect::<Result<Vec<_>, _>>()?;
        authorized.commit().await?;
        Ok(result)
    }

    pub async fn receipt_document_network(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        receipt_id: Uuid,
    ) -> NetworkResult<ReceiptDocument> {
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_INVENTORY_READ)
            .await?;
        let transaction = authorized.sqlx_transaction();
        let row = sqlx::query(
            r#"SELECT r.id, r.receipt_no, supplier.display_name AS supplier_name,
                      owner.display_name AS owner_name, r.source_reference,
                      r.received_at::text AS received_at, r.status, COUNT(iu.id) AS item_count,
                      r.warranty_duration_days, r.warranty_label_snapshot,
                      r.warranty_started_at::text AS warranty_started_at,
                      r.warranty_expires_at::text AS warranty_expires_at
                 FROM inbound_receipts r
                 JOIN business_parties owner ON owner.tenant_id=r.tenant_id AND owner.id=r.owner_party_id
                 LEFT JOIN business_parties supplier ON supplier.tenant_id=r.tenant_id AND supplier.id=r.supplier_party_id
                 LEFT JOIN inbound_receipt_lines rl ON rl.tenant_id=r.tenant_id AND rl.receipt_id=r.id
                 LEFT JOIN inventory_units iu ON iu.tenant_id=rl.tenant_id AND iu.inbound_receipt_line_id=rl.id
                WHERE r.tenant_id=$1 AND r.id=$2
                GROUP BY r.id,supplier.display_name,owner.display_name"#,
        ).bind(tenant_id).bind(receipt_id).fetch_optional(&mut **transaction).await?
            .ok_or_else(|| super::network::NetworkServiceError::Invalid(format!("unknown receipt {receipt_id}")))?;
        let receipt = receipt_record_postgres(row)?;
        let items = sqlx::query(
            r#"SELECT sku.code AS sku_code,sku.name AS sku_name,iu.barcode,iu.inventory_status,
                      owner.display_name AS owner_name
                 FROM inbound_receipt_lines rl
                 JOIN inventory_units iu ON iu.tenant_id=rl.tenant_id AND iu.inbound_receipt_line_id=rl.id
                 JOIN skus sku ON sku.tenant_id=iu.tenant_id AND sku.id=iu.sku_id
                 JOIN business_parties owner ON owner.tenant_id=iu.tenant_id AND owner.id=iu.owner_party_id
                WHERE rl.tenant_id=$1 AND rl.receipt_id=$2 ORDER BY sku.code,iu.barcode"#,
        ).bind(tenant_id).bind(receipt_id).fetch_all(&mut **transaction).await?
            .into_iter().map(document_receipt_item_postgres).collect::<Result<Vec<_>,_>>()?;
        let void_info = load_receipt_void_postgres(transaction, tenant_id, receipt_id).await?;
        let void_eligibility =
            receipt_void_eligibility_postgres(transaction, tenant_id, receipt_id, &receipt.status)
                .await?;
        authorized.commit().await?;
        Ok(ReceiptDocument {
            receipt,
            items,
            void_info,
            void_eligibility,
        })
    }

    pub async fn outbound_order_document_network(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        order_id: Uuid,
    ) -> NetworkResult<OutboundOrderDocument> {
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_INVENTORY_READ)
            .await?;
        let transaction = authorized.sqlx_transaction();
        let row = sqlx::query(
            r#"SELECT o.id,o.order_no,receiver.display_name AS receiver_name,o.status,o.created_at::text AS created_at,
                      (SELECT s.shipment_no FROM outbound_shipments s WHERE s.tenant_id=o.tenant_id AND s.outbound_order_id=o.id ORDER BY s.shipped_at DESC,s.id DESC LIMIT 1) AS latest_shipment_no,
                      (SELECT s.shipped_at::text FROM outbound_shipments s WHERE s.tenant_id=o.tenant_id AND s.outbound_order_id=o.id ORDER BY s.shipped_at DESC,s.id DESC LIMIT 1) AS latest_shipped_at,
                      COUNT(DISTINCT a.id) AS item_count,COUNT(DISTINCT ret.id) AS returned_count
                 FROM outbound_orders o
                 JOIN business_parties receiver ON receiver.tenant_id=o.tenant_id AND receiver.id=o.upstream_receiver_id
                 LEFT JOIN outbound_order_lines ol ON ol.tenant_id=o.tenant_id AND ol.outbound_order_id=o.id
                 LEFT JOIN outbound_allocations a ON a.tenant_id=ol.tenant_id AND a.outbound_order_line_id=ol.id
                 LEFT JOIN outbound_shipment_lines sl ON sl.tenant_id=a.tenant_id AND sl.outbound_allocation_id=a.id
                 LEFT JOIN outbound_return_lines ret ON ret.tenant_id=sl.tenant_id AND ret.outbound_shipment_line_id=sl.id
                WHERE o.tenant_id=$1 AND o.id=$2 GROUP BY o.id,receiver.display_name"#,
        ).bind(tenant_id).bind(order_id).fetch_optional(&mut **transaction).await?
            .ok_or_else(|| super::network::NetworkServiceError::Invalid(format!("unknown outbound order {order_id}")))?;
        let order = outbound_record_postgres(row)?;
        let items = sqlx::query(outbound_document_postgres())
            .bind(tenant_id)
            .bind(order_id)
            .fetch_all(&mut **transaction)
            .await?
            .into_iter()
            .map(document_outbound_item_postgres)
            .collect::<Result<Vec<_>, _>>()?;
        let void_info = load_outbound_void_postgres(transaction, tenant_id, order_id).await?;
        let void_eligibility =
            outbound_void_eligibility_postgres(transaction, tenant_id, order_id, &order.status)
                .await?;
        authorized.commit().await?;
        Ok(OutboundOrderDocument {
            order,
            items,
            void_info,
            void_eligibility,
        })
    }

    pub async fn lookup_return_candidate_network(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        barcode: &str,
    ) -> NetworkResult<ReturnCandidate> {
        let barcode = barcode.trim().to_uppercase();
        if barcode.is_empty() {
            return Err(super::network::NetworkServiceError::Invalid(
                "barcode must not be empty".to_owned(),
            ));
        }
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_INVENTORY_READ)
            .await?;
        let row = sqlx::query(
            r#"SELECT iu.id AS inventory_unit_id,iu.barcode,sl.id AS shipment_line_id,ship.id AS shipment_id,
                      ship.shipment_no,ship.shipped_at::text AS shipped_at,
                      ship.warranty_duration_days,ship.warranty_label_snapshot,
                      ship.warranty_started_at::text AS warranty_started_at,ship.warranty_expires_at::text AS warranty_expires_at,
                      o.id AS order_id,o.order_no,receiver.display_name AS receiver_name
                 FROM inventory_units iu
                 JOIN outbound_shipment_lines sl ON sl.tenant_id=iu.tenant_id AND sl.inventory_unit_id=iu.id
                 JOIN outbound_shipments ship ON ship.tenant_id=sl.tenant_id AND ship.id=sl.outbound_shipment_id
                 JOIN outbound_orders o ON o.tenant_id=ship.tenant_id AND o.id=ship.outbound_order_id
                 JOIN business_parties receiver ON receiver.tenant_id=o.tenant_id AND receiver.id=o.upstream_receiver_id
                WHERE iu.tenant_id=$1 AND iu.barcode=$2 AND iu.inventory_status IN ('shipped','delivered')
                  AND NOT EXISTS (SELECT 1 FROM outbound_return_lines ret WHERE ret.tenant_id=sl.tenant_id AND ret.outbound_shipment_line_id=sl.id)
                ORDER BY ship.shipped_at DESC,sl.id DESC LIMIT 1"#,
        ).bind(tenant_id).bind(&barcode).fetch_optional(&mut **authorized.sqlx_transaction()).await?
            .ok_or_else(|| super::network::NetworkServiceError::Invalid(format!("barcode {barcode} has no returnable shipment")))?;
        let result = return_candidate_postgres(row)?;
        authorized.commit().await?;
        Ok(result)
    }
}

fn warranty_from_postgres(
    row: &sqlx::postgres::PgRow,
) -> Result<Option<WarrantyTerms>, sqlx::Error> {
    let Some(duration) = row.try_get::<Option<i32>, _>("warranty_duration_days")? else {
        return Ok(None);
    };
    Ok(Some(WarrantyTerms {
        duration_days: duration as u32,
        label_snapshot: row.try_get("warranty_label_snapshot")?,
        starts_at: row.try_get("warranty_started_at")?,
        expires_at: row.try_get("warranty_expires_at")?,
    }))
}

async fn load_receipt_void_postgres(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    receipt_id: Uuid,
) -> NetworkResult<Option<DocumentVoidInfo>> {
    load_document_void_postgres(transaction, tenant_id, "inbound_receipt_id", receipt_id).await
}

async fn load_outbound_void_postgres(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    order_id: Uuid,
) -> NetworkResult<Option<DocumentVoidInfo>> {
    load_document_void_postgres(transaction, tenant_id, "outbound_order_id", order_id).await
}

async fn load_document_void_postgres(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    field: &str,
    document_id: Uuid,
) -> NetworkResult<Option<DocumentVoidInfo>> {
    let query = format!(
        "SELECT reason, actor_id, voided_at::text AS voided_at FROM document_voids WHERE tenant_id=$1 AND {field}=$2"
    );
    let row = sqlx::query(&query)
        .bind(tenant_id)
        .bind(document_id)
        .fetch_optional(&mut **transaction)
        .await?;
    row.map(|row| -> Result<DocumentVoidInfo, sqlx::Error> {
        Ok(DocumentVoidInfo {
            reason: row.try_get("reason")?,
            actor_id: row.try_get::<Uuid, _>("actor_id")?.to_string(),
            voided_at: row.try_get("voided_at")?,
        })
    })
    .transpose()
    .map_err(NetworkServiceError::from)
}

async fn receipt_void_eligibility_postgres(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    receipt_id: Uuid,
    status: &str,
) -> NetworkResult<DocumentVoidEligibility> {
    if status == "voided" {
        return Ok(DocumentVoidEligibility {
            can_void: false,
            blockers: vec!["该收货单已经作废".to_owned()],
        });
    }
    if status != "posted" {
        return Ok(DocumentVoidEligibility {
            can_void: false,
            blockers: vec![format!("当前单据状态为 {status}")],
        });
    }
    let rows = sqlx::query(
        r#"SELECT iu.barcode,iu.inventory_status,
                  EXISTS (SELECT 1 FROM outbound_allocations oa WHERE oa.tenant_id=iu.tenant_id AND oa.inventory_unit_id=iu.id) AS has_outbound
             FROM inbound_receipt_lines rl JOIN inventory_units iu ON iu.tenant_id=rl.tenant_id AND iu.inbound_receipt_line_id=rl.id
            WHERE rl.tenant_id=$1 AND rl.receipt_id=$2 ORDER BY iu.barcode"#,
    )
    .bind(tenant_id)
    .bind(receipt_id)
    .fetch_all(&mut **transaction)
    .await?;
    let mut blockers = Vec::new();
    for row in rows {
        let barcode: String = row.try_get("barcode")?;
        let unit_status: String = row.try_get("inventory_status")?;
        if row.try_get::<bool, _>("has_outbound")? {
            blockers.push(format!("SN {barcode} 已关联出库业务"));
        } else if !matches!(
            unit_status.as_str(),
            "received" | "available" | "quarantined"
        ) {
            blockers.push(format!("SN {barcode} 当前状态为 {unit_status}"));
        }
    }
    Ok(DocumentVoidEligibility {
        can_void: blockers.is_empty(),
        blockers,
    })
}

async fn outbound_void_eligibility_postgres(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    order_id: Uuid,
    status: &str,
) -> NetworkResult<DocumentVoidEligibility> {
    if status == "voided" {
        return Ok(DocumentVoidEligibility {
            can_void: false,
            blockers: vec!["该出库订单已经作废".to_owned()],
        });
    }
    let barcodes: Vec<String> = sqlx::query_scalar(r#"SELECT sl.scanned_barcode_snapshot FROM outbound_shipments ship JOIN outbound_shipment_lines sl ON sl.tenant_id=ship.tenant_id AND sl.outbound_shipment_id=ship.id LEFT JOIN outbound_return_lines ret ON ret.tenant_id=sl.tenant_id AND ret.outbound_shipment_line_id=sl.id WHERE ship.tenant_id=$1 AND ship.outbound_order_id=$2 AND ret.id IS NULL ORDER BY sl.scanned_barcode_snapshot"#)
        .bind(tenant_id).bind(order_id).fetch_all(&mut **transaction).await?;
    let blockers = barcodes
        .into_iter()
        .map(|barcode| format!("SN {barcode} 已出库但尚未退回"))
        .collect::<Vec<_>>();
    Ok(DocumentVoidEligibility {
        can_void: blockers.is_empty(),
        blockers,
    })
}

fn receipt_record_postgres(row: sqlx::postgres::PgRow) -> Result<ReceiptRecord, sqlx::Error> {
    Ok(ReceiptRecord {
        receipt_id: row.try_get::<Uuid, _>("id")?.to_string(),
        receipt_no: row.try_get("receipt_no")?,
        supplier_name: row.try_get("supplier_name")?,
        owner_name: row.try_get("owner_name")?,
        source_reference: row.try_get("source_reference")?,
        received_at: row.try_get("received_at")?,
        status: row.try_get("status")?,
        item_count: row.try_get::<i64, _>("item_count")? as u32,
        warranty: warranty_from_postgres(&row)?,
    })
}

fn outbound_record_postgres(
    row: sqlx::postgres::PgRow,
) -> Result<OutboundOrderRecord, sqlx::Error> {
    Ok(OutboundOrderRecord {
        order_id: row.try_get::<Uuid, _>("id")?.to_string(),
        order_no: row.try_get("order_no")?,
        receiver_name: row.try_get("receiver_name")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        latest_shipment_no: row.try_get("latest_shipment_no")?,
        latest_shipped_at: row.try_get("latest_shipped_at")?,
        item_count: row.try_get::<i64, _>("item_count")? as u32,
        returned_count: row.try_get::<i64, _>("returned_count")? as u32,
    })
}

fn document_receipt_item_postgres(row: sqlx::postgres::PgRow) -> Result<DocumentItem, sqlx::Error> {
    Ok(DocumentItem {
        sku_code: row.try_get("sku_code")?,
        sku_name: row.try_get("sku_name")?,
        barcode: row.try_get("barcode")?,
        inventory_status: row.try_get("inventory_status")?,
        allocation_status: None,
        owner_name: row.try_get("owner_name")?,
        shipment_id: None,
        shipment_line_id: None,
        shipment_no: None,
        shipped_at: None,
        warranty: None,
        return_no: None,
        returned_at: None,
        return_reason: None,
        return_disposition: None,
    })
}

fn outbound_document_postgres() -> &'static str {
    r#"SELECT sku.code AS sku_code,sku.name AS sku_name,iu.barcode,iu.inventory_status,
              a.status AS allocation_status,owner.display_name AS owner_name,
              ship.id AS shipment_id,sl.id AS shipment_line_id,ship.shipment_no,ship.shipped_at::text AS shipped_at,
              ship.warranty_duration_days,ship.warranty_label_snapshot,
              ship.warranty_started_at::text AS warranty_started_at,ship.warranty_expires_at::text AS warranty_expires_at,
              rb.return_no,rb.returned_at::text AS returned_at,ret.reason AS return_reason,ret.disposition AS return_disposition
         FROM outbound_order_lines ol
         JOIN outbound_allocations a ON a.tenant_id=ol.tenant_id AND a.outbound_order_line_id=ol.id
         JOIN inventory_units iu ON iu.tenant_id=a.tenant_id AND iu.id=a.inventory_unit_id
         JOIN skus sku ON sku.tenant_id=iu.tenant_id AND sku.id=iu.sku_id
         JOIN business_parties owner ON owner.tenant_id=iu.tenant_id AND owner.id=iu.owner_party_id
         LEFT JOIN outbound_shipment_lines sl ON sl.tenant_id=a.tenant_id AND sl.outbound_allocation_id=a.id
         LEFT JOIN outbound_shipments ship ON ship.tenant_id=sl.tenant_id AND ship.id=sl.outbound_shipment_id
         LEFT JOIN outbound_return_lines ret ON ret.tenant_id=sl.tenant_id AND ret.outbound_shipment_line_id=sl.id
         LEFT JOIN outbound_return_batches rb ON rb.tenant_id=ret.tenant_id AND rb.id=ret.return_batch_id
        WHERE ol.tenant_id=$1 AND ol.outbound_order_id=$2 ORDER BY ship.shipped_at,sku.code,iu.barcode"#
}

fn document_outbound_item_postgres(
    row: sqlx::postgres::PgRow,
) -> Result<DocumentItem, sqlx::Error> {
    Ok(DocumentItem {
        sku_code: row.try_get("sku_code")?,
        sku_name: row.try_get("sku_name")?,
        barcode: row.try_get("barcode")?,
        inventory_status: row.try_get("inventory_status")?,
        allocation_status: row.try_get("allocation_status")?,
        owner_name: row.try_get("owner_name")?,
        shipment_id: row
            .try_get::<Option<Uuid>, _>("shipment_id")?
            .map(|v| v.to_string()),
        shipment_line_id: row
            .try_get::<Option<Uuid>, _>("shipment_line_id")?
            .map(|v| v.to_string()),
        shipment_no: row.try_get("shipment_no")?,
        shipped_at: row.try_get("shipped_at")?,
        warranty: warranty_from_postgres(&row)?,
        return_no: row.try_get("return_no")?,
        returned_at: row.try_get("returned_at")?,
        return_reason: row.try_get("return_reason")?,
        return_disposition: row.try_get("return_disposition")?,
    })
}

fn return_candidate_postgres(row: sqlx::postgres::PgRow) -> Result<ReturnCandidate, sqlx::Error> {
    Ok(ReturnCandidate {
        barcode: row.try_get("barcode")?,
        inventory_unit_id: row.try_get::<Uuid, _>("inventory_unit_id")?.to_string(),
        shipment_id: row.try_get::<Uuid, _>("shipment_id")?.to_string(),
        shipment_line_id: row.try_get::<Uuid, _>("shipment_line_id")?.to_string(),
        shipment_no: row.try_get("shipment_no")?,
        shipped_at: row.try_get("shipped_at")?,
        order_id: row.try_get::<Uuid, _>("order_id")?.to_string(),
        order_no: row.try_get("order_no")?,
        receiver_name: row.try_get("receiver_name")?,
        warranty: warranty_from_postgres(&row)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::{open_workbook, Reader, Xlsx};

    #[tokio::test]
    async fn document_lookup_is_not_limited_to_the_latest_five_hundred_records() {
        let path = std::env::temp_dir().join(format!("record-lookup-{}.sqlite3", Uuid::now_v7()));
        let database = OfflineDatabase::open(&path).await.expect("open database");
        let workspace_id = database.workspace_id().to_owned();
        let party_id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO business_parties (id, workspace_id, normalized_name, display_name, created_at) VALUES (?1, ?2, 'history-party', '历史往来方', '2026-01-01T00:00:00Z')",
        )
        .bind(&party_id)
        .bind(&workspace_id)
        .execute(database.pool())
        .await
        .expect("insert party");

        let mut transaction = database.pool().begin().await.expect("begin seed");
        for index in 0..=500 {
            let receipt_id = format!("receipt-{index:04}");
            let order_id = format!("order-{index:04}");
            sqlx::query(
                "INSERT INTO inbound_receipts (id, workspace_id, receipt_no, owner_party_id, warehouse_id, received_at, status, actor_id, idempotency_key, request_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, '2026-01-01T00:00:00Z', 'posted', 'actor', ?6, ?7, '2026-01-01T00:00:00Z')",
            )
            .bind(&receipt_id)
            .bind(&workspace_id)
            .bind(format!("RK-{index:04}"))
            .bind(&party_id)
            .bind(database.warehouse_id())
            .bind(format!("receipt-key-{index:04}"))
            .bind(format!("receipt-request-{index:04}"))
            .execute(&mut *transaction)
            .await
            .expect("insert receipt");
            sqlx::query(
                "INSERT INTO outbound_orders (id, workspace_id, order_no, upstream_receiver_id, status, actor_id, idempotency_key, request_id, created_at) VALUES (?1, ?2, ?3, ?4, 'open', 'actor', ?5, ?6, '2026-01-01T00:00:00Z')",
            )
            .bind(&order_id)
            .bind(&workspace_id)
            .bind(format!("DD-{index:04}"))
            .bind(&party_id)
            .bind(format!("order-key-{index:04}"))
            .bind(format!("order-request-{index:04}"))
            .execute(&mut *transaction)
            .await
            .expect("insert order");
        }
        transaction.commit().await.expect("commit seed");

        let receipts = database
            .list_receipt_records(RecordSearchQuery {
                search: None,
                limit: Some(500),
            })
            .await
            .expect("list receipts");
        let orders = database
            .list_outbound_order_records(RecordSearchQuery {
                search: None,
                limit: Some(500),
            })
            .await
            .expect("list orders");
        assert!(!receipts
            .iter()
            .any(|record| record.receipt_id == "receipt-0000"));
        assert!(!orders.iter().any(|record| record.order_id == "order-0000"));

        let receipt = database
            .receipt_document("receipt-0000")
            .await
            .expect("open old receipt");
        let order = database
            .outbound_order_document("order-0000")
            .await
            .expect("open old order");
        assert_eq!(receipt.receipt.receipt_no, "RK-0000");
        assert_eq!(order.order.order_no, "DD-0000");

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
    fn outbound_workbook_keeps_all_shipped_serials_and_separates_returns() {
        let path = std::env::temp_dir().join(format!("outbound-document-{}.xlsx", Uuid::now_v7()));
        let terms = WarrantyTerms {
            duration_days: 30,
            label_snapshot: "一个月".to_owned(),
            starts_at: "2026-08-14T01:00:00Z".to_owned(),
            expires_at: "2026-09-13T01:00:00Z".to_owned(),
        };
        let items = ["A", "B", "C"]
            .into_iter()
            .map(|barcode| DocumentItem {
                sku_code: "RAM-32G".to_owned(),
                sku_name: "32G 内存".to_owned(),
                barcode: barcode.to_owned(),
                inventory_status: "delivered".to_owned(),
                allocation_status: Some("shipped".to_owned()),
                owner_name: Some("货主甲".to_owned()),
                shipment_id: Some("shipment-1".to_owned()),
                shipment_line_id: Some(format!("line-{barcode}")),
                shipment_no: Some("CK-001".to_owned()),
                shipped_at: Some("2026-08-14T01:00:00Z".to_owned()),
                warranty: Some(terms.clone()),
                return_no: (barcode == "B").then(|| "TH-001".to_owned()),
                returned_at: (barcode == "B").then(|| "2026-08-20T01:00:00Z".to_owned()),
                return_reason: (barcode == "B").then(|| "无法点亮".to_owned()),
                return_disposition: (barcode == "B").then(|| "quarantine".to_owned()),
            })
            .collect();
        let document = OutboundOrderDocument {
            order: OutboundOrderRecord {
                order_id: "order-1".to_owned(),
                order_no: "DD-001".to_owned(),
                receiver_name: "张三".to_owned(),
                status: "completed".to_owned(),
                created_at: "2026-08-14T00:50:00Z".to_owned(),
                latest_shipment_no: Some("CK-001".to_owned()),
                latest_shipped_at: Some("2026-08-14T01:00:00Z".to_owned()),
                item_count: 3,
                returned_count: 1,
            },
            items,
            void_info: None,
            void_eligibility: DocumentVoidEligibility {
                can_void: false,
                blockers: vec!["存在未退回商品".to_owned()],
            },
        };
        write_outbound_workbook(&path, &document).expect("write outbound workbook");
        let mut workbook: Xlsx<_> = open_workbook(&path).expect("open workbook");
        let outbound = workbook.worksheet_range("出库单").expect("outbound sheet");
        let outbound_text = outbound
            .rows()
            .flat_map(|row| row.iter())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("|");
        for expected in ["DD-001", "张三", "A", "B", "C", "一个月"] {
            assert!(outbound_text.contains(expected), "missing {expected}");
        }
        let returns = workbook.worksheet_range("售后记录").expect("returns sheet");
        let returns_text = returns
            .rows()
            .flat_map(|row| row.iter())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("|");
        assert!(returns_text.contains("B"));
        assert!(returns_text.contains("TH-001"));
        assert!(!returns_text.contains("line-A"));
        let _ = std::fs::remove_file(path);
    }
}
