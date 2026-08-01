use super::domain::{
    DomainError, InspectionKind, InventoryStatus, InventoryUnit, NewInventoryUnit, QualityOutcome,
    QualityStatus,
};
use super::sqlite::{now_utc, OfflineDatabase};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};
use std::collections::HashSet;
use thiserror::Error;
use uuid::Uuid;

const RECEIPT_SCOPE: &str = "post_inbound_receipt";
const INSPECTION_SCOPE: &str = "complete_quality_inspection";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ApplicationError {
    #[error("{field}: {message}")]
    Validation { field: String, message: String },
    #[error("{entity} not found: {key}")]
    NotFound { entity: String, key: String },
    #[error("{entity} conflict for {key}: {message}")]
    Conflict {
        entity: String,
        key: String,
        message: String,
    },
    #[error("idempotency key {key} was already used with another request in {scope}")]
    IdempotencyConflict { scope: String, key: String },
    #[error("domain rule rejected the operation: {message}")]
    Domain { message: String },
    #[error("storage operation failed: {message}")]
    Storage { message: String },
}

impl From<DomainError> for ApplicationError {
    fn from(error: DomainError) -> Self {
        Self::Domain {
            message: error.to_string(),
        }
    }
}

pub type ApplicationResult<T> = Result<T, ApplicationError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostReceiptRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub receipt_no: String,
    pub owner_name: String,
    pub sku_code: String,
    pub sku_name: String,
    pub source_reference: Option<String>,
    pub received_at: String,
    pub actor_id: String,
    pub barcodes: Vec<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptUnit {
    pub inventory_unit_id: String,
    pub barcode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostReceiptResponse {
    pub receipt_id: String,
    pub receipt_line_id: String,
    pub receipt_no: String,
    pub owner_party_id: String,
    pub sku_id: String,
    pub received_count: u32,
    pub units: Vec<ReceiptUnit>,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompleteInspectionRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub inspection_no: String,
    pub inspection_kind: InspectionKind,
    pub inspector_id: String,
    pub inspected_at: String,
    pub results: Vec<InspectionResultInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectionResultInput {
    pub barcode: String,
    pub outcome: QualityOutcome,
    #[serde(default)]
    pub defect_code: Option<String>,
    #[serde(default = "empty_json_object")]
    pub measurements: Value,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectedUnit {
    pub inventory_unit_id: String,
    pub barcode: String,
    pub outcome: QualityOutcome,
    pub inventory_status: InventoryStatus,
    pub quality_status: QualityStatus,
    pub location_id: String,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteInspectionResponse {
    pub inspection_id: String,
    pub inspection_no: String,
    pub inspected_count: u32,
    pub passed_count: u32,
    pub failed_count: u32,
    pub units: Vec<InspectedUnit>,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryListQuery {
    pub search: Option<String>,
    pub owner_party_id: Option<String>,
    pub sku_id: Option<String>,
    pub inventory_status: Option<InventoryStatus>,
    pub quality_status: Option<QualityStatus>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryListItem {
    pub inventory_unit_id: String,
    pub barcode: String,
    pub receipt_id: String,
    pub receipt_no: String,
    pub owner_party_id: String,
    pub owner_name: String,
    pub sku_id: String,
    pub sku_code: String,
    pub sku_name: String,
    pub location_id: String,
    pub location_code: String,
    pub location_name: String,
    pub inventory_status: InventoryStatus,
    pub quality_status: QualityStatus,
    pub version: u64,
    pub received_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryListResponse {
    pub items: Vec<InventoryListItem>,
    pub total: u64,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventorySummaryQuery {
    pub owner_party_id: Option<String>,
    pub sku_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryStatusSummary {
    pub received: u64,
    pub available: u64,
    pub reserved: u64,
    pub shipped: u64,
    pub delivered: u64,
    pub quarantined: u64,
    pub scrapped: u64,
    pub returned_to_owner: u64,
    pub voided: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityStatusSummary {
    pub untested: u64,
    pub testing: u64,
    pub passed: u64,
    pub failed: u64,
    pub waived: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventorySummaryResponse {
    pub total_units: u64,
    pub inventory: InventoryStatusSummary,
    pub quality: QualityStatusSummary,
}

impl OfflineDatabase {
    pub async fn post_receipt(
        &self,
        request: PostReceiptRequest,
    ) -> ApplicationResult<PostReceiptResponse> {
        let request = normalize_receipt_request(request)?;
        let request_hash = request_hash(&request)?;
        let workspace_id = self.workspace_id();
        let now = application_now()?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| storage("begin receipt transaction", error))?;

        ensure_workspace_writable(&mut transaction, workspace_id).await?;

        if let Some(mut response) = load_idempotent_response::<PostReceiptResponse>(
            &mut transaction,
            workspace_id,
            RECEIPT_SCOPE,
            &request.idempotency_key,
            &request_hash,
        )
        .await?
        {
            response.idempotent_replay = true;
            transaction
                .commit()
                .await
                .map_err(|error| storage("finish receipt replay", error))?;
            return Ok(response);
        }

        for barcode in &request.barcodes {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM inventory_units WHERE workspace_id = ?1 AND barcode = ?2)",
            )
            .bind(workspace_id)
            .bind(barcode)
            .fetch_one(&mut *transaction)
            .await
            .map_err(|error| storage("check receipt barcode", error))?;
            if exists {
                return Err(ApplicationError::Conflict {
                    entity: "inventory_barcode".to_owned(),
                    key: barcode.clone(),
                    message: "barcode already exists in this workspace".to_owned(),
                });
            }
        }

        let owner_party_id =
            lookup_or_create_owner(&mut transaction, workspace_id, &request.owner_name, &now)
                .await?;
        let sku_id = lookup_or_create_sku(
            &mut transaction,
            workspace_id,
            &request.sku_code,
            &request.sku_name,
            &now,
        )
        .await?;

        let receipt_id = new_id();
        let receipt_line_id = new_id();
        let mut receipt = super::domain::InboundReceipt::new(
            receipt_id.clone(),
            owner_party_id.clone(),
            self.warehouse_id().to_owned(),
            request.received_at.clone(),
            request.actor_id.clone(),
        )?;
        receipt.post()?;
        super::domain::InboundReceiptLine::new(
            receipt_line_id.clone(),
            receipt_id.clone(),
            sku_id.clone(),
            request.barcodes.len() as u32,
        )?;

        sqlx::query(
            r#"
            INSERT INTO inbound_receipts (
                id, workspace_id, receipt_no, owner_party_id, warehouse_id,
                source_reference, received_at, status, actor_id, idempotency_key,
                request_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'posted', ?8, ?9, ?10, ?11)
            "#,
        )
        .bind(&receipt_id)
        .bind(workspace_id)
        .bind(&request.receipt_no)
        .bind(&owner_party_id)
        .bind(self.warehouse_id())
        .bind(&request.source_reference)
        .bind(&request.received_at)
        .bind(&request.actor_id)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(|error| receipt_insert_error(&request.receipt_no, error))?;

        sqlx::query(
            r#"
            INSERT INTO inbound_receipt_lines (
                id, workspace_id, receipt_id, sku_id, declared_quantity,
                scanned_quantity, notes, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)
            "#,
        )
        .bind(&receipt_line_id)
        .bind(workspace_id)
        .bind(&receipt_id)
        .bind(&sku_id)
        .bind(request.barcodes.len() as i64)
        .bind(&request.notes)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage("insert receipt line", error))?;

        let mut units = Vec::with_capacity(request.barcodes.len());
        for barcode in &request.barcodes {
            let inventory_unit_id = new_id();
            let unit = InventoryUnit::receive(NewInventoryUnit {
                id: inventory_unit_id.clone(),
                barcode: barcode.clone(),
                inbound_receipt_line_id: receipt_line_id.clone(),
                owner_party_id: owner_party_id.clone(),
                sku_id: sku_id.clone(),
                location_id: self.receiving_location_id().to_owned(),
                received_at: request.received_at.clone(),
            })?;

            let insert = sqlx::query(
                r#"
                INSERT INTO inventory_units (
                    id, workspace_id, barcode, inbound_receipt_line_id, owner_party_id,
                    sku_id, location_id, inventory_status, quality_status, version,
                    received_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'received', 'untested', ?8, ?9, ?10)
                "#,
            )
            .bind(&unit.id)
            .bind(workspace_id)
            .bind(&unit.barcode)
            .bind(&unit.inbound_receipt_line_id)
            .bind(&unit.owner_party_id)
            .bind(&unit.sku_id)
            .bind(&unit.location_id)
            .bind(unit.version as i64)
            .bind(&unit.received_at)
            .bind(&now)
            .execute(&mut *transaction)
            .await;
            if let Err(error) = insert {
                return Err(inventory_insert_error(barcode, error));
            }

            sqlx::query(
                r#"
                INSERT INTO stock_movements (
                    id, workspace_id, inventory_unit_id, movement_type, from_location_id,
                    to_location_id, source_type, source_id, actor_id, occurred_at, created_at
                ) VALUES (?1, ?2, ?3, 'received', NULL, ?4, 'inbound_receipt', ?5, ?6, ?7, ?8)
                "#,
            )
            .bind(new_id())
            .bind(workspace_id)
            .bind(&unit.id)
            .bind(&unit.location_id)
            .bind(&receipt_id)
            .bind(&request.actor_id)
            .bind(&request.received_at)
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage("insert receipt stock movement", error))?;

            units.push(ReceiptUnit {
                inventory_unit_id,
                barcode: barcode.clone(),
            });
        }

        let response = PostReceiptResponse {
            receipt_id: receipt_id.clone(),
            receipt_line_id,
            receipt_no: request.receipt_no.clone(),
            owner_party_id,
            sku_id,
            received_count: units.len() as u32,
            units,
            idempotent_replay: false,
        };
        let details = json!({
            "receipt_no": request.receipt_no,
            "owner_party_id": response.owner_party_id,
            "sku_id": response.sku_id,
            "received_count": response.received_count,
        });
        insert_audit(
            &mut transaction,
            workspace_id,
            &request.actor_id,
            "inbound_receipt.posted",
            "inbound_receipt",
            &receipt_id,
            &request.request_id,
            details,
            &now,
        )
        .await?;
        save_idempotent_response(
            &mut transaction,
            workspace_id,
            RECEIPT_SCOPE,
            &request.idempotency_key,
            &request_hash,
            &response,
            &now,
        )
        .await?;

        transaction
            .commit()
            .await
            .map_err(|error| storage("commit receipt transaction", error))?;
        Ok(response)
    }

    pub async fn complete_inspection(
        &self,
        request: CompleteInspectionRequest,
    ) -> ApplicationResult<CompleteInspectionResponse> {
        let request = normalize_inspection_request(request)?;
        let request_hash = request_hash(&request)?;
        let workspace_id = self.workspace_id();
        let now = application_now()?;
        let inspection_id = new_id();
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| storage("begin inspection transaction", error))?;

        ensure_workspace_writable(&mut transaction, workspace_id).await?;

        if let Some(mut response) = load_idempotent_response::<CompleteInspectionResponse>(
            &mut transaction,
            workspace_id,
            INSPECTION_SCOPE,
            &request.idempotency_key,
            &request_hash,
        )
        .await?
        {
            response.idempotent_replay = true;
            transaction
                .commit()
                .await
                .map_err(|error| storage("finish inspection replay", error))?;
            return Ok(response);
        }

        let mut validated = Vec::with_capacity(request.results.len());
        for result in &request.results {
            let mut unit = load_inventory_unit(&mut transaction, workspace_id, &result.barcode)
                .await?
                .ok_or_else(|| ApplicationError::NotFound {
                    entity: "inventory_unit".to_owned(),
                    key: result.barcode.clone(),
                })?;
            let previous_location_id = unit.location_id.clone();
            let previous_version = unit.version;
            let mut domain_inspection = unit.begin_quality_inspection(
                inspection_id.clone(),
                request.inspection_kind,
                request.inspector_id.clone(),
                request.inspected_at.clone(),
            )?;
            unit.complete_quality_inspection(
                &mut domain_inspection,
                result.outcome,
                request.inspected_at.clone(),
                result.defect_code.clone(),
                result.notes.clone(),
            )?;
            unit.location_id = match result.outcome {
                QualityOutcome::Passed => self.storage_location_id().to_owned(),
                QualityOutcome::Failed => self.quarantine_location_id().to_owned(),
            };
            validated.push(ValidatedInspectionResult {
                input: result.clone(),
                unit,
                previous_location_id,
                previous_version,
            });
        }

        sqlx::query(
            r#"
            INSERT INTO quality_inspections (
                id, workspace_id, inspection_no, inspection_type, status, inspector_id,
                inspected_at, idempotency_key, request_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, 'completed', ?5, ?6, ?7, ?8, ?9)
            "#,
        )
        .bind(&inspection_id)
        .bind(workspace_id)
        .bind(&request.inspection_no)
        .bind(inspection_kind_name(request.inspection_kind))
        .bind(&request.inspector_id)
        .bind(&request.inspected_at)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(|error| inspection_insert_error(&request.inspection_no, error))?;

        let mut response_units = Vec::with_capacity(validated.len());
        let mut passed_count = 0_u32;
        let mut failed_count = 0_u32;
        for validated_result in validated {
            let result_id = new_id();
            let measurements_json = serde_json::to_string(&validated_result.input.measurements)
                .map_err(|error| storage("serialize inspection measurements", error))?;
            sqlx::query(
                r#"
                INSERT INTO quality_inspection_results (
                    id, workspace_id, inspection_id, inventory_unit_id, result,
                    defect_code, measurements_json, notes, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                "#,
            )
            .bind(result_id)
            .bind(workspace_id)
            .bind(&inspection_id)
            .bind(&validated_result.unit.id)
            .bind(quality_outcome_name(validated_result.input.outcome))
            .bind(clean_optional(validated_result.input.defect_code))
            .bind(measurements_json)
            .bind(clean_optional(validated_result.input.notes))
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage("insert inspection result", error))?;

            let update = sqlx::query(
                r#"
                UPDATE inventory_units
                SET location_id = ?1, inventory_status = ?2, quality_status = ?3,
                    version = ?4, updated_at = ?5
                WHERE workspace_id = ?6 AND id = ?7 AND version = ?8
                "#,
            )
            .bind(&validated_result.unit.location_id)
            .bind(validated_result.unit.inventory_status.to_string())
            .bind(validated_result.unit.quality_status.to_string())
            .bind(validated_result.unit.version as i64)
            .bind(&now)
            .bind(workspace_id)
            .bind(&validated_result.unit.id)
            .bind(validated_result.previous_version as i64)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage("update inspected inventory unit", error))?;
            if update.rows_affected() != 1 {
                return Err(ApplicationError::Conflict {
                    entity: "inventory_unit_version".to_owned(),
                    key: validated_result.input.barcode,
                    message: "inventory unit changed while the inspection was being completed"
                        .to_owned(),
                });
            }

            sqlx::query(
                r#"
                INSERT INTO stock_movements (
                    id, workspace_id, inventory_unit_id, movement_type, from_location_id,
                    to_location_id, source_type, source_id, actor_id, occurred_at, created_at
                ) VALUES (?1, ?2, ?3, 'moved', ?4, ?5, 'quality_inspection', ?6, ?7, ?8, ?9)
                "#,
            )
            .bind(new_id())
            .bind(workspace_id)
            .bind(&validated_result.unit.id)
            .bind(&validated_result.previous_location_id)
            .bind(&validated_result.unit.location_id)
            .bind(&inspection_id)
            .bind(&request.inspector_id)
            .bind(&request.inspected_at)
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage("insert inspection stock movement", error))?;

            match validated_result.input.outcome {
                QualityOutcome::Passed => passed_count += 1,
                QualityOutcome::Failed => failed_count += 1,
            }
            response_units.push(InspectedUnit {
                inventory_unit_id: validated_result.unit.id,
                barcode: validated_result.unit.barcode,
                outcome: validated_result.input.outcome,
                inventory_status: validated_result.unit.inventory_status,
                quality_status: validated_result.unit.quality_status,
                location_id: validated_result.unit.location_id,
                version: validated_result.unit.version,
            });
        }

        let response = CompleteInspectionResponse {
            inspection_id: inspection_id.clone(),
            inspection_no: request.inspection_no.clone(),
            inspected_count: response_units.len() as u32,
            passed_count,
            failed_count,
            units: response_units,
            idempotent_replay: false,
        };
        insert_audit(
            &mut transaction,
            workspace_id,
            &request.inspector_id,
            "quality_inspection.completed",
            "quality_inspection",
            &inspection_id,
            &request.request_id,
            json!({
                "inspection_no": request.inspection_no,
                "inspection_kind": inspection_kind_name(request.inspection_kind),
                "inspected_count": response.inspected_count,
                "passed_count": response.passed_count,
                "failed_count": response.failed_count,
            }),
            &now,
        )
        .await?;
        save_idempotent_response(
            &mut transaction,
            workspace_id,
            INSPECTION_SCOPE,
            &request.idempotency_key,
            &request_hash,
            &response,
            &now,
        )
        .await?;

        transaction
            .commit()
            .await
            .map_err(|error| storage("commit inspection transaction", error))?;
        Ok(response)
    }

    pub async fn list_inventory(
        &self,
        query: InventoryListQuery,
    ) -> ApplicationResult<InventoryListResponse> {
        let search = clean_optional(query.search).map(|value| format!("%{value}%"));
        let owner_party_id = clean_optional(query.owner_party_id);
        let sku_id = clean_optional(query.sku_id);
        let inventory_status = query.inventory_status.map(|value| value.to_string());
        let quality_status = query.quality_status.map(|value| value.to_string());
        let limit = query.limit.unwrap_or(100).clamp(1, 500);
        let offset = query.offset.unwrap_or(0);

        let total: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM inventory_units iu
            JOIN inbound_receipt_lines irl ON irl.id = iu.inbound_receipt_line_id
            JOIN inbound_receipts ir ON ir.id = irl.receipt_id
            JOIN business_parties bp ON bp.id = iu.owner_party_id
            JOIN skus s ON s.id = iu.sku_id
            WHERE iu.workspace_id = ?1
              AND (?2 IS NULL OR iu.barcode LIKE ?2 OR ir.receipt_no LIKE ?2
                   OR bp.display_name LIKE ?2 OR s.code LIKE ?2 OR s.name LIKE ?2)
              AND (?3 IS NULL OR iu.owner_party_id = ?3)
              AND (?4 IS NULL OR iu.sku_id = ?4)
              AND (?5 IS NULL OR iu.inventory_status = ?5)
              AND (?6 IS NULL OR iu.quality_status = ?6)
            "#,
        )
        .bind(self.workspace_id())
        .bind(&search)
        .bind(&owner_party_id)
        .bind(&sku_id)
        .bind(&inventory_status)
        .bind(&quality_status)
        .fetch_one(self.pool())
        .await
        .map_err(|error| storage("count inventory units", error))?;

        let rows = sqlx::query(
            r#"
            SELECT
                iu.id AS inventory_unit_id, iu.barcode, ir.id AS receipt_id, ir.receipt_no,
                iu.owner_party_id, bp.display_name AS owner_name,
                iu.sku_id, s.code AS sku_code, s.name AS sku_name,
                iu.location_id, l.code AS location_code, l.name AS location_name,
                iu.inventory_status, iu.quality_status, iu.version,
                iu.received_at, iu.updated_at
            FROM inventory_units iu
            JOIN inbound_receipt_lines irl ON irl.id = iu.inbound_receipt_line_id
            JOIN inbound_receipts ir ON ir.id = irl.receipt_id
            JOIN business_parties bp ON bp.id = iu.owner_party_id
            JOIN skus s ON s.id = iu.sku_id
            JOIN locations l ON l.id = iu.location_id
            WHERE iu.workspace_id = ?1
              AND (?2 IS NULL OR iu.barcode LIKE ?2 OR ir.receipt_no LIKE ?2
                   OR bp.display_name LIKE ?2 OR s.code LIKE ?2 OR s.name LIKE ?2)
              AND (?3 IS NULL OR iu.owner_party_id = ?3)
              AND (?4 IS NULL OR iu.sku_id = ?4)
              AND (?5 IS NULL OR iu.inventory_status = ?5)
              AND (?6 IS NULL OR iu.quality_status = ?6)
            ORDER BY iu.received_at DESC, iu.id DESC
            LIMIT ?7 OFFSET ?8
            "#,
        )
        .bind(self.workspace_id())
        .bind(&search)
        .bind(&owner_party_id)
        .bind(&sku_id)
        .bind(&inventory_status)
        .bind(&quality_status)
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(self.pool())
        .await
        .map_err(|error| storage("list inventory units", error))?;

        let items = rows
            .into_iter()
            .map(inventory_list_item_from_row)
            .collect::<ApplicationResult<Vec<_>>>()?;
        Ok(InventoryListResponse {
            items,
            total: nonnegative_u64("inventory total", total)?,
            limit,
            offset,
        })
    }

    pub async fn inventory_summary(
        &self,
        query: InventorySummaryQuery,
    ) -> ApplicationResult<InventorySummaryResponse> {
        let owner_party_id = clean_optional(query.owner_party_id);
        let sku_id = clean_optional(query.sku_id);
        let rows = sqlx::query(
            r#"
            SELECT inventory_status, quality_status, COUNT(*) AS unit_count
            FROM inventory_units
            WHERE workspace_id = ?1
              AND (?2 IS NULL OR owner_party_id = ?2)
              AND (?3 IS NULL OR sku_id = ?3)
            GROUP BY inventory_status, quality_status
            "#,
        )
        .bind(self.workspace_id())
        .bind(&owner_party_id)
        .bind(&sku_id)
        .fetch_all(self.pool())
        .await
        .map_err(|error| storage("summarize inventory", error))?;

        let mut summary = InventorySummaryResponse::default();
        for row in rows {
            let count = nonnegative_u64("inventory group count", row.try_get("unit_count")?)?;
            let inventory_status = parse_inventory_status(row.try_get("inventory_status")?)?;
            let quality_status = parse_quality_status(row.try_get("quality_status")?)?;
            summary.total_units += count;
            summary.inventory.add(inventory_status, count);
            summary.quality.add(quality_status, count);
        }
        Ok(summary)
    }
}

impl InventoryStatusSummary {
    fn add(&mut self, status: InventoryStatus, count: u64) {
        match status {
            InventoryStatus::Received => self.received += count,
            InventoryStatus::Available => self.available += count,
            InventoryStatus::Reserved => self.reserved += count,
            InventoryStatus::Shipped => self.shipped += count,
            InventoryStatus::Delivered => self.delivered += count,
            InventoryStatus::Quarantined => self.quarantined += count,
            InventoryStatus::Scrapped => self.scrapped += count,
            InventoryStatus::ReturnedToOwner => self.returned_to_owner += count,
            InventoryStatus::Voided => self.voided += count,
        }
    }
}

impl QualityStatusSummary {
    fn add(&mut self, status: QualityStatus, count: u64) {
        match status {
            QualityStatus::Untested => self.untested += count,
            QualityStatus::Testing => self.testing += count,
            QualityStatus::Passed => self.passed += count,
            QualityStatus::Failed => self.failed += count,
            QualityStatus::Waived => self.waived += count,
        }
    }
}

#[derive(Debug)]
struct ValidatedInspectionResult {
    input: InspectionResultInput,
    unit: InventoryUnit,
    previous_location_id: String,
    previous_version: u64,
}

fn normalize_receipt_request(
    mut request: PostReceiptRequest,
) -> ApplicationResult<PostReceiptRequest> {
    request.request_id = required_text("request_id", request.request_id)?;
    request.idempotency_key = required_text("idempotency_key", request.idempotency_key)?;
    request.receipt_no = required_text("receipt_no", request.receipt_no)?.to_uppercase();
    request.owner_name = normalized_display_name("owner_name", request.owner_name)?;
    request.sku_code = required_text("sku_code", request.sku_code)?.to_uppercase();
    request.sku_name = normalized_display_name("sku_name", request.sku_name)?;
    request.received_at = required_text("received_at", request.received_at)?;
    request.actor_id = required_text("actor_id", request.actor_id)?;
    request.source_reference = clean_optional(request.source_reference);
    request.notes = clean_optional(request.notes);
    if request.barcodes.is_empty() {
        return Err(validation("barcodes", "at least one barcode is required"));
    }
    let mut unique = HashSet::with_capacity(request.barcodes.len());
    for (index, barcode) in request.barcodes.iter_mut().enumerate() {
        *barcode =
            required_text(&format!("barcodes[{index}]"), std::mem::take(barcode))?.to_uppercase();
        if !unique.insert(barcode.clone()) {
            return Err(ApplicationError::Conflict {
                entity: "inventory_barcode".to_owned(),
                key: barcode.clone(),
                message: "barcode is duplicated inside the receipt batch".to_owned(),
            });
        }
    }
    Ok(request)
}

fn normalize_inspection_request(
    mut request: CompleteInspectionRequest,
) -> ApplicationResult<CompleteInspectionRequest> {
    request.request_id = required_text("request_id", request.request_id)?;
    request.idempotency_key = required_text("idempotency_key", request.idempotency_key)?;
    request.inspection_no = required_text("inspection_no", request.inspection_no)?.to_uppercase();
    request.inspector_id = required_text("inspector_id", request.inspector_id)?;
    request.inspected_at = required_text("inspected_at", request.inspected_at)?;
    if request.results.is_empty() {
        return Err(validation(
            "results",
            "at least one inspection result is required",
        ));
    }
    let mut unique = HashSet::with_capacity(request.results.len());
    for (index, result) in request.results.iter_mut().enumerate() {
        result.barcode = required_text(
            &format!("results[{index}].barcode"),
            std::mem::take(&mut result.barcode),
        )?
        .to_uppercase();
        result.defect_code = clean_optional(result.defect_code.take());
        result.notes = clean_optional(result.notes.take());
        if !result.measurements.is_object() {
            return Err(validation(
                &format!("results[{index}].measurements"),
                "measurements must be a JSON object",
            ));
        }
        if !unique.insert(result.barcode.clone()) {
            return Err(ApplicationError::Conflict {
                entity: "inspection_barcode".to_owned(),
                key: result.barcode.clone(),
                message: "barcode is duplicated inside the inspection batch".to_owned(),
            });
        }
    }
    Ok(request)
}

async fn lookup_or_create_owner(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    display_name: &str,
    now: &str,
) -> ApplicationResult<String> {
    let normalized_name = display_name.to_lowercase();
    let candidate_id = new_id();
    sqlx::query(
        r#"
        INSERT INTO business_parties (id, workspace_id, normalized_name, display_name, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT (workspace_id, normalized_name) DO NOTHING
        "#,
    )
    .bind(&candidate_id)
    .bind(workspace_id)
    .bind(&normalized_name)
    .bind(display_name)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("lookup or create goods owner", error))?;

    let party_id: String = sqlx::query_scalar(
        "SELECT id FROM business_parties WHERE workspace_id = ?1 AND normalized_name = ?2",
    )
    .bind(workspace_id)
    .bind(&normalized_name)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| storage("load goods owner", error))?;
    sqlx::query(
        r#"
        INSERT INTO party_roles (workspace_id, party_id, role, created_at)
        VALUES (?1, ?2, 'goods_owner', ?3)
        ON CONFLICT (workspace_id, party_id, role) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(&party_id)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("ensure goods owner role", error))?;
    Ok(party_id)
}

async fn lookup_or_create_sku(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    code: &str,
    name: &str,
    now: &str,
) -> ApplicationResult<String> {
    let candidate_id = new_id();
    sqlx::query(
        r#"
        INSERT INTO skus (id, workspace_id, code, name, tracking_mode, active, created_at)
        VALUES (?1, ?2, ?3, ?4, 'serial', 1, ?5)
        ON CONFLICT (workspace_id, code) DO NOTHING
        "#,
    )
    .bind(&candidate_id)
    .bind(workspace_id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("lookup or create SKU", error))?;
    sqlx::query_scalar("SELECT id FROM skus WHERE workspace_id = ?1 AND code = ?2")
        .bind(workspace_id)
        .bind(code)
        .fetch_one(&mut **transaction)
        .await
        .map_err(|error| storage("load SKU", error))
}

async fn load_inventory_unit(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    barcode: &str,
) -> ApplicationResult<Option<InventoryUnit>> {
    let row = sqlx::query(
        r#"
        SELECT
            iu.id, iu.barcode, iu.inbound_receipt_line_id, iu.owner_party_id,
            iu.sku_id, iu.location_id, iu.received_at, iu.inventory_status,
            iu.quality_status, iu.version,
            (
                SELECT oa.id FROM outbound_allocations oa
                WHERE oa.workspace_id = iu.workspace_id
                  AND oa.inventory_unit_id = iu.id
                  AND oa.status = 'active'
                LIMIT 1
            ) AS active_allocation_id,
            (
                SELECT osl.id FROM outbound_shipment_lines osl
                JOIN outbound_shipments os ON os.id = osl.outbound_shipment_id
                WHERE osl.workspace_id = iu.workspace_id
                  AND osl.inventory_unit_id = iu.id
                ORDER BY os.shipped_at DESC, osl.id DESC
                LIMIT 1
            ) AS latest_shipment_line_id
        FROM inventory_units iu
        WHERE iu.workspace_id = ?1 AND iu.barcode = ?2
        "#,
    )
    .bind(workspace_id)
    .bind(barcode)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| storage("load inventory unit for inspection", error))?;

    row.map(|row| {
        let version: i64 = row.try_get("version")?;
        Ok(InventoryUnit {
            id: row.try_get("id")?,
            barcode: row.try_get("barcode")?,
            inbound_receipt_line_id: row.try_get("inbound_receipt_line_id")?,
            owner_party_id: row.try_get("owner_party_id")?,
            sku_id: row.try_get("sku_id")?,
            location_id: row.try_get("location_id")?,
            received_at: row.try_get("received_at")?,
            inventory_status: parse_inventory_status(row.try_get("inventory_status")?)?,
            quality_status: parse_quality_status(row.try_get("quality_status")?)?,
            active_allocation_id: row.try_get("active_allocation_id")?,
            latest_shipment_line_id: row.try_get("latest_shipment_line_id")?,
            version: nonnegative_u64("inventory version", version)?,
        })
    })
    .transpose()
}

