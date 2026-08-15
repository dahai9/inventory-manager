use super::domain::{
    DomainError, InspectionKind, InventoryStatus, InventoryUnit, NewInventoryUnit, QualityOutcome,
    QualityStatus,
};
use super::sqlite::{now_utc, OfflineDatabase};
use super::warranty::{resolve_warranty, WarrantyInput};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};
use std::collections::{HashMap, HashSet};
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
    pub supplier_name: String,
    pub sku_code: String,
    pub sku_name: String,
    pub source_reference: Option<String>,
    pub received_at: String,
    pub actor_id: String,
    pub barcodes: Vec<String>,
    pub notes: Option<String>,
    #[serde(default)]
    pub warranty: Option<WarrantyInput>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supplier_party_id: Option<String>,
    pub sku_id: String,
    pub received_count: u32,
    pub units: Vec<ReceiptUnit>,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceCatalog {
    pub products: Vec<CatalogProduct>,
    #[serde(default)]
    pub parties: Vec<CatalogParty>,
    pub goods_owners: Vec<CatalogParty>,
    pub suppliers: Vec<CatalogParty>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogProduct {
    pub sku_id: String,
    pub code: String,
    pub name: String,
    pub serial_prefix: Option<String>,
    pub serial_forbidden_chars: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogParty {
    pub party_id: String,
    pub display_name: String,
    #[serde(default)]
    pub roles: Vec<CatalogPartyRole>,
    #[serde(default)]
    pub contact_name: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub wechat: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateCatalogProductRequest {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub serial_prefix: Option<String>,
    #[serde(default)]
    pub serial_forbidden_chars: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveCatalogProductRequest {
    #[serde(default)]
    pub sku_id: Option<String>,
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub serial_prefix: Option<String>,
    #[serde(default)]
    pub serial_forbidden_chars: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogPartyRole {
    GoodsOwner,
    UpstreamReceiver,
    Supplier,
    Carrier,
}

impl CatalogPartyRole {
    pub(crate) fn database_value(self) -> &'static str {
        match self {
            Self::GoodsOwner => "goods_owner",
            Self::UpstreamReceiver => "upstream_receiver",
            Self::Supplier => "supplier",
            Self::Carrier => "carrier",
        }
    }

    pub(crate) fn from_database_value(value: &str) -> Option<Self> {
        match value {
            "goods_owner" => Some(Self::GoodsOwner),
            "upstream_receiver" => Some(Self::UpstreamReceiver),
            "supplier" => Some(Self::Supplier),
            "carrier" => Some(Self::Carrier),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateCatalogPartyRequest {
    pub display_name: String,
    pub role: CatalogPartyRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveCatalogPartyRequest {
    #[serde(default)]
    pub party_id: Option<String>,
    pub display_name: String,
    pub roles: Vec<CatalogPartyRole>,
    #[serde(default)]
    pub contact_name: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub wechat: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityLabelDisposition {
    Available,
    Quarantine,
}

impl QualityLabelDisposition {
    pub(crate) fn database_value(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Quarantine => "quarantine",
        }
    }

    pub(crate) fn outcome(self) -> QualityOutcome {
        match self {
            Self::Available => QualityOutcome::Passed,
            Self::Quarantine => QualityOutcome::Failed,
        }
    }

    pub(crate) fn from_database_value(value: &str) -> Option<Self> {
        match value {
            "available" => Some(Self::Available),
            "quarantine" => Some(Self::Quarantine),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityLabelNameHistory {
    pub history_id: String,
    pub old_name: String,
    pub new_name: String,
    pub changed_by: String,
    pub change_note: Option<String>,
    pub changed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityLabel {
    pub quality_label_id: String,
    pub name: String,
    pub disposition: QualityLabelDisposition,
    pub active: bool,
    pub usage_count: u64,
    pub name_history: Vec<QualityLabelNameHistory>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveQualityLabelRequest {
    #[serde(default)]
    pub quality_label_id: Option<String>,
    pub name: String,
    pub disposition: QualityLabelDisposition,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub rename_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkuScanRules {
    id: String,
    serial_prefix: Option<String>,
    serial_forbidden_chars: String,
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
    pub quality_label_id: Option<String>,
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
pub struct InventorySupplierStockSummary {
    pub supplier_party_id: Option<String>,
    pub supplier_name: String,
    pub on_hand_units: u64,
    pub inventory: InventoryStatusSummary,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryProductStockSummary {
    pub sku_id: String,
    pub sku_code: String,
    pub sku_name: String,
    pub on_hand_units: u64,
    pub inventory: InventoryStatusSummary,
    pub suppliers: Vec<InventorySupplierStockSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventorySummaryResponse {
    pub total_units: u64,
    pub inventory: InventoryStatusSummary,
    pub quality: QualityStatusSummary,
    #[serde(default)]
    pub products: Vec<InventoryProductStockSummary>,
}

impl OfflineDatabase {
    pub async fn post_receipt(
        &self,
        request: PostReceiptRequest,
    ) -> ApplicationResult<PostReceiptResponse> {
        let request = normalize_receipt_request(request)?;
        let request_hash = request_hash(&request)?;
        let warranty = resolve_warranty(request.warranty.clone(), &request.received_at)
            .map_err(|message| validation("warranty", &message))?;
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

        let supplier_is_owner = request
            .owner_name
            .eq_ignore_ascii_case(&request.supplier_name);
        let legacy_owner_party_id = if supplier_is_owner {
            None
        } else {
            Some(
                lookup_catalog_party(
                    &mut transaction,
                    workspace_id,
                    &request.owner_name,
                    CatalogPartyRole::GoodsOwner,
                )
                .await?,
            )
        };
        let supplier_party_id = lookup_catalog_party(
            &mut transaction,
            workspace_id,
            &request.supplier_name,
            CatalogPartyRole::Supplier,
        )
        .await?;
        let owner_party_id = legacy_owner_party_id.unwrap_or_else(|| supplier_party_id.clone());
        let sku = lookup_catalog_sku(
            &mut transaction,
            workspace_id,
            &request.sku_code,
            &request.sku_name,
        )
        .await?;
        validate_barcodes_for_sku(&request.barcodes, &sku)?;
        let sku_id = sku.id;

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
                supplier_party_id, source_reference, received_at, status, actor_id,
                idempotency_key, request_id, created_at, warranty_duration_days,
                warranty_label_snapshot, warranty_started_at, warranty_expires_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'posted', ?9, ?10, ?11, ?12,
                      ?13, ?14, ?15, ?16)
            "#,
        )
        .bind(&receipt_id)
        .bind(workspace_id)
        .bind(&request.receipt_no)
        .bind(&owner_party_id)
        .bind(self.warehouse_id())
        .bind(&supplier_party_id)
        .bind(&request.source_reference)
        .bind(&request.received_at)
        .bind(&request.actor_id)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .bind(&now)
        .bind(
            warranty
                .as_ref()
                .map(|terms| i64::from(terms.duration_days)),
        )
        .bind(warranty.as_ref().map(|terms| terms.label_snapshot.as_str()))
        .bind(warranty.as_ref().map(|terms| terms.starts_at.as_str()))
        .bind(warranty.as_ref().map(|terms| terms.expires_at.as_str()))
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
            supplier_party_id: Some(supplier_party_id),
            sku_id,
            received_count: units.len() as u32,
            units,
            idempotent_replay: false,
        };
        let details = json!({
            "receipt_no": request.receipt_no,
            "owner_party_id": response.owner_party_id,
            "supplier_party_id": response.supplier_party_id,
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
            let (quality_label_id, quality_label_snapshot) = if let Some(quality_label_id) =
                &result.quality_label_id
            {
                let label = sqlx::query(
                    r#"
                        SELECT id, name, disposition, active, created_at, updated_at,
                               (SELECT COUNT(*)
                                  FROM quality_inspection_results result
                                 WHERE result.workspace_id = label.workspace_id
                                   AND result.quality_label_id = label.id) AS usage_count
                          FROM quality_labels
                          AS label
                         WHERE workspace_id = ?1 AND id = ?2
                        "#,
                )
                .bind(workspace_id)
                .bind(quality_label_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|error| storage("load inspection quality label", error))?
                .map(quality_label_from_sqlite_row)
                .transpose()?
                .ok_or_else(|| ApplicationError::NotFound {
                    entity: "quality_label".to_owned(),
                    key: quality_label_id.clone(),
                })?;
                if !label.active {
                    return Err(ApplicationError::Conflict {
                        entity: "quality_label".to_owned(),
                        key: label.name,
                        message: "quality label is inactive".to_owned(),
                    });
                }
                if label.disposition.outcome() != result.outcome {
                    return Err(ApplicationError::Conflict {
                        entity: "quality_label_disposition".to_owned(),
                        key: label.name,
                        message: "quality label does not match the requested inventory handling"
                            .to_owned(),
                    });
                }
                (Some(label.quality_label_id), Some(label.name))
            } else {
                (None, None)
            };
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
                quality_label_id,
                quality_label_snapshot,
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
                    quality_label_id, quality_label_snapshot, defect_code,
                    measurements_json, notes, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                "#,
            )
            .bind(result_id)
            .bind(workspace_id)
            .bind(&inspection_id)
            .bind(&validated_result.unit.id)
            .bind(quality_outcome_name(validated_result.input.outcome))
            .bind(&validated_result.quality_label_id)
            .bind(&validated_result.quality_label_snapshot)
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

    pub async fn list_reference_catalog(&self) -> ApplicationResult<ReferenceCatalog> {
        let products = sqlx::query(
            r#"
            SELECT id, code, name, serial_prefix, serial_forbidden_chars
              FROM skus
             WHERE workspace_id = ?1 AND active = 1
             ORDER BY code, id
            "#,
        )
        .bind(self.workspace_id())
        .fetch_all(self.pool())
        .await
        .map_err(|error| storage("list catalog products", error))?
        .into_iter()
        .map(|row| {
            Ok(CatalogProduct {
                sku_id: row
                    .try_get("id")
                    .map_err(|error| storage("read catalog SKU id", error))?,
                code: row
                    .try_get("code")
                    .map_err(|error| storage("read catalog SKU code", error))?,
                name: row
                    .try_get("name")
                    .map_err(|error| storage("read catalog SKU name", error))?,
                serial_prefix: row
                    .try_get("serial_prefix")
                    .map_err(|error| storage("read catalog SKU serial prefix", error))?,
                serial_forbidden_chars: row
                    .try_get("serial_forbidden_chars")
                    .map_err(|error| storage("read catalog SKU scan safeguard", error))?,
            })
        })
        .collect::<ApplicationResult<Vec<_>>>()?;
        let parties = self.list_catalog_parties().await?;
        let goods_owners = parties
            .iter()
            .filter(|party| party.roles.contains(&CatalogPartyRole::GoodsOwner))
            .cloned()
            .collect();
        let suppliers = parties
            .iter()
            .filter(|party| party.roles.contains(&CatalogPartyRole::Supplier))
            .cloned()
            .collect();
        Ok(ReferenceCatalog {
            products,
            parties,
            goods_owners,
            suppliers,
        })
    }

    pub async fn list_quality_labels(&self) -> ApplicationResult<Vec<QualityLabel>> {
        let mut labels = sqlx::query(
            r#"
            SELECT label.id, label.name, label.disposition, label.active,
                   label.created_at, label.updated_at,
                   (SELECT COUNT(*)
                      FROM quality_inspection_results result
                     WHERE result.workspace_id = label.workspace_id
                       AND result.quality_label_id = label.id) AS usage_count
              FROM quality_labels label
             WHERE label.workspace_id = ?1
             ORDER BY label.active DESC, label.disposition, label.name, label.id
            "#,
        )
        .bind(self.workspace_id())
        .fetch_all(self.pool())
        .await
        .map_err(|error| storage("list quality labels", error))?
        .into_iter()
        .map(quality_label_from_sqlite_row)
        .collect::<ApplicationResult<Vec<_>>>()?;
        let history_rows = sqlx::query(
            r#"
            SELECT id, quality_label_id, old_name, new_name,
                   changed_by_snapshot, change_note, changed_at
              FROM quality_label_name_history
             WHERE workspace_id = ?1
             ORDER BY changed_at DESC, id DESC
            "#,
        )
        .bind(self.workspace_id())
        .fetch_all(self.pool())
        .await
        .map_err(|error| storage("list quality label name history", error))?;
        let mut history_by_label: HashMap<String, Vec<QualityLabelNameHistory>> = HashMap::new();
        for row in history_rows {
            let quality_label_id: String = row
                .try_get("quality_label_id")
                .map_err(|error| storage("read quality label history label id", error))?;
            history_by_label
                .entry(quality_label_id)
                .or_default()
                .push(quality_label_name_history_from_sqlite_row(row)?);
        }
        for label in &mut labels {
            label.name_history = history_by_label
                .remove(&label.quality_label_id)
                .unwrap_or_default();
        }
        Ok(labels)
    }

    pub async fn save_quality_label(
        &self,
        input: SaveQualityLabelRequest,
    ) -> ApplicationResult<QualityLabel> {
        let input = normalize_quality_label(input)?;
        let workspace_id = self.workspace_id();
        let now = application_now()?;
        let normalized_name = input.name.to_lowercase();
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| storage("begin quality label transaction", error))?;
        ensure_workspace_writable(&mut transaction, workspace_id).await?;

        let updating = input.quality_label_id.is_some();
        let mut previous_label: Option<(String, QualityLabelDisposition, u64)> = None;
        let quality_label_id = if let Some(quality_label_id) = input.quality_label_id.clone() {
            let current = sqlx::query(
                r#"
                SELECT label.name, label.disposition,
                       (SELECT COUNT(*)
                          FROM quality_inspection_results result
                         WHERE result.workspace_id = label.workspace_id
                           AND result.quality_label_id = label.id) AS usage_count
                  FROM quality_labels label
                 WHERE label.workspace_id = ?1 AND label.id = ?2
                "#,
            )
            .bind(workspace_id)
            .bind(&quality_label_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|error| storage("find quality label for update", error))?;
            let current = current.ok_or_else(|| ApplicationError::NotFound {
                entity: "quality_label".to_owned(),
                key: quality_label_id.clone(),
            })?;
            let current_name: String = current
                .try_get("name")
                .map_err(|error| storage("read current quality label name", error))?;
            let current_disposition_value: String = current
                .try_get("disposition")
                .map_err(|error| storage("read current quality label disposition", error))?;
            let current_disposition =
                QualityLabelDisposition::from_database_value(&current_disposition_value)
                    .ok_or_else(|| {
                        storage(
                            "read current quality label disposition",
                            format!("unknown value {current_disposition_value}"),
                        )
                    })?;
            let usage_count = u64::try_from(
                current
                    .try_get::<i64, _>("usage_count")
                    .map_err(|error| storage("read quality label usage count", error))?,
            )
            .map_err(|error| storage("read quality label usage count", error))?;
            if usage_count > 0 && current_disposition != input.disposition {
                return Err(ApplicationError::Conflict {
                    entity: "quality_label_disposition".to_owned(),
                    key: current_name,
                    message: "a used quality label cannot change inventory handling; create a new label instead"
                        .to_owned(),
                });
            }
            previous_label = Some((current_name, current_disposition, usage_count));
            quality_label_id
        } else {
            new_id()
        };

        let conflicting_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM quality_labels WHERE workspace_id = ?1 AND normalized_name = ?2 AND id <> ?3",
        )
        .bind(workspace_id)
        .bind(&normalized_name)
        .bind(&quality_label_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| storage("check quality label name", error))?;
        if conflicting_id.is_some() {
            return Err(ApplicationError::Conflict {
                entity: "quality_label".to_owned(),
                key: input.name,
                message: "quality label name already exists in this workspace".to_owned(),
            });
        }

        if updating {
            sqlx::query(
                r#"
                UPDATE quality_labels
                   SET name = ?1, normalized_name = ?2, disposition = ?3,
                       active = ?4, updated_at = ?5
                 WHERE workspace_id = ?6 AND id = ?7
                "#,
            )
            .bind(&input.name)
            .bind(&normalized_name)
            .bind(input.disposition.database_value())
            .bind(i64::from(input.active))
            .bind(&now)
            .bind(workspace_id)
            .bind(&quality_label_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage("update quality label", error))?;
            if let Some((old_name, _, _)) = &previous_label {
                if old_name != &input.name {
                    sqlx::query(
                        r#"
                        INSERT INTO quality_label_name_history (
                            id, workspace_id, quality_label_id, old_name, new_name,
                            changed_by, changed_by_snapshot, change_note, changed_at
                        ) VALUES (?1, ?2, ?3, ?4, ?5, 'local', '本机操作', ?6, ?7)
                        "#,
                    )
                    .bind(new_id())
                    .bind(workspace_id)
                    .bind(&quality_label_id)
                    .bind(old_name)
                    .bind(&input.name)
                    .bind(&input.rename_note)
                    .bind(&now)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| storage("record quality label name history", error))?;
                }
            }
        } else {
            sqlx::query(
                r#"
                INSERT INTO quality_labels (
                    id, workspace_id, name, normalized_name, disposition, active,
                    created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                "#,
            )
            .bind(&quality_label_id)
            .bind(workspace_id)
            .bind(&input.name)
            .bind(&normalized_name)
            .bind(input.disposition.database_value())
            .bind(i64::from(input.active))
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage("create quality label", error))?;
        }

        let label = sqlx::query(
            r#"
            SELECT label.id, label.name, label.disposition, label.active,
                   label.created_at, label.updated_at,
                   (SELECT COUNT(*)
                      FROM quality_inspection_results result
                     WHERE result.workspace_id = label.workspace_id
                       AND result.quality_label_id = label.id) AS usage_count
              FROM quality_labels label
             WHERE label.workspace_id = ?1 AND label.id = ?2
            "#,
        )
        .bind(workspace_id)
        .bind(&quality_label_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| storage("load saved quality label", error))
        .and_then(quality_label_from_sqlite_row)?;
        let mut label = label;
        label.name_history = sqlx::query(
            r#"
            SELECT id, quality_label_id, old_name, new_name,
                   changed_by_snapshot, change_note, changed_at
              FROM quality_label_name_history
             WHERE workspace_id = ?1 AND quality_label_id = ?2
             ORDER BY changed_at DESC, id DESC
            "#,
        )
        .bind(workspace_id)
        .bind(&quality_label_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| storage("load saved quality label name history", error))?
        .into_iter()
        .map(quality_label_name_history_from_sqlite_row)
        .collect::<ApplicationResult<Vec<_>>>()?;
        transaction
            .commit()
            .await
            .map_err(|error| storage("commit quality label", error))?;
        Ok(label)
    }

    pub async fn create_catalog_product(
        &self,
        input: CreateCatalogProductRequest,
    ) -> ApplicationResult<CatalogProduct> {
        let input = normalize_catalog_product(input)?;
        let workspace_id = self.workspace_id();
        let now = application_now()?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| storage("begin catalog product transaction", error))?;
        ensure_workspace_writable(&mut transaction, workspace_id).await?;
        let sku_id = new_id();
        let inserted = sqlx::query(
            r#"
            INSERT INTO skus
                (id, workspace_id, code, name, tracking_mode, active,
                 serial_prefix, serial_forbidden_chars, created_at)
            VALUES (?1, ?2, ?3, ?4, 'serial', 1, ?5, ?6, ?7)
            ON CONFLICT (workspace_id, code) DO NOTHING
            "#,
        )
        .bind(&sku_id)
        .bind(workspace_id)
        .bind(&input.code)
        .bind(&input.name)
        .bind(&input.serial_prefix)
        .bind(&input.serial_forbidden_chars)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage("create catalog product", error))?;
        if inserted.rows_affected() != 1 {
            return Err(ApplicationError::Conflict {
                entity: "sku".to_owned(),
                key: input.code,
                message: "product code already exists in this workspace".to_owned(),
            });
        }
        transaction
            .commit()
            .await
            .map_err(|error| storage("commit catalog product", error))?;
        Ok(CatalogProduct {
            sku_id,
            code: input.code,
            name: input.name,
            serial_prefix: input.serial_prefix,
            serial_forbidden_chars: input.serial_forbidden_chars,
        })
    }

    pub async fn save_catalog_product(
        &self,
        input: SaveCatalogProductRequest,
    ) -> ApplicationResult<CatalogProduct> {
        let input = normalize_saved_catalog_product(input)?;
        let Some(sku_id) = input.sku_id else {
            return self
                .create_catalog_product(CreateCatalogProductRequest {
                    code: input.code,
                    name: input.name,
                    serial_prefix: input.serial_prefix,
                    serial_forbidden_chars: input.serial_forbidden_chars,
                })
                .await;
        };
        let workspace_id = self.workspace_id();
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| storage("begin catalog product save transaction", error))?;
        ensure_workspace_writable(&mut transaction, workspace_id).await?;

        let conflicting_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM skus WHERE workspace_id = ?1 AND code = ?2 AND id <> ?3",
        )
        .bind(workspace_id)
        .bind(&input.code)
        .bind(&sku_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| storage("check catalog product code", error))?;
        if conflicting_id.is_some() {
            return Err(ApplicationError::Conflict {
                entity: "sku".to_owned(),
                key: input.code,
                message: "product code already exists in this workspace".to_owned(),
            });
        }

        let updated = sqlx::query(
            r#"
            UPDATE skus
               SET code = ?1, name = ?2, serial_prefix = ?3, serial_forbidden_chars = ?4
             WHERE workspace_id = ?5 AND id = ?6 AND active = 1
            "#,
        )
        .bind(&input.code)
        .bind(&input.name)
        .bind(&input.serial_prefix)
        .bind(&input.serial_forbidden_chars)
        .bind(workspace_id)
        .bind(&sku_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage("update catalog product", error))?;
        if updated.rows_affected() != 1 {
            return Err(ApplicationError::NotFound {
                entity: "sku".to_owned(),
                key: sku_id,
            });
        }
        transaction
            .commit()
            .await
            .map_err(|error| storage("commit catalog product save", error))?;
        Ok(CatalogProduct {
            sku_id,
            code: input.code,
            name: input.name,
            serial_prefix: input.serial_prefix,
            serial_forbidden_chars: input.serial_forbidden_chars,
        })
    }

    pub async fn create_catalog_party(
        &self,
        input: CreateCatalogPartyRequest,
    ) -> ApplicationResult<CatalogParty> {
        let input = normalize_catalog_party(input)?;
        let workspace_id = self.workspace_id();
        let now = application_now()?;
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| storage("begin catalog party transaction", error))?;
        ensure_workspace_writable(&mut transaction, workspace_id).await?;
        let party_id = lookup_or_create_party(
            &mut transaction,
            workspace_id,
            &input.display_name,
            input.role,
            &now,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|error| storage("commit catalog party", error))?;
        Ok(CatalogParty {
            party_id,
            display_name: input.display_name,
            roles: vec![input.role],
            contact_name: None,
            phone: None,
            wechat: None,
            email: None,
            address: None,
            notes: None,
        })
    }

    pub async fn save_catalog_party(
        &self,
        input: SaveCatalogPartyRequest,
    ) -> ApplicationResult<CatalogParty> {
        let input = normalize_saved_catalog_party(input)?;
        let workspace_id = self.workspace_id();
        let now = application_now()?;
        let normalized_name = input.display_name.to_lowercase();
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| storage("begin catalog party save transaction", error))?;
        ensure_workspace_writable(&mut transaction, workspace_id).await?;

        let party_id = if let Some(party_id) = &input.party_id {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM business_parties WHERE workspace_id = ?1 AND id = ?2)",
            )
            .bind(workspace_id)
            .bind(party_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(|error| storage("find catalog party for update", error))?;
            if !exists {
                return Err(ApplicationError::NotFound {
                    entity: "business_party".to_owned(),
                    key: party_id.clone(),
                });
            }
            party_id.clone()
        } else {
            let candidate_id = new_id();
            sqlx::query(
                r#"
                INSERT INTO business_parties
                    (id, workspace_id, normalized_name, display_name, contact_name,
                     phone, wechat, email, address, notes, created_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT (workspace_id, normalized_name) DO NOTHING
                "#,
            )
            .bind(&candidate_id)
            .bind(workspace_id)
            .bind(&normalized_name)
            .bind(&input.display_name)
            .bind(&input.contact_name)
            .bind(&input.phone)
            .bind(&input.wechat)
            .bind(&input.email)
            .bind(&input.address)
            .bind(&input.notes)
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage("create unified catalog party", error))?;
            sqlx::query_scalar(
                "SELECT id FROM business_parties WHERE workspace_id = ?1 AND normalized_name = ?2",
            )
            .bind(workspace_id)
            .bind(&normalized_name)
            .fetch_one(&mut *transaction)
            .await
            .map_err(|error| storage("load unified catalog party", error))?
        };

        let conflicting_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM business_parties WHERE workspace_id = ?1 AND normalized_name = ?2 AND id <> ?3",
        )
        .bind(workspace_id)
        .bind(&normalized_name)
        .bind(&party_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| storage("check catalog party name", error))?;
        if conflicting_id.is_some() {
            return Err(ApplicationError::Conflict {
                entity: "business_party".to_owned(),
                key: input.display_name,
                message: "party name already exists in this workspace".to_owned(),
            });
        }

        sqlx::query(
            r#"
            UPDATE business_parties
               SET normalized_name = ?1, display_name = ?2, contact_name = ?3,
                   phone = ?4, wechat = ?5, email = ?6, address = ?7, notes = ?8
             WHERE workspace_id = ?9 AND id = ?10
            "#,
        )
        .bind(&normalized_name)
        .bind(&input.display_name)
        .bind(&input.contact_name)
        .bind(&input.phone)
        .bind(&input.wechat)
        .bind(&input.email)
        .bind(&input.address)
        .bind(&input.notes)
        .bind(workspace_id)
        .bind(&party_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage("update unified catalog party", error))?;

        sqlx::query("DELETE FROM party_roles WHERE workspace_id = ?1 AND party_id = ?2")
            .bind(workspace_id)
            .bind(&party_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage("replace catalog party roles", error))?;
        for role in &input.roles {
            sqlx::query(
                "INSERT INTO party_roles (workspace_id, party_id, role, created_at) VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(workspace_id)
            .bind(&party_id)
            .bind(role.database_value())
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage("save catalog party role", error))?;
        }
        transaction
            .commit()
            .await
            .map_err(|error| storage("commit catalog party save", error))?;

        Ok(CatalogParty {
            party_id,
            display_name: input.display_name,
            roles: input.roles,
            contact_name: input.contact_name,
            phone: input.phone,
            wechat: input.wechat,
            email: input.email,
            address: input.address,
            notes: input.notes,
        })
    }

    async fn list_catalog_parties(&self) -> ApplicationResult<Vec<CatalogParty>> {
        let party_rows = sqlx::query(
            r#"
            SELECT id, display_name, contact_name, phone, wechat, email, address, notes
              FROM business_parties
             WHERE workspace_id = ?1
             ORDER BY display_name COLLATE NOCASE, id
            "#,
        )
        .bind(self.workspace_id())
        .fetch_all(self.pool())
        .await
        .map_err(|error| storage("list unified catalog parties", error))?;
        let role_rows = sqlx::query(
            "SELECT party_id, role FROM party_roles WHERE workspace_id = ?1 ORDER BY party_id, role",
        )
        .bind(self.workspace_id())
        .fetch_all(self.pool())
        .await
        .map_err(|error| storage("list catalog party roles", error))?;
        let mut roles_by_party: HashMap<String, Vec<CatalogPartyRole>> = HashMap::new();
        for row in role_rows {
            let party_id: String = row
                .try_get("party_id")
                .map_err(|error| storage("read catalog party role owner", error))?;
            let role: String = row
                .try_get("role")
                .map_err(|error| storage("read catalog party role", error))?;
            let role = CatalogPartyRole::from_database_value(&role).ok_or_else(|| {
                validation("party role", &format!("unsupported stored role {role}"))
            })?;
            roles_by_party.entry(party_id).or_default().push(role);
        }
        for roles in roles_by_party.values_mut() {
            roles.sort();
        }
        party_rows
            .into_iter()
            .map(|row| {
                let party_id: String = row
                    .try_get("id")
                    .map_err(|error| storage("read catalog party id", error))?;
                Ok(CatalogParty {
                    roles: roles_by_party.remove(&party_id).unwrap_or_default(),
                    party_id,
                    display_name: row
                        .try_get("display_name")
                        .map_err(|error| storage("read catalog party name", error))?,
                    contact_name: row
                        .try_get("contact_name")
                        .map_err(|error| storage("read catalog party contact", error))?,
                    phone: row
                        .try_get("phone")
                        .map_err(|error| storage("read catalog party phone", error))?,
                    wechat: row
                        .try_get("wechat")
                        .map_err(|error| storage("read catalog party wechat", error))?,
                    email: row
                        .try_get("email")
                        .map_err(|error| storage("read catalog party email", error))?,
                    address: row
                        .try_get("address")
                        .map_err(|error| storage("read catalog party address", error))?,
                    notes: row
                        .try_get("notes")
                        .map_err(|error| storage("read catalog party notes", error))?,
                })
            })
            .collect()
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

        let product_rows = sqlx::query(
            r#"
            SELECT units.sku_id, sku.code AS sku_code, sku.name AS sku_name,
                   receipt.supplier_party_id,
                   COALESCE(supplier.display_name, '历史来源未记录') AS supplier_name,
                   units.inventory_status, COUNT(*) AS unit_count
              FROM inventory_units units
              JOIN skus sku
                ON sku.id = units.sku_id
               AND sku.workspace_id = units.workspace_id
              JOIN inbound_receipt_lines line
                ON line.id = units.inbound_receipt_line_id
               AND line.workspace_id = units.workspace_id
              JOIN inbound_receipts receipt
                ON receipt.id = line.receipt_id
               AND receipt.workspace_id = units.workspace_id
              LEFT JOIN business_parties supplier
                ON supplier.id = receipt.supplier_party_id
               AND supplier.workspace_id = units.workspace_id
             WHERE units.workspace_id = ?1
               AND (?2 IS NULL OR units.owner_party_id = ?2)
               AND (?3 IS NULL OR units.sku_id = ?3)
               AND units.inventory_status IN ('received', 'available', 'reserved', 'quarantined')
             GROUP BY units.sku_id, sku.code, sku.name, receipt.supplier_party_id,
                      supplier.display_name, units.inventory_status
            "#,
        )
        .bind(self.workspace_id())
        .bind(&owner_party_id)
        .bind(&sku_id)
        .fetch_all(self.pool())
        .await
        .map_err(|error| storage("summarize on-hand products", error))?;

        for row in product_rows {
            let count = nonnegative_u64("on-hand product group count", row.try_get("unit_count")?)?;
            let inventory_status = parse_inventory_status(row.try_get("inventory_status")?)?;
            summary.add_on_hand_group(
                row.try_get("sku_id")?,
                row.try_get("sku_code")?,
                row.try_get("sku_name")?,
                row.try_get("supplier_party_id")?,
                row.try_get("supplier_name")?,
                inventory_status,
                count,
            );
        }
        summary.sort_on_hand_products();
        Ok(summary)
    }
}

impl InventorySummaryResponse {
    pub(crate) fn add_on_hand_group(
        &mut self,
        sku_id: String,
        sku_code: String,
        sku_name: String,
        supplier_party_id: Option<String>,
        supplier_name: String,
        inventory_status: InventoryStatus,
        count: u64,
    ) {
        let product =
            if let Some(index) = self.products.iter().position(|item| item.sku_id == sku_id) {
                &mut self.products[index]
            } else {
                self.products.push(InventoryProductStockSummary {
                    sku_id: sku_id.clone(),
                    sku_code,
                    sku_name,
                    ..InventoryProductStockSummary::default()
                });
                self.products.last_mut().expect("product was just inserted")
            };
        product.on_hand_units += count;
        product.inventory.add(inventory_status, count);

        let supplier = if let Some(index) = product
            .suppliers
            .iter()
            .position(|item| item.supplier_party_id == supplier_party_id)
        {
            &mut product.suppliers[index]
        } else {
            product.suppliers.push(InventorySupplierStockSummary {
                supplier_party_id,
                supplier_name,
                ..InventorySupplierStockSummary::default()
            });
            product
                .suppliers
                .last_mut()
                .expect("supplier was just inserted")
        };
        supplier.on_hand_units += count;
        supplier.inventory.add(inventory_status, count);
    }

    pub(crate) fn sort_on_hand_products(&mut self) {
        for product in &mut self.products {
            product.suppliers.sort_by(|left, right| {
                right
                    .on_hand_units
                    .cmp(&left.on_hand_units)
                    .then_with(|| left.supplier_name.cmp(&right.supplier_name))
            });
        }
        self.products.sort_by(|left, right| {
            right
                .on_hand_units
                .cmp(&left.on_hand_units)
                .then_with(|| left.sku_code.cmp(&right.sku_code))
        });
    }
}

impl InventoryStatusSummary {
    pub(crate) fn add(&mut self, status: InventoryStatus, count: u64) {
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
    pub(crate) fn add(&mut self, status: QualityStatus, count: u64) {
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
    quality_label_id: Option<String>,
    quality_label_snapshot: Option<String>,
}

fn normalize_quality_label(
    mut input: SaveQualityLabelRequest,
) -> ApplicationResult<SaveQualityLabelRequest> {
    input.quality_label_id = clean_optional(input.quality_label_id);
    if let Some(quality_label_id) = &input.quality_label_id {
        Uuid::parse_str(quality_label_id)
            .map_err(|_| validation("quality_label_id", "must be a UUID"))?;
    }
    input.name = normalized_display_name("quality label name", input.name)?;
    if input.name.chars().count() > 40 {
        return Err(validation(
            "quality label name",
            "must be at most 40 characters",
        ));
    }
    input.rename_note = clean_optional(input.rename_note);
    if input
        .rename_note
        .as_ref()
        .is_some_and(|note| note.chars().count() > 200)
    {
        return Err(validation(
            "quality label rename note",
            "must be at most 200 characters",
        ));
    }
    Ok(input)
}

fn normalize_catalog_product(
    mut input: CreateCatalogProductRequest,
) -> ApplicationResult<CreateCatalogProductRequest> {
    input.code = required_text("product code", input.code)?.to_uppercase();
    input.name = normalized_display_name("product name", input.name)?;
    input.serial_prefix = clean_optional(input.serial_prefix)
        .map(|value| required_text("SN prefix", value).map(|prefix| prefix.to_uppercase()))
        .transpose()?;
    if input.serial_forbidden_chars.len() > 128 {
        return Err(validation(
            "serial_forbidden_chars",
            "must be at most 128 characters",
        ));
    }
    let forbidden = parse_forbidden_serial_tokens(&input.serial_forbidden_chars);
    if let Some((prefix, token)) = input.serial_prefix.as_ref().and_then(|prefix| {
        forbidden
            .iter()
            .find(|token| prefix.contains(token.as_str()))
            .map(|token| (prefix, token))
    }) {
        return Err(validation(
            "serial_prefix",
            &format!("prefix {prefix} contains forbidden character or token {token}"),
        ));
    }
    Ok(input)
}

fn normalize_saved_catalog_product(
    mut input: SaveCatalogProductRequest,
) -> ApplicationResult<SaveCatalogProductRequest> {
    input.sku_id = clean_optional(input.sku_id);
    if let Some(sku_id) = &input.sku_id {
        Uuid::parse_str(sku_id).map_err(|_| validation("sku_id", "must be a UUID"))?;
    }
    let normalized = normalize_catalog_product(CreateCatalogProductRequest {
        code: input.code,
        name: input.name,
        serial_prefix: input.serial_prefix,
        serial_forbidden_chars: input.serial_forbidden_chars,
    })?;
    Ok(SaveCatalogProductRequest {
        sku_id: input.sku_id,
        code: normalized.code,
        name: normalized.name,
        serial_prefix: normalized.serial_prefix,
        serial_forbidden_chars: normalized.serial_forbidden_chars,
    })
}

fn normalize_catalog_party(
    mut input: CreateCatalogPartyRequest,
) -> ApplicationResult<CreateCatalogPartyRequest> {
    input.display_name = normalized_display_name("party name", input.display_name)?;
    Ok(input)
}

fn normalize_saved_catalog_party(
    mut input: SaveCatalogPartyRequest,
) -> ApplicationResult<SaveCatalogPartyRequest> {
    input.party_id = clean_optional(input.party_id);
    if let Some(party_id) = &input.party_id {
        Uuid::parse_str(party_id).map_err(|_| validation("party_id", "must be a UUID"))?;
    }
    input.display_name = normalized_display_name("party name", input.display_name)?;
    input.roles.sort();
    input.roles.dedup();
    if input.roles.is_empty() {
        return Err(validation("roles", "select at least one party role"));
    }
    input.contact_name = normalize_optional_party_field("contact_name", input.contact_name, 120)?;
    input.phone = normalize_optional_party_field("phone", input.phone, 120)?;
    input.wechat = normalize_optional_party_field("wechat", input.wechat, 120)?;
    input.email = normalize_optional_party_field("email", input.email, 254)?;
    input.address = normalize_optional_party_field("address", input.address, 1000)?;
    input.notes = normalize_optional_party_field("notes", input.notes, 2000)?;
    Ok(input)
}

fn normalize_optional_party_field(
    field: &str,
    value: Option<String>,
    max_chars: usize,
) -> ApplicationResult<Option<String>> {
    let value = clean_optional(value);
    if value
        .as_ref()
        .is_some_and(|value| value.chars().count() > max_chars)
    {
        return Err(validation(
            field,
            &format!("must be at most {max_chars} characters"),
        ));
    }
    Ok(value)
}

fn validate_barcodes_for_sku(barcodes: &[String], rules: &SkuScanRules) -> ApplicationResult<()> {
    let forbidden = parse_forbidden_serial_tokens(&rules.serial_forbidden_chars);
    for barcode in barcodes {
        if let Some(prefix) = &rules.serial_prefix {
            if !barcode.starts_with(prefix) {
                return Err(ApplicationError::Validation {
                    field: "barcodes".to_owned(),
                    message: format!("SN {barcode} does not match product prefix {prefix}"),
                });
            }
        }
        if let Some(token) = forbidden
            .iter()
            .find(|token| barcode.contains(token.as_str()))
        {
            return Err(ApplicationError::Validation {
                field: "barcodes".to_owned(),
                message: format!("SN {barcode} contains forbidden character or token {token}"),
            });
        }
    }
    Ok(())
}

fn parse_forbidden_serial_tokens(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter_map(|token| {
            if token == " " {
                Some(" ".to_owned())
            } else {
                let token = token.trim();
                (!token.is_empty()).then(|| token.to_uppercase())
            }
        })
        .collect()
}

fn normalize_receipt_request(
    mut request: PostReceiptRequest,
) -> ApplicationResult<PostReceiptRequest> {
    request.request_id = required_text("request_id", request.request_id)?;
    request.idempotency_key = required_text("idempotency_key", request.idempotency_key)?;
    request.receipt_no = required_text("receipt_no", request.receipt_no)?.to_uppercase();
    request.owner_name = normalized_display_name("owner_name", request.owner_name)?;
    request.supplier_name = normalized_display_name("supplier_name", request.supplier_name)?;
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
        result.quality_label_id = clean_optional(result.quality_label_id.take());
        if let Some(quality_label_id) = &result.quality_label_id {
            Uuid::parse_str(quality_label_id).map_err(|_| {
                validation(
                    &format!("results[{index}].quality_label_id"),
                    "must be a UUID",
                )
            })?;
        }
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

async fn lookup_or_create_party(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    display_name: &str,
    role: CatalogPartyRole,
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
    .map_err(|error| storage("lookup or create business party", error))?;

    let party_id: String = sqlx::query_scalar(
        "SELECT id FROM business_parties WHERE workspace_id = ?1 AND normalized_name = ?2",
    )
    .bind(workspace_id)
    .bind(&normalized_name)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| storage("load business party", error))?;
    sqlx::query(
        r#"
        INSERT INTO party_roles (workspace_id, party_id, role, created_at)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT (workspace_id, party_id, role) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(&party_id)
    .bind(role.database_value())
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("ensure business party role", error))?;
    Ok(party_id)
}

async fn lookup_catalog_party(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    display_name: &str,
    role: CatalogPartyRole,
) -> ApplicationResult<String> {
    let normalized_name = display_name.to_lowercase();
    sqlx::query_scalar(
        r#"
        SELECT bp.id
          FROM business_parties bp
          JOIN party_roles pr
            ON pr.workspace_id = bp.workspace_id AND pr.party_id = bp.id
         WHERE bp.workspace_id = ?1
           AND bp.normalized_name = ?2
           AND pr.role = ?3
        "#,
    )
    .bind(workspace_id)
    .bind(&normalized_name)
    .bind(role.database_value())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| storage("load catalog party", error))?
    .ok_or_else(|| ApplicationError::NotFound {
        entity: role.database_value().to_owned(),
        key: display_name.to_owned(),
    })
}

async fn lookup_catalog_sku(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    code: &str,
    name: &str,
) -> ApplicationResult<SkuScanRules> {
    let row = sqlx::query(
        "SELECT id, name, tracking_mode, active, serial_prefix, serial_forbidden_chars FROM skus WHERE workspace_id = ?1 AND code = ?2",
    )
    .bind(workspace_id)
    .bind(code)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| storage("load catalog SKU", error))?
    .ok_or_else(|| ApplicationError::NotFound {
        entity: "sku".to_owned(),
        key: code.to_owned(),
    })?;
    let catalog_name: String = row
        .try_get("name")
        .map_err(|error| storage("read SKU name", error))?;
    if catalog_name != name {
        return Err(ApplicationError::Conflict {
            entity: "sku".to_owned(),
            key: code.to_owned(),
            message: format!(
                "provided product name {name} does not match catalog name {catalog_name}"
            ),
        });
    }
    let active: i64 = row
        .try_get("active")
        .map_err(|error| storage("read SKU active status", error))?;
    if active != 1 {
        return Err(ApplicationError::Conflict {
            entity: "sku".to_owned(),
            key: code.to_owned(),
            message: "product is inactive".to_owned(),
        });
    }
    let tracking_mode: String = row
        .try_get("tracking_mode")
        .map_err(|error| storage("read SKU tracking mode", error))?;
    if tracking_mode != "serial" {
        return Err(ApplicationError::Conflict {
            entity: "sku".to_owned(),
            key: code.to_owned(),
            message: "product is not serial-tracked".to_owned(),
        });
    }
    Ok(SkuScanRules {
        id: row
            .try_get("id")
            .map_err(|error| storage("read SKU id", error))?,
        serial_prefix: row
            .try_get("serial_prefix")
            .map_err(|error| storage("read SKU serial prefix", error))?,
        serial_forbidden_chars: row
            .try_get("serial_forbidden_chars")
            .map_err(|error| storage("read SKU scan safeguard", error))?,
    })
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

fn default_true() -> bool {
    true
}

fn quality_label_from_sqlite_row(row: sqlx::sqlite::SqliteRow) -> ApplicationResult<QualityLabel> {
    let disposition: String = row
        .try_get("disposition")
        .map_err(|error| storage("read quality label disposition", error))?;
    Ok(QualityLabel {
        quality_label_id: row
            .try_get("id")
            .map_err(|error| storage("read quality label id", error))?,
        name: row
            .try_get("name")
            .map_err(|error| storage("read quality label name", error))?,
        disposition: QualityLabelDisposition::from_database_value(&disposition).ok_or_else(
            || {
                storage(
                    "read quality label disposition",
                    format!("unknown value {disposition}"),
                )
            },
        )?,
        active: row
            .try_get::<i64, _>("active")
            .map_err(|error| storage("read quality label state", error))?
            != 0,
        usage_count: u64::try_from(
            row.try_get::<i64, _>("usage_count")
                .map_err(|error| storage("read quality label usage count", error))?,
        )
        .map_err(|error| storage("read quality label usage count", error))?,
        name_history: Vec::new(),
        created_at: row
            .try_get("created_at")
            .map_err(|error| storage("read quality label creation time", error))?,
        updated_at: row
            .try_get("updated_at")
            .map_err(|error| storage("read quality label update time", error))?,
    })
}

fn quality_label_name_history_from_sqlite_row(
    row: sqlx::sqlite::SqliteRow,
) -> ApplicationResult<QualityLabelNameHistory> {
    Ok(QualityLabelNameHistory {
        history_id: row
            .try_get("id")
            .map_err(|error| storage("read quality label history id", error))?,
        old_name: row
            .try_get("old_name")
            .map_err(|error| storage("read old quality label name", error))?,
        new_name: row
            .try_get("new_name")
            .map_err(|error| storage("read new quality label name", error))?,
        changed_by: row
            .try_get("changed_by_snapshot")
            .map_err(|error| storage("read quality label history actor", error))?,
        change_note: row
            .try_get("change_note")
            .map_err(|error| storage("read quality label rename note", error))?,
        changed_at: row
            .try_get("changed_at")
            .map_err(|error| storage("read quality label rename time", error))?,
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
            supplier_name: "供应商 A".to_owned(),
            sku_code: "model-x".to_owned(),
            sku_name: "型号 X".to_owned(),
            source_reference: Some("采购单 1".to_owned()),
            received_at: "2026-07-31T01:00:00Z".to_owned(),
            actor_id: "operator-1".to_owned(),
            barcodes: barcodes.iter().map(|value| (*value).to_owned()).collect(),
            notes: None,
            warranty: None,
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
                    quality_label_id: None,
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

    async fn create_test_party(
        database: &OfflineDatabase,
        display_name: &str,
        role: CatalogPartyRole,
    ) -> CatalogParty {
        database
            .create_catalog_party(CreateCatalogPartyRequest {
                display_name: display_name.to_owned(),
                role,
            })
            .await
            .expect("create test catalog party")
    }

    async fn seed_default_receipt_parties(database: &OfflineDatabase) {
        create_test_party(database, "客户 A", CatalogPartyRole::GoodsOwner).await;
        create_test_party(database, "供应商 A", CatalogPartyRole::Supplier).await;
    }

    async fn test_database_with_default_catalog() -> (OfflineDatabase, PathBuf) {
        let (database, path) = test_database().await;
        seed_default_receipt_parties(&database).await;
        database
            .create_catalog_product(CreateCatalogProductRequest {
                code: "MODEL-X".to_owned(),
                name: "型号 X".to_owned(),
                serial_prefix: None,
                serial_forbidden_chars: String::new(),
            })
            .await
            .expect("create default test product");
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

    #[test]
    fn catalog_product_rejects_impossible_scan_rules_but_allows_empty_rules() {
        let error = normalize_catalog_product(CreateCatalogProductRequest {
            code: "rules-1".to_owned(),
            name: "Rules 1".to_owned(),
            serial_prefix: Some("sn-bad".to_owned()),
            serial_forbidden_chars: "BAD".to_owned(),
        })
        .expect_err("a prefix containing a case-normalized forbidden token must fail");
        assert!(matches!(
            error,
            ApplicationError::Validation { field, message }
                if field == "serial_prefix" && message.contains("BAD")
        ));

        let allowed = normalize_catalog_product(CreateCatalogProductRequest {
            code: "rules-2".to_owned(),
            name: "Rules 2".to_owned(),
            serial_prefix: Some("sn".to_owned()),
            serial_forbidden_chars: String::new(),
        })
        .expect("an empty forbidden rule intentionally allows any prefix");
        assert_eq!(allowed.serial_prefix.as_deref(), Some("SN"));
    }

    #[tokio::test]
    async fn saved_catalog_product_keeps_identity_for_existing_and_new_inventory() {
        let (database, path) = test_database_with_default_catalog().await;
        let original = database
            .list_reference_catalog()
            .await
            .expect("list original catalog")
            .products
            .into_iter()
            .next()
            .expect("default product");
        let first = database
            .post_receipt(receipt_request(
                "product-edit-request-1",
                "product-edit-key-1",
                "product-edit-receipt-1",
                &["EDIT001"],
            ))
            .await
            .expect("receive inventory before product edit");

        let saved = database
            .save_catalog_product(SaveCatalogProductRequest {
                sku_id: Some(original.sku_id.clone()),
                code: " model-edited ".to_owned(),
                name: "型号已编辑".to_owned(),
                serial_prefix: Some("edit".to_owned()),
                serial_forbidden_chars: "-, ".to_owned(),
            })
            .await
            .expect("save edited product");
        assert_eq!(saved.sku_id, original.sku_id);
        assert_eq!(saved.code, "MODEL-EDITED");
        assert_eq!(first.sku_id, saved.sku_id);

        let mut second_request = receipt_request(
            "product-edit-request-2",
            "product-edit-key-2",
            "product-edit-receipt-2",
            &["EDIT002"],
        );
        second_request.sku_code = saved.code.clone();
        second_request.sku_name = saved.name.clone();
        let second = database
            .post_receipt(second_request)
            .await
            .expect("receive inventory after product edit");
        assert_eq!(second.sku_id, saved.sku_id);

        let inventory = database
            .list_inventory(InventoryListQuery {
                sku_id: Some(saved.sku_id.clone()),
                ..InventoryListQuery::default()
            })
            .await
            .expect("list inventory for edited product");
        assert_eq!(inventory.total, 2);
        assert!(inventory.items.iter().all(|item| {
            item.sku_id == saved.sku_id
                && item.sku_code == saved.code
                && item.sku_name == saved.name
        }));
        close_and_remove(database, path).await;
    }

    #[test]
    fn receipt_response_deserializes_legacy_payload_without_supplier_identity() {
        let response: PostReceiptResponse = serde_json::from_value(json!({
            "receipt_id": "receipt-1",
            "receipt_line_id": "line-1",
            "receipt_no": "R-1",
            "owner_party_id": "owner-1",
            "sku_id": "sku-1",
            "received_count": 0,
            "units": [],
            "idempotent_replay": false
        }))
        .expect("legacy idempotency response must remain readable");
        assert_eq!(response.supplier_party_id, None);
    }

    #[tokio::test]
    async fn receipts_require_role_qualified_active_serial_catalog_entries() {
        let (database, path) = test_database().await;
        let unknown_owner = database
            .post_receipt(receipt_request(
                "catalog-guard-request-1",
                "catalog-guard-key-1",
                "catalog-guard-receipt-1",
                &["GUARD001"],
            ))
            .await
            .expect_err("receipt must not create an unknown owner");
        assert!(matches!(
            unknown_owner,
            ApplicationError::NotFound { entity, .. } if entity == "goods_owner"
        ));
        let empty_catalog: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM business_parties), (SELECT COUNT(*) FROM skus)",
        )
        .fetch_one(database.pool())
        .await
        .expect("count untouched catalog");
        assert_eq!(empty_catalog, (0, 0));

        create_test_party(&database, "客户 A", CatalogPartyRole::Supplier).await;
        let wrong_owner_role = database
            .post_receipt(receipt_request(
                "catalog-guard-request-owner-role",
                "catalog-guard-key-owner-role",
                "catalog-guard-receipt-owner-role",
                &["GUARD-OWNER-ROLE"],
            ))
            .await
            .expect_err("a party without goods owner role must be rejected");
        assert!(matches!(
            wrong_owner_role,
            ApplicationError::NotFound { entity, .. } if entity == "goods_owner"
        ));

        create_test_party(&database, "客户 A", CatalogPartyRole::GoodsOwner).await;
        create_test_party(&database, "供应商 A", CatalogPartyRole::GoodsOwner).await;
        let product = database
            .create_catalog_product(CreateCatalogProductRequest {
                code: "MODEL-X".to_owned(),
                name: "型号 X".to_owned(),
                serial_prefix: None,
                serial_forbidden_chars: String::new(),
            })
            .await
            .expect("create guarded product");

        let wrong_supplier_role = database
            .post_receipt(receipt_request(
                "catalog-guard-request-2",
                "catalog-guard-key-2",
                "catalog-guard-receipt-2",
                &["GUARD002"],
            ))
            .await
            .expect_err("a party without supplier role must be rejected");
        assert!(matches!(
            wrong_supplier_role,
            ApplicationError::NotFound { entity, .. } if entity == "supplier"
        ));

        let supplier = create_test_party(&database, "供应商 A", CatalogPartyRole::Supplier).await;
        let mut unknown_sku = receipt_request(
            "catalog-guard-request-3",
            "catalog-guard-key-3",
            "catalog-guard-receipt-3",
            &["GUARD003"],
        );
        unknown_sku.sku_code = "UNKNOWN".to_owned();
        unknown_sku.sku_name = "Unknown".to_owned();
        assert!(matches!(
            database
                .post_receipt(unknown_sku)
                .await
                .expect_err("receipt must not create an unknown product"),
            ApplicationError::NotFound { entity, .. } if entity == "sku"
        ));

        let mut wrong_name = receipt_request(
            "catalog-guard-request-4",
            "catalog-guard-key-4",
            "catalog-guard-receipt-4",
            &["GUARD004"],
        );
        wrong_name.sku_name = "型号 X typo".to_owned();
        assert!(matches!(
            database
                .post_receipt(wrong_name)
                .await
                .expect_err("payload product name must match catalog facts"),
            ApplicationError::Conflict { entity, message, .. }
                if entity == "sku" && message.contains("does not match")
        ));

        sqlx::query("UPDATE skus SET active = 0 WHERE id = ?1")
            .bind(&product.sku_id)
            .execute(database.pool())
            .await
            .expect("disable guarded product");
        assert!(matches!(
            database
                .post_receipt(receipt_request(
                    "catalog-guard-request-5",
                    "catalog-guard-key-5",
                    "catalog-guard-receipt-5",
                    &["GUARD005"],
                ))
                .await
                .expect_err("inactive products must be rejected"),
            ApplicationError::Conflict { entity, message, .. }
                if entity == "sku" && message.contains("inactive")
        ));

        sqlx::query("UPDATE skus SET active = 1, tracking_mode = 'quantity' WHERE id = ?1")
            .bind(&product.sku_id)
            .execute(database.pool())
            .await
            .expect("switch guarded product to quantity tracking");
        assert!(matches!(
            database
                .post_receipt(receipt_request(
                    "catalog-guard-request-6",
                    "catalog-guard-key-6",
                    "catalog-guard-receipt-6",
                    &["GUARD006"],
                ))
                .await
                .expect_err("quantity-tracked products must be rejected"),
            ApplicationError::Conflict { entity, message, .. }
                if entity == "sku" && message.contains("serial-tracked")
        ));

        let rejected_facts: (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM inbound_receipts), (SELECT COUNT(*) FROM inventory_units), (SELECT COUNT(*) FROM idempotency_records), (SELECT COUNT(*) FROM audit_logs)",
        )
        .fetch_one(database.pool())
        .await
        .expect("count rejected receipt facts");
        assert_eq!(rejected_facts, (0, 0, 0, 0));

        sqlx::query("UPDATE skus SET tracking_mode = 'serial' WHERE id = ?1")
            .bind(&product.sku_id)
            .execute(database.pool())
            .await
            .expect("restore guarded product serial tracking");
        let response = database
            .post_receipt(receipt_request(
                "catalog-guard-request-7",
                "catalog-guard-key-7",
                "catalog-guard-receipt-7",
                &["GUARD007"],
            ))
            .await
            .expect("valid catalog references should post");
        assert_eq!(
            response.supplier_party_id.as_deref(),
            Some(supplier.party_id.as_str())
        );
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn catalog_entries_persist_and_are_reused_by_inbound_receipts() {
        let (database, path) = test_database().await;
        let product = database
            .create_catalog_product(CreateCatalogProductRequest {
                code: " px-100 ".to_owned(),
                name: "产品 PX".to_owned(),
                serial_prefix: Some("px".to_owned()),
                serial_forbidden_chars: "-, ".to_owned(),
            })
            .await
            .expect("create reusable product");
        let owner = database
            .create_catalog_party(CreateCatalogPartyRequest {
                display_name: "客户 Catalog".to_owned(),
                role: CatalogPartyRole::GoodsOwner,
            })
            .await
            .expect("create reusable goods owner");
        let supplier = database
            .create_catalog_party(CreateCatalogPartyRequest {
                display_name: "供应商 Catalog".to_owned(),
                role: CatalogPartyRole::Supplier,
            })
            .await
            .expect("create reusable supplier");
        assert_ne!(owner.party_id, supplier.party_id);

        database.pool().close().await;
        let database = OfflineDatabase::open(&path)
            .await
            .expect("reopen catalog database");
        let catalog = database
            .list_reference_catalog()
            .await
            .expect("list persisted catalog");
        assert_eq!(catalog.products, vec![product.clone()]);
        assert_eq!(catalog.goods_owners, vec![owner.clone()]);
        assert_eq!(catalog.suppliers, vec![supplier.clone()]);

        let mut request = receipt_request(
            "catalog-request-1",
            "catalog-receipt-key-1",
            "catalog-receipt-1",
            &["PX0001"],
        );
        request.owner_name = owner.display_name.clone();
        request.supplier_name = supplier.display_name.clone();
        request.sku_code = product.code.clone();
        request.sku_name = product.name.clone();
        let response = database
            .post_receipt(request)
            .await
            .expect("post receipt from reusable catalog entries");
        assert_eq!(response.owner_party_id, owner.party_id);
        assert_eq!(
            response.supplier_party_id.as_deref(),
            Some(supplier.party_id.as_str())
        );
        assert_eq!(response.sku_id, product.sku_id);

        let audit_details: String = sqlx::query_scalar(
            "SELECT details_json FROM audit_logs WHERE entity_id = ?1 AND action = 'inbound_receipt.posted'",
        )
        .bind(&response.receipt_id)
        .fetch_one(database.pool())
        .await
        .expect("load receipt audit details");
        let audit_details: Value =
            serde_json::from_str(&audit_details).expect("decode receipt audit details");
        assert_eq!(
            audit_details["supplier_party_id"],
            Value::String(supplier.party_id.clone())
        );

        let (receipt_owner_id, receipt_supplier_id, receipt_sku_id): (
            String,
            Option<String>,
            String,
        ) = sqlx::query_as(
            r#"
            SELECT ir.owner_party_id, ir.supplier_party_id, irl.sku_id
              FROM inbound_receipts ir
              JOIN inbound_receipt_lines irl ON irl.receipt_id = ir.id
             WHERE ir.id = ?1
            "#,
        )
        .bind(&response.receipt_id)
        .fetch_one(database.pool())
        .await
        .expect("load receipt catalog associations");
        assert_eq!(receipt_owner_id, owner.party_id);
        assert_eq!(
            receipt_supplier_id.as_deref(),
            Some(supplier.party_id.as_str())
        );
        assert_eq!(receipt_sku_id, product.sku_id);

        let catalog_after_receipt = database
            .list_reference_catalog()
            .await
            .expect("list reused catalog");
        assert_eq!(catalog_after_receipt.products.len(), 1);
        assert_eq!(catalog_after_receipt.goods_owners.len(), 1);
        assert_eq!(catalog_after_receipt.suppliers.len(), 1);
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn unified_party_contacts_and_supplier_only_receipt_are_persisted() {
        let (database, path) = test_database().await;
        let product = database
            .create_catalog_product(CreateCatalogProductRequest {
                code: "CONTACT-SKU".to_owned(),
                name: "Contact Product".to_owned(),
                serial_prefix: Some("CT".to_owned()),
                serial_forbidden_chars: String::new(),
            })
            .await
            .expect("create contact test product");
        let party = database
            .save_catalog_party(SaveCatalogPartyRequest {
                party_id: None,
                display_name: "  Unified Partner  ".to_owned(),
                roles: vec![
                    CatalogPartyRole::Supplier,
                    CatalogPartyRole::UpstreamReceiver,
                ],
                contact_name: Some("Contact A".to_owned()),
                phone: Some("13800138000".to_owned()),
                wechat: Some("contact-a".to_owned()),
                email: Some("contact@example.com".to_owned()),
                address: Some("Shenzhen".to_owned()),
                notes: Some("Priority".to_owned()),
            })
            .await
            .expect("save unified party");
        assert_eq!(party.display_name, "Unified Partner");
        assert!(party.roles.contains(&CatalogPartyRole::Supplier));
        assert!(party.roles.contains(&CatalogPartyRole::UpstreamReceiver));
        assert_eq!(party.phone.as_deref(), Some("13800138000"));

        let catalog = database
            .list_reference_catalog()
            .await
            .expect("list unified catalog");
        assert_eq!(catalog.parties, vec![party.clone()]);
        assert_eq!(catalog.suppliers, vec![party.clone()]);
        assert!(catalog.goods_owners.is_empty());

        let mut request = receipt_request(
            "supplier-only-request",
            "supplier-only-key",
            "supplier-only-receipt",
            &["CT0001"],
        );
        request.owner_name = party.display_name.clone();
        request.supplier_name = party.display_name.clone();
        request.sku_code = product.code;
        request.sku_name = product.name;
        let receipt = database
            .post_receipt(request)
            .await
            .expect("post supplier-only receipt");
        assert_eq!(receipt.owner_party_id, party.party_id);
        assert_eq!(
            receipt.supplier_party_id.as_deref(),
            Some(party.party_id.as_str())
        );

        let updated = database
            .save_catalog_party(SaveCatalogPartyRequest {
                party_id: Some(party.party_id.clone()),
                display_name: party.display_name,
                roles: vec![CatalogPartyRole::Supplier, CatalogPartyRole::GoodsOwner],
                contact_name: Some("Contact B".to_owned()),
                phone: Some("0755-12345678".to_owned()),
                wechat: None,
                email: None,
                address: Some("Guangzhou".to_owned()),
                notes: None,
            })
            .await
            .expect("update unified party");
        assert_eq!(updated.party_id, receipt.owner_party_id);
        assert_eq!(updated.contact_name.as_deref(), Some("Contact B"));
        let party_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM business_parties WHERE workspace_id = ?1")
                .bind(database.workspace_id())
                .fetch_one(database.pool())
                .await
                .expect("count unified parties");
        assert_eq!(party_count, 1);
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn sku_scan_rules_reject_wrong_prefix_and_forbidden_characters() {
        let (database, path) = test_database().await;
        seed_default_receipt_parties(&database).await;
        let product = database
            .create_catalog_product(CreateCatalogProductRequest {
                code: "RULED-SKU".to_owned(),
                name: "受控扫码产品".to_owned(),
                serial_prefix: Some("snx".to_owned()),
                serial_forbidden_chars: "-, ".to_owned(),
            })
            .await
            .expect("create product with scan safeguards");

        let mut wrong_prefix = receipt_request(
            "rules-request-1",
            "rules-receipt-key-1",
            "rules-receipt-1",
            &["OTHER001"],
        );
        wrong_prefix.sku_code = product.code.clone();
        wrong_prefix.sku_name = product.name.clone();
        let prefix_error = database
            .post_receipt(wrong_prefix)
            .await
            .expect_err("wrong product prefix must be rejected");
        assert!(matches!(
            &prefix_error,
            ApplicationError::Validation { field, message }
                if field == "barcodes" && message.contains("prefix SNX")
        ));

        let mut forbidden_character = receipt_request(
            "rules-request-2",
            "rules-receipt-key-2",
            "rules-receipt-2",
            &["SNX-001"],
        );
        forbidden_character.sku_code = product.code.clone();
        forbidden_character.sku_name = product.name.clone();
        let forbidden_error = database
            .post_receipt(forbidden_character)
            .await
            .expect_err("forbidden SN character must be rejected");
        assert!(matches!(
            &forbidden_error,
            ApplicationError::Validation { field, message }
                if field == "barcodes" && message.contains("forbidden") && message.contains('-')
        ));

        let receipt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbound_receipts")
            .fetch_one(database.pool())
            .await
            .expect("count rejected receipts");
        let unit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inventory_units")
            .fetch_one(database.pool())
            .await
            .expect("count rejected receipt units");
        assert_eq!(receipt_count, 0);
        assert_eq!(unit_count, 0);

        let mut valid = receipt_request(
            "rules-request-3",
            "rules-receipt-key-3",
            "rules-receipt-3",
            &["SNX001"],
        );
        valid.sku_code = product.code.clone();
        valid.sku_name = product.name.clone();
        let response = database
            .post_receipt(valid)
            .await
            .expect("valid product SN should be accepted");
        assert_eq!(response.sku_id, product.sku_id);
        assert_eq!(response.units[0].barcode, "SNX001");
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn workspace_wide_duplicate_barcode_rolls_back_the_entire_receipt() {
        let (database, path) = test_database_with_default_catalog().await;
        database
            .post_receipt(receipt_request(
                "request-1",
                "receipt-key-1",
                "R-1",
                &["DUP-1"],
            ))
            .await
            .expect("seed existing barcode");
        create_test_party(&database, "客户 B", CatalogPartyRole::GoodsOwner).await;
        create_test_party(&database, "供应商 B", CatalogPartyRole::Supplier).await;
        database
            .create_catalog_product(CreateCatalogProductRequest {
                code: "MODEL-Y".to_owned(),
                name: "型号 Y".to_owned(),
                serial_prefix: None,
                serial_forbidden_chars: String::new(),
            })
            .await
            .expect("create conflicting receipt product");

        let mut conflicting = receipt_request(
            "request-2",
            "receipt-key-2",
            "R-2",
            &["NEW-BARCODE", "dup-1"],
        );
        conflicting.owner_name = "客户 B".to_owned();
        conflicting.supplier_name = "供应商 B".to_owned();
        conflicting.sku_code = "MODEL-Y".to_owned();
        conflicting.sku_name = "型号 Y".to_owned();
        let error = database
            .post_receipt(conflicting)
            .await
            .expect_err("the entire batch must be rejected");
        assert!(matches!(
            error,
            ApplicationError::Conflict {
                ref entity,
                ref key,
                ..
            } if entity == "inventory_barcode" && key == "DUP-1"
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
        let catalog_owner_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM business_parties WHERE normalized_name = '客户 b'",
        )
        .fetch_one(database.pool())
        .await
        .expect("check catalog owner");
        let catalog_supplier_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM business_parties WHERE normalized_name = '供应商 b'",
        )
        .fetch_one(database.pool())
        .await
        .expect("check catalog supplier");
        let catalog_sku_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM skus WHERE code = 'MODEL-Y'")
                .fetch_one(database.pool())
                .await
                .expect("check catalog SKU");
        assert_eq!(receipt_count, 1);
        assert_eq!(unit_count, 1);
        assert_eq!(rolled_back_unit, 0);
        assert_eq!(catalog_owner_count, 1);
        assert_eq!(catalog_supplier_count, 1);
        assert_eq!(catalog_sku_count, 1);
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn receipt_replay_is_idempotent_and_units_default_to_received_untested() {
        let (database, path) = test_database_with_default_catalog().await;
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
    async fn quality_labels_are_multi_value_persistent_and_snapshot_inspection_history() {
        let (database, path) = test_database_with_default_catalog().await;
        let available = database
            .save_quality_label(SaveQualityLabelRequest {
                quality_label_id: None,
                name: "  外观完好  ".to_owned(),
                disposition: QualityLabelDisposition::Available,
                active: true,
                rename_note: None,
            })
            .await
            .expect("create available quality label");
        let quarantine = database
            .save_quality_label(SaveQualityLabelRequest {
                quality_label_id: None,
                name: "屏幕异常".to_owned(),
                disposition: QualityLabelDisposition::Quarantine,
                active: true,
                rename_note: None,
            })
            .await
            .expect("create quarantine quality label");
        assert_eq!(database.list_quality_labels().await.unwrap().len(), 2);

        database
            .post_receipt(receipt_request(
                "quality-label-receipt-request",
                "quality-label-receipt-key",
                "R-QUALITY-LABEL",
                &["LABEL-SNAPSHOT", "LABEL-INACTIVE", "LABEL-MISMATCH"],
            ))
            .await
            .expect("post quality label test receipt");
        let mut inspection = inspection_request(
            "quality-label-inspection-request",
            "quality-label-inspection-key",
            "Q-QUALITY-LABEL",
            InspectionKind::Initial,
            vec![("LABEL-SNAPSHOT", QualityOutcome::Passed)],
        );
        inspection.results[0].quality_label_id = Some(available.quality_label_id.clone());
        database
            .complete_inspection(inspection)
            .await
            .expect("complete labeled inspection");

        let renamed = database
            .save_quality_label(SaveQualityLabelRequest {
                quality_label_id: Some(available.quality_label_id.clone()),
                name: "验收通过".to_owned(),
                disposition: QualityLabelDisposition::Available,
                active: false,
                rename_note: Some("统一验收用语".to_owned()),
            })
            .await
            .expect("rename and deactivate quality label");
        assert_eq!(renamed.usage_count, 1);
        assert_eq!(renamed.name_history.len(), 1);
        assert_eq!(renamed.name_history[0].old_name, "外观完好");
        assert_eq!(renamed.name_history[0].new_name, "验收通过");
        assert_eq!(
            renamed.name_history[0].change_note.as_deref(),
            Some("统一验收用语")
        );
        assert_eq!(renamed.name_history[0].changed_by, "本机操作");
        let trace = database
            .inventory_trace("LABEL-SNAPSHOT")
            .await
            .expect("trace labeled inventory");
        assert_eq!(
            trace.inspections[0].quality_label_snapshot.as_deref(),
            Some("外观完好")
        );

        assert!(matches!(
            database
                .save_quality_label(SaveQualityLabelRequest {
                    quality_label_id: Some(available.quality_label_id.clone()),
                    name: "验收通过".to_owned(),
                    disposition: QualityLabelDisposition::Quarantine,
                    active: false,
                    rename_note: None,
                })
                .await,
            Err(ApplicationError::Conflict { entity, .. }) if entity == "quality_label_disposition"
        ));
        let history_id = renamed.name_history[0].history_id.clone();
        assert!(sqlx::query(
            "UPDATE quality_label_name_history SET new_name = '篡改' WHERE id = ?1"
        )
        .bind(&history_id)
        .execute(database.pool())
        .await
        .is_err());
        assert!(
            sqlx::query("DELETE FROM quality_label_name_history WHERE id = ?1")
                .bind(&history_id)
                .execute(database.pool())
                .await
                .is_err()
        );

        let mut inactive = inspection_request(
            "quality-label-inactive-request",
            "quality-label-inactive-key",
            "Q-QUALITY-INACTIVE",
            InspectionKind::Initial,
            vec![("LABEL-INACTIVE", QualityOutcome::Passed)],
        );
        inactive.results[0].quality_label_id = Some(available.quality_label_id.clone());
        assert!(matches!(
            database.complete_inspection(inactive).await,
            Err(ApplicationError::Conflict { entity, .. }) if entity == "quality_label"
        ));

        let mut mismatched = inspection_request(
            "quality-label-mismatch-request",
            "quality-label-mismatch-key",
            "Q-QUALITY-MISMATCH",
            InspectionKind::Initial,
            vec![("LABEL-MISMATCH", QualityOutcome::Passed)],
        );
        mismatched.results[0].quality_label_id = Some(quarantine.quality_label_id);
        assert!(matches!(
            database.complete_inspection(mismatched).await,
            Err(ApplicationError::Conflict { entity, .. })
                if entity == "quality_label_disposition"
        ));
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn inspection_batch_is_atomic_when_any_unit_has_an_invalid_transition() {
        let (database, path) = test_database_with_default_catalog().await;
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
        let (database, path) = test_database_with_default_catalog().await;
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
        assert_eq!(summary.products.len(), 1);
        assert_eq!(summary.products[0].sku_code, "MODEL-X");
        assert_eq!(summary.products[0].on_hand_units, 2);
        assert_eq!(summary.products[0].suppliers.len(), 1);
        assert_eq!(summary.products[0].suppliers[0].supplier_name, "供应商 A");
        assert_eq!(summary.products[0].suppliers[0].on_hand_units, 2);

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
    async fn inventory_summary_groups_on_hand_products_by_supplier_and_excludes_departed_units() {
        let (database, path) = test_database_with_default_catalog().await;
        create_test_party(&database, "供应商 B", CatalogPartyRole::Supplier).await;

        database
            .post_receipt(receipt_request(
                "summary-source-a-request",
                "summary-source-a-key",
                "R-SUMMARY-A",
                &["SUMMARY-A-1"],
            ))
            .await
            .expect("post first supplier receipt");
        let mut source_b = receipt_request(
            "summary-source-b-request",
            "summary-source-b-key",
            "R-SUMMARY-B",
            &["SUMMARY-B-1", "SUMMARY-B-2"],
        );
        source_b.supplier_name = "供应商 B".to_owned();
        database
            .post_receipt(source_b)
            .await
            .expect("post second supplier receipt");

        sqlx::query(
            "UPDATE inventory_units SET inventory_status = 'delivered' WHERE barcode = 'SUMMARY-B-2'",
        )
        .execute(database.pool())
        .await
        .expect("mark one unit as departed");

        let summary = database
            .inventory_summary(InventorySummaryQuery::default())
            .await
            .expect("summarize supplier stock");
        assert_eq!(summary.total_units, 3);
        assert_eq!(summary.inventory.received, 2);
        assert_eq!(summary.inventory.delivered, 1);
        assert_eq!(summary.products.len(), 1);
        let product = &summary.products[0];
        assert_eq!(product.on_hand_units, 2);
        assert_eq!(product.inventory.received, 2);
        assert_eq!(product.suppliers.len(), 2);
        assert_eq!(product.suppliers[0].supplier_name, "供应商 A");
        assert_eq!(product.suppliers[0].on_hand_units, 1);
        assert_eq!(product.suppliers[1].supplier_name, "供应商 B");
        assert_eq!(product.suppliers[1].on_hand_units, 1);
        close_and_remove(database, path).await;
    }

    #[tokio::test]
    async fn archived_offline_workspace_rejects_new_writes() {
        let (database, path) = test_database_with_default_catalog().await;
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
            .mark_read_only("export-1", &"a".repeat(64))
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