fn inventory_list_item_from_row(
    row: sqlx::sqlite::SqliteRow,
) -> ApplicationResult<InventoryListItem> {
    let version: i64 = row.try_get("version")?;
    Ok(InventoryListItem {
        inventory_unit_id: row.try_get("inventory_unit_id")?,
        barcode: row.try_get("barcode")?,
        receipt_id: row.try_get("receipt_id")?,
        receipt_no: row.try_get("receipt_no")?,
        owner_party_id: row.try_get("owner_party_id")?,
        owner_name: row.try_get("owner_name")?,
        sku_id: row.try_get("sku_id")?,
        sku_code: row.try_get("sku_code")?,
        sku_name: row.try_get("sku_name")?,
        location_id: row.try_get("location_id")?,
        location_code: row.try_get("location_code")?,
        location_name: row.try_get("location_name")?,
        inventory_status: parse_inventory_status(row.try_get("inventory_status")?)?,
        quality_status: parse_quality_status(row.try_get("quality_status")?)?,
        version: nonnegative_u64("inventory version", version)?,
        received_at: row.try_get("received_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

async fn load_idempotent_response<T: DeserializeOwned>(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    scope: &str,
    idempotency_key: &str,
    request_hash: &str,
) -> ApplicationResult<Option<T>> {
    let row = sqlx::query(
        r#"
        SELECT request_hash, response_json
        FROM idempotency_records
        WHERE workspace_id = ?1 AND scope = ?2 AND idempotency_key = ?3
        "#,
    )
    .bind(workspace_id)
    .bind(scope)
    .bind(idempotency_key)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| storage("load idempotency result", error))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored_hash: String = row.try_get("request_hash")?;
    if stored_hash != request_hash {
        return Err(ApplicationError::IdempotencyConflict {
            scope: scope.to_owned(),
            key: idempotency_key.to_owned(),
        });
    }
    let response_json: String = row.try_get("response_json")?;
    let response = serde_json::from_str(&response_json)
        .map_err(|error| storage("decode idempotency response", error))?;
    Ok(Some(response))
}

async fn save_idempotent_response<T: Serialize>(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    scope: &str,
    idempotency_key: &str,
    request_hash: &str,
    response: &T,
    now: &str,
) -> ApplicationResult<()> {
    let response_json = serde_json::to_string(response)
        .map_err(|error| storage("encode idempotency response", error))?;
    sqlx::query(
        r#"
        INSERT INTO idempotency_records (
            id, workspace_id, scope, idempotency_key, request_hash, response_json, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        "#,
    )
    .bind(new_id())
    .bind(workspace_id)
    .bind(scope)
    .bind(idempotency_key)
    .bind(request_hash)
    .bind(response_json)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("save idempotency result", error))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_audit(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    actor_id: &str,
    action: &str,
    entity_type: &str,
    entity_id: &str,
    request_id: &str,
    details: Value,
    occurred_at: &str,
) -> ApplicationResult<()> {
    let details_json =
        serde_json::to_string(&details).map_err(|error| storage("encode audit details", error))?;
    sqlx::query(
        r#"
        INSERT INTO audit_logs (
            id, workspace_id, actor_id, action, entity_type, entity_id,
            request_id, result, details_json, occurred_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'success', ?8, ?9)
        "#,
    )
    .bind(new_id())
    .bind(workspace_id)
    .bind(actor_id)
    .bind(action)
    .bind(entity_type)
    .bind(entity_id)
    .bind(request_id)
    .bind(details_json)
    .bind(occurred_at)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("insert audit log", error))?;
    Ok(())
}

fn request_hash<T: Serialize>(request: &T) -> ApplicationResult<String> {
    let bytes = serde_json::to_vec(request)
        .map_err(|error| storage("encode request for idempotency", error))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn inspection_kind_name(kind: InspectionKind) -> &'static str {
    match kind {
        InspectionKind::Initial => "initial",
        InspectionKind::Retest => "retest",
    }
}

fn quality_outcome_name(outcome: QualityOutcome) -> &'static str {
    match outcome {
        QualityOutcome::Passed => "passed",
        QualityOutcome::Failed => "failed",
    }
}

fn parse_inventory_status(value: String) -> ApplicationResult<InventoryStatus> {
    match value.as_str() {
        "received" => Ok(InventoryStatus::Received),
        "available" => Ok(InventoryStatus::Available),
        "reserved" => Ok(InventoryStatus::Reserved),
        "shipped" => Ok(InventoryStatus::Shipped),
        "delivered" => Ok(InventoryStatus::Delivered),
        "quarantined" => Ok(InventoryStatus::Quarantined),
        "scrapped" => Ok(InventoryStatus::Scrapped),
        "returned_to_owner" => Ok(InventoryStatus::ReturnedToOwner),
        "voided" => Ok(InventoryStatus::Voided),
        _ => Err(storage(
            "decode inventory status",
            format!("unknown value {value}"),
        )),
    }
}

fn parse_quality_status(value: String) -> ApplicationResult<QualityStatus> {
    match value.as_str() {
        "untested" => Ok(QualityStatus::Untested),
        "testing" => Ok(QualityStatus::Testing),
        "passed" => Ok(QualityStatus::Passed),
        "failed" => Ok(QualityStatus::Failed),
        "waived" => Ok(QualityStatus::Waived),
        _ => Err(storage(
            "decode quality status",
            format!("unknown value {value}"),
        )),
    }
}

fn nonnegative_u64(field: &str, value: i64) -> ApplicationResult<u64> {
    u64::try_from(value)
        .map_err(|_| storage("decode database number", format!("{field} is {value}")))
}

fn required_text(field: &str, value: String) -> ApplicationResult<String> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        Err(validation(field, "value must not be empty"))
    } else {
        Ok(value)
    }
}

fn normalized_display_name(field: &str, value: String) -> ApplicationResult<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        Err(validation(field, "value must not be empty"))
    } else {
        Ok(value)
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_owned();
        (!value.is_empty()).then_some(value)
    })
}

fn empty_json_object() -> Value {
    json!({})
}

fn new_id() -> String {
    Uuid::now_v7().to_string()
}

fn application_now() -> ApplicationResult<String> {
    now_utc().map_err(|message| ApplicationError::Storage { message })
}

fn validation(field: &str, message: &str) -> ApplicationError {
    ApplicationError::Validation {
        field: field.to_owned(),
        message: message.to_owned(),
    }
}

fn storage(context: &str, error: impl std::fmt::Display) -> ApplicationError {
    ApplicationError::Storage {
        message: format!("{context}: {error}"),
    }
}

async fn ensure_workspace_writable(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
) -> ApplicationResult<()> {
    let read_only: i64 = sqlx::query_scalar("SELECT read_only FROM workspaces WHERE id = ?1")
        .bind(workspace_id)
        .fetch_one(&mut **transaction)
        .await
        .map_err(|error| storage("read workspace write mode", error))?;
    if read_only != 0 {
        return Err(ApplicationError::Conflict {
            entity: "workspace".to_owned(),
            key: workspace_id.to_owned(),
            message: "offline workspace is archived and read-only after one-time upgrade"
                .to_owned(),
        });
    }
    Ok(())
}

fn inventory_insert_error(barcode: &str, error: sqlx::Error) -> ApplicationError {
    if error
        .as_database_error()
        .is_some_and(|database_error| database_error.is_unique_violation())
    {
        ApplicationError::Conflict {
            entity: "inventory_barcode".to_owned(),
            key: barcode.to_owned(),
            message: "barcode already exists in this workspace".to_owned(),
        }
    } else {
        storage("insert inventory unit", error)
    }
}

fn receipt_insert_error(receipt_no: &str, error: sqlx::Error) -> ApplicationError {
    if error
        .as_database_error()
        .is_some_and(|database_error| database_error.is_unique_violation())
    {
        ApplicationError::Conflict {
            entity: "inbound_receipt".to_owned(),
            key: receipt_no.to_owned(),
            message: "receipt number or idempotency key already exists".to_owned(),
        }
    } else {
        storage("insert inbound receipt", error)
    }
}

fn inspection_insert_error(inspection_no: &str, error: sqlx::Error) -> ApplicationError {
    if error
        .as_database_error()
        .is_some_and(|database_error| database_error.is_unique_violation())
    {
        ApplicationError::Conflict {
            entity: "quality_inspection".to_owned(),
            key: inspection_no.to_owned(),
            message: "inspection number or idempotency key already exists".to_owned(),
        }
    } else {
        storage("insert quality inspection", error)
    }
}

impl From<sqlx::Error> for ApplicationError {
    fn from(error: sqlx::Error) -> Self {
        storage("read database row", error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn receipt_request(
        request_id: &str,
        idempotency_key: &str,
        receipt_no: &str,
        barcodes: &[&str],
    ) -> PostReceiptRequest {
        PostReceiptRequest {
            request_id: request_id.to_owned(),
            idempotency_key: idempotency_key.to_owned(),
            receipt_no: receipt_no.to_owned(),
            owner_name: "客户 A".to_owned(),
            sku_code: "model-x".to_owned(),
            sku_name: "型号 X".to_owned(),
            source_reference: Some("采购单 1".to_owned()),
            received_at: "2026-07-31T01:00:00Z".to_owned(),
            actor_id: "operator-1".to_owned(),
            barcodes: barcodes.iter().map(|value| (*value).to_owned()).collect(),
            notes: None,
        }
    }

    fn inspection_request(
        request_id: &str,
        idempotency_key: &str,
        inspection_no: &str,
        inspection_kind: InspectionKind,
        results: Vec<(&str, QualityOutcome)>,
    ) -> CompleteInspectionRequest {
        CompleteInspectionRequest {
            request_id: request_id.to_owned(),
            idempotency_key: idempotency_key.to_owned(),
            inspection_no: inspection_no.to_owned(),
            inspection_kind,
            inspector_id: "inspector-1".to_owned(),
            inspected_at: "2026-07-31T02:00:00Z".to_owned(),
            results: results
                .into_iter()
                .map(|(barcode, outcome)| InspectionResultInput {
                    barcode: barcode.to_owned(),
                    outcome,
                    defect_code: None,
                    measurements: json!({}),
                    notes: None,
                })
                .collect(),
        }
    }

    async fn test_database() -> (OfflineDatabase, PathBuf) {
        let path = std::env::temp_dir().join(format!("inventory-v2-{}.sqlite", new_id()));
        let database = OfflineDatabase::open(&path)
            .await
            .expect("open isolated test database");
        (database, path)
    }

    async fn close_and_remove(database: OfflineDatabase, path: PathBuf) {
        database.pool().close().await;
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = std::fs::remove_file(candidate);
        }
    }

    #[tokio::test]
    async fn duplicate_barcode_rolls_back_the_entire_receipt() {
        let (database, path) = test_database().await;
        database
            .post_receipt(receipt_request(
                "request-1",
                "receipt-key-1",
                "R-1",
                &["DUP-1"],
            ))
            .await
            .expect("seed existing barcode");

        let mut conflicting = receipt_request(
            "request-2",
            "receipt-key-2",
            "R-2",
            &["NEW-BARCODE", "dup-1"],
        );
        conflicting.owner_name = "客户 B".to_owned();
        conflicting.sku_code = "MODEL-Y".to_owned();
        conflicting.sku_name = "型号 Y".to_owned();
        let error = database
            .post_receipt(conflicting)
            .await
            .expect_err("the entire batch must be rejected");
        assert!(matches!(
            error,
            ApplicationError::Conflict { ref key, .. } if key == "DUP-1"
        ));

        let receipt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbound_receipts")
            .fetch_one(database.pool())
            .await
            .expect("count receipts");
        let unit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inventory_units")
            .fetch_one(database.pool())
            .await
            .expect("count units");
        let rolled_back_unit: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM inventory_units WHERE barcode = 'NEW-BARCODE'",
        )
        .fetch_one(database.pool())
        .await
        .expect("check rolled back unit");
        let rolled_back_owner: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM business_parties WHERE normalized_name = '客户 b'",
        )
        .fetch_one(database.pool())
        .await
        .expect("check rolled back owner");
        let rolled_back_sku: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM skus WHERE code = 'MODEL-Y'")
                .fetch_one(database.pool())
                .await
                .expect("check rolled back SKU");
        assert_eq!(receipt_count, 1);
        assert_eq!(unit_count, 1);
        assert_eq!(rolled_back_unit, 0);
        assert_eq!(rolled_back_owner, 0);
        assert_eq!(rolled_back_sku, 0);
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn receipt_replay_is_idempotent_and_units_default_to_received_untested() {
        let (database, path) = test_database().await;
        let request = receipt_request(
            "request-1",
            "receipt-key-1",
            "receipt-1",
            &[" item-1 ", "item-2"],
        );
        let first = database
            .post_receipt(request.clone())
            .await
            .expect("post receipt");
        let replay = database
            .post_receipt(request)
            .await
            .expect("replay receipt");
        assert!(!first.idempotent_replay);
        assert!(replay.idempotent_replay);
        assert_eq!(first.receipt_id, replay.receipt_id);
        assert_eq!(first.units, replay.units);

        let rows = sqlx::query(
            "SELECT barcode, inventory_status, quality_status, version, location_id FROM inventory_units ORDER BY barcode",
        )
        .fetch_all(database.pool())
        .await
        .expect("load inventory defaults");
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.get::<String, _>("inventory_status"), "received");
            assert_eq!(row.get::<String, _>("quality_status"), "untested");
            assert_eq!(row.get::<i64, _>("version"), 1);
            assert_eq!(
                row.get::<String, _>("location_id"),
                database.receiving_location_id()
            );
        }
        assert_eq!(rows[0].get::<String, _>("barcode"), "ITEM-1");

        let receipt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbound_receipts")
            .fetch_one(database.pool())
            .await
            .expect("count receipts");
        let movement_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(database.pool())
            .await
            .expect("count movements");
        let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs")
            .fetch_one(database.pool())
            .await
            .expect("count audits");
        assert_eq!(receipt_count, 1);
        assert_eq!(movement_count, 2);
        assert_eq!(audit_count, 1);
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn inspection_batch_is_atomic_when_any_unit_has_an_invalid_transition() {
        let (database, path) = test_database().await;
        database
            .post_receipt(receipt_request(
                "request-1",
                "receipt-key-1",
                "R-1",
                &["VALID-UNIT", "ALREADY-TESTED"],
            ))
            .await
            .expect("post receipt");
        database
            .complete_inspection(inspection_request(
                "request-2",
                "inspection-key-1",
                "Q-1",
                InspectionKind::Initial,
                vec![("ALREADY-TESTED", QualityOutcome::Passed)],
            ))
            .await
            .expect("complete prerequisite inspection");

        let error = database
            .complete_inspection(inspection_request(
                "request-3",
                "inspection-key-2",
                "Q-2",
                InspectionKind::Initial,
                vec![
                    ("VALID-UNIT", QualityOutcome::Passed),
                    ("ALREADY-TESTED", QualityOutcome::Failed),
                ],
            ))
            .await
            .expect_err("invalid second result must reject the whole batch");
        assert!(matches!(error, ApplicationError::Domain { .. }));

        let row = sqlx::query(
            "SELECT inventory_status, quality_status, version, location_id FROM inventory_units WHERE barcode = 'VALID-UNIT'",
        )
        .fetch_one(database.pool())
        .await
        .expect("load valid unit after rollback");
        assert_eq!(row.get::<String, _>("inventory_status"), "received");
        assert_eq!(row.get::<String, _>("quality_status"), "untested");
        assert_eq!(row.get::<i64, _>("version"), 1);
        assert_eq!(
            row.get::<String, _>("location_id"),
            database.receiving_location_id()
        );
        let inspection_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quality_inspections")
            .fetch_one(database.pool())
            .await
            .expect("count inspections");
        let idempotency_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idempotency_records WHERE scope = ?1")
                .bind(INSPECTION_SCOPE)
                .fetch_one(database.pool())
                .await
                .expect("count inspection idempotency records");
        assert_eq!(inspection_count, 1);
        assert_eq!(idempotency_count, 1);
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn inspection_updates_projections_and_replay_does_not_duplicate_facts() {
        let (database, path) = test_database().await;
        database
            .post_receipt(receipt_request(
                "request-1",
                "receipt-key-1",
                "R-1",
                &["PASS-1", "FAIL-1"],
            ))
            .await
            .expect("post receipt");
        let request = inspection_request(
            "request-2",
            "inspection-key-1",
            "Q-1",
            InspectionKind::Initial,
            vec![
                ("PASS-1", QualityOutcome::Passed),
                ("FAIL-1", QualityOutcome::Failed),
            ],
        );
        let first = database
            .complete_inspection(request.clone())
            .await
            .expect("complete inspection");
        let replay = database
            .complete_inspection(request)
            .await
            .expect("replay inspection");
        assert_eq!(first.inspection_id, replay.inspection_id);
        assert!(replay.idempotent_replay);
        assert_eq!(first.passed_count, 1);
        assert_eq!(first.failed_count, 1);

        let passed = database
            .list_inventory(InventoryListQuery {
                search: Some("PASS-1".to_owned()),
                ..InventoryListQuery::default()
            })
            .await
            .expect("list passed inventory");
        assert_eq!(passed.total, 1);
        assert_eq!(passed.items[0].inventory_status, InventoryStatus::Available);
        assert_eq!(passed.items[0].quality_status, QualityStatus::Passed);
        assert_eq!(passed.items[0].version, 3);
        assert_eq!(passed.items[0].location_id, database.storage_location_id());

        let failed = database
            .list_inventory(InventoryListQuery {
                search: Some("FAIL-1".to_owned()),
                ..InventoryListQuery::default()
            })
            .await
            .expect("list failed inventory");
        assert_eq!(
            failed.items[0].inventory_status,
            InventoryStatus::Quarantined
        );
        assert_eq!(failed.items[0].quality_status, QualityStatus::Failed);
        assert_eq!(
            failed.items[0].location_id,
            database.quarantine_location_id()
        );

        let summary = database
            .inventory_summary(InventorySummaryQuery::default())
            .await
            .expect("summarize inventory");
        assert_eq!(summary.total_units, 2);
        assert_eq!(summary.inventory.available, 1);
        assert_eq!(summary.inventory.quarantined, 1);
        assert_eq!(summary.quality.passed, 1);
        assert_eq!(summary.quality.failed, 1);

        let inspection_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quality_inspections")
            .fetch_one(database.pool())
            .await
            .expect("count inspections");
        let result_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM quality_inspection_results")
                .fetch_one(database.pool())
                .await
                .expect("count results");
        let movement_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(database.pool())
            .await
            .expect("count movements");
        assert_eq!(inspection_count, 1);
        assert_eq!(result_count, 2);
        assert_eq!(movement_count, 4);
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn archived_offline_workspace_rejects_new_writes() {
        let (database, path) = test_database().await;
        database
            .post_receipt(receipt_request(
                "request-1",
                "receipt-key-1",
                "R-1",
                &["ARCHIVE-1"],
            ))
            .await
            .expect("post receipt before archive");
        database
            .mark_read_only("export-1", "checksum-1")
            .await
            .expect("archive workspace");
        assert!(database.is_read_only().await.expect("read archive marker"));

        let error = database
            .post_receipt(receipt_request(
                "request-2",
                "receipt-key-2",
                "R-2",
                &["ARCHIVE-2"],
            ))
            .await
            .expect_err("archived workspace must reject receipt writes");
        assert!(
            matches!(error, ApplicationError::Conflict { ref entity, .. } if entity == "workspace")
        );

        let status: String = sqlx::query_scalar(
            "SELECT status FROM migration_packages WHERE export_id = 'export-1'",
        )
        .fetch_one(database.pool())
        .await
        .expect("archive package record");
        assert_eq!(status, "archived");
        close_and_remove(database, path).await;
    }
}
