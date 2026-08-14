//! PostgreSQL quality and outbound workflows.
//!
//! Every method in this module follows the same boundary:
//!
//! 1. establish tenant/RLS context and authenticate a bearer session;
//! 2. derive the actor, membership, device and session from that session;
//! 3. lock the rows which form the business decision;
//! 4. write the document, inventory projection, append-only movement, audit
//!    event and idempotency response in one transaction.
//!
//! The request types intentionally do not contain `tenant_id` or any actor
//! identity. `tenant_id` is a transport route parameter in the current HTTP
//! adapter and is checked against the session by `NetworkDatabase`; it is not
//! trusted as a business DTO field.

use super::application::{CompleteInspectionResponse, InspectedUnit};
use super::domain::{InspectionKind, InventoryStatus, QualityOutcome, QualityStatus};
use super::network::{NetworkResult, NetworkService, NetworkServiceError};
use super::outbound::{
    AllocationItem, ConfirmOutboundDeliveryResponse, CreateOutboundOrderResponse,
    ReturnOutboundShipmentResponse, ShipOutboundResponse, ShipmentItem,
};
use super::warranty::{resolve_warranty, WarrantyInput};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const PERMISSION_QUALITY_WRITE: &str = "inventory.quality.write";
pub const PERMISSION_ORDER_WRITE: &str = "inventory.order.write";
pub const PERMISSION_ALLOCATION_WRITE: &str = "inventory.allocation.write";
pub const PERMISSION_SHIPMENT_WRITE: &str = "inventory.shipment.write";
pub const PERMISSION_DELIVERY_WRITE: &str = "inventory.delivery.write";
pub const PERMISSION_RETURN_WRITE: &str = "inventory.return.write";

const INSPECTION_SCOPE: &str = "complete_quality_inspection";
const ORDER_SCOPE: &str = "create_outbound_order";
const ALLOCATION_SCOPE: &str = "allocate_outbound_order";
const SHIPMENT_SCOPE: &str = "ship_outbound_order";
const DELIVERY_SCOPE: &str = "confirm_outbound_delivery";
const RETURN_SCOPE: &str = "return_outbound_shipment";

/// Network inspection input. The inspector is the authenticated session
/// actor, not a client supplied field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkCompleteInspectionRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub inspection_no: String,
    pub inspection_kind: InspectionKind,
    pub inspected_at: String,
    pub results: Vec<NetworkInspectionResultInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInspectionResultInput {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkCreateOutboundOrderRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub order_no: String,
    pub upstream_receiver_name: String,
    pub sku_code: String,
    pub sku_name: String,
    pub required_quantity: u32,
    pub required_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkAllocateOutboundRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub order_id: Uuid,
    pub order_line_id: Uuid,
    /// Empty means FIFO. A non-empty list requests explicit barcodes.
    #[serde(default)]
    pub barcodes: Vec<String>,
    /// A scan-first order may contain several SKUs. The selected units are
    /// grouped into order lines by their actual inventory SKU in this mode.
    #[serde(default)]
    pub allow_mixed_skus: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkShipOutboundRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub order_id: Uuid,
    pub shipment_no: String,
    #[serde(default)]
    pub allocation_ids: Vec<Uuid>,
    #[serde(default)]
    pub barcodes: Vec<String>,
    pub shipped_at: String,
    #[serde(default)]
    pub warranty: Option<WarrantyInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfirmOutboundDeliveryRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub shipment_id: Uuid,
    pub confirmation_code: String,
    #[serde(default)]
    pub shipment_line_ids: Vec<Uuid>,
    pub confirmed_at: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkReturnOutboundShipmentRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub shipment_id: Uuid,
    #[serde(default)]
    pub shipment_line_ids: Vec<Uuid>,
    pub return_no: String,
    pub returned_at: String,
    pub reason: String,
}

impl NetworkService {
    /// Compatibility name matching the offline application service. Network
    /// callers should still use [`Self::complete_quality_inspection`] when the
    /// quality concern should be explicit at the route boundary.
    pub async fn complete_inspection(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: NetworkCompleteInspectionRequest,
    ) -> NetworkResult<CompleteInspectionResponse> {
        self.complete_quality_inspection(tenant_id, session_token, request)
            .await
    }

    /// Complete an initial inspection or a retest. A failed unit is moved to
    /// the quarantine location; a passed unit becomes available in storage.
    pub async fn complete_quality_inspection(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: NetworkCompleteInspectionRequest,
    ) -> NetworkResult<CompleteInspectionResponse> {
        let request = normalize_inspection(request)?;
        let digest = request_digest(&request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_QUALITY_WRITE)
            .await?;
        let identity = authorized.session();
        let actor_id = identity.identity.user_id;
        let membership_id = identity.identity.membership_id;
        let device_id = identity.device_id;
        let session_id = identity.session_id;
        let transaction = authorized.sqlx_transaction();

        if let Some(mut replay) = claim_idempotency::<CompleteInspectionResponse>(
            transaction,
            tenant_id,
            INSPECTION_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            authorized.commit().await?;
            return Ok(replay);
        }

        let storage_location = location_for_kind(transaction, tenant_id, "storage").await?;
        let quarantine_location = location_for_kind(transaction, tenant_id, "quarantine").await?;
        let inspection_id = Uuid::now_v7();

        // Lock and validate every unit before writing any inspection fact. A
        // failure in any row rolls the whole batch back.
        let mut rows = Vec::with_capacity(request.results.len());
        let mut seen = HashSet::with_capacity(request.results.len());
        for result in &request.results {
            if !seen.insert(result.barcode.as_str()) {
                return Err(NetworkServiceError::Conflict {
                    entity: "inspection_barcode".to_owned(),
                    key: result.barcode.clone(),
                });
            }
            let (quality_label_id, quality_label_snapshot) =
                if let Some(quality_label_id) = &result.quality_label_id {
                    let quality_label_id = Uuid::parse_str(quality_label_id).map_err(|_| {
                        NetworkServiceError::Invalid("quality_label_id must be a UUID".to_owned())
                    })?;
                    let label = sqlx::query(
                        r#"
                        SELECT name, disposition, active
                          FROM quality_labels
                         WHERE tenant_id = $1 AND id = $2
                         FOR SHARE
                        "#,
                    )
                    .bind(tenant_id)
                    .bind(quality_label_id)
                    .fetch_optional(&mut **transaction)
                    .await?
                    .ok_or_else(|| NetworkServiceError::Conflict {
                        entity: "quality_label".to_owned(),
                        key: quality_label_id.to_string(),
                    })?;
                    let name: String = label.try_get("name")?;
                    let disposition: String = label.try_get("disposition")?;
                    let active: bool = label.try_get("active")?;
                    if !active {
                        return Err(NetworkServiceError::Conflict {
                            entity: "quality_label_inactive".to_owned(),
                            key: name,
                        });
                    }
                    let expected_outcome = match disposition.as_str() {
                        "available" => QualityOutcome::Passed,
                        "quarantine" => QualityOutcome::Failed,
                        other => {
                            return Err(NetworkServiceError::Invalid(format!(
                                "unknown quality label disposition {other}"
                            )))
                        }
                    };
                    if expected_outcome != result.outcome {
                        return Err(NetworkServiceError::Conflict {
                            entity: "quality_label_disposition".to_owned(),
                            key: name,
                        });
                    }
                    (Some(quality_label_id), Some(name))
                } else {
                    (None, None)
                };
            let row = sqlx::query(
                r#"
                SELECT id, barcode, location_id, inventory_status, quality_status, version
                  FROM inventory_units
                 WHERE tenant_id = $1 AND barcode = $2
                 FOR UPDATE
                "#,
            )
            .bind(tenant_id)
            .bind(&result.barcode)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or_else(|| NetworkServiceError::Conflict {
                entity: "inventory_barcode".to_owned(),
                key: result.barcode.clone(),
            })?;
            let inventory_status: String = row.try_get("inventory_status")?;
            let quality_status: String = row.try_get("quality_status")?;
            if !inspection_transition_allowed(
                request.inspection_kind,
                &inventory_status,
                &quality_status,
            ) {
                return Err(NetworkServiceError::Conflict {
                    entity: "quality_transition".to_owned(),
                    key: result.barcode.clone(),
                });
            }
            rows.push(InspectionRow {
                id: row.try_get("id")?,
                barcode: row.try_get("barcode")?,
                old_location: row.try_get("location_id")?,
                old_version: row.try_get("version")?,
                result: result.clone(),
                quality_label_id,
                quality_label_snapshot,
            });
        }

        sqlx::query(
            r#"
            INSERT INTO quality_inspections
                (tenant_id, id, inspection_no, inspection_type, status,
                 inspector_id, inspected_at, idempotency_key, request_id)
            VALUES ($1, $2, $3, $4, 'completed', $5, $6::timestamptz, $7, $8)
            "#,
        )
        .bind(tenant_id)
        .bind(inspection_id)
        .bind(&request.inspection_no)
        .bind(inspection_kind_name(request.inspection_kind))
        .bind(actor_id)
        .bind(&request.inspected_at)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .execute(&mut **transaction)
        .await
        .map_err(|error| conflict_or_sqlx("quality_inspection", &request.inspection_no, error))?;

        let mut units = Vec::with_capacity(rows.len());
        let mut passed_count = 0_u32;
        let mut failed_count = 0_u32;
        for row in rows {
            let result = row.result;
            let outcome_name = quality_outcome_name(result.outcome);
            let (quality_status, inventory_status, destination) = match result.outcome {
                QualityOutcome::Passed => ("passed", "available", storage_location),
                QualityOutcome::Failed => ("failed", "quarantined", quarantine_location),
            };
            let measurements_json = serde_json::to_string(&result.measurements)?;
            sqlx::query(
                r#"
                INSERT INTO quality_inspection_results
                    (tenant_id, id, inspection_id, inventory_unit_id, result,
                     quality_label_id, quality_label_snapshot, defect_code,
                     measurements_json, notes)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::jsonb, $10)
                "#,
            )
            .bind(tenant_id)
            .bind(Uuid::now_v7())
            .bind(inspection_id)
            .bind(row.id)
            .bind(outcome_name)
            .bind(row.quality_label_id)
            .bind(&row.quality_label_snapshot)
            .bind(&result.defect_code)
            .bind(measurements_json)
            .bind(&result.notes)
            .execute(&mut **transaction)
            .await?;

            let updated = sqlx::query(
                r#"
                UPDATE inventory_units
                   SET location_id = $1,
                       inventory_status = $2,
                       quality_status = $3,
                       version = version + 1,
                       updated_at = CURRENT_TIMESTAMP
                 WHERE tenant_id = $4 AND id = $5 AND version = $6
                "#,
            )
            .bind(destination)
            .bind(inventory_status)
            .bind(quality_status)
            .bind(tenant_id)
            .bind(row.id)
            .bind(row.old_version)
            .execute(&mut **transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "inventory_unit_version".to_owned(),
                    key: row.barcode.clone(),
                });
            }
            sqlx::query(
                r#"
                INSERT INTO stock_movements
                    (tenant_id, id, inventory_unit_id, movement_type,
                     from_location_id, to_location_id, source_type, source_id,
                     actor_id, occurred_at)
                VALUES ($1, $2, $3, 'moved', $4, $5, 'quality_inspection', $6,
                        $7, $8::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(Uuid::now_v7())
            .bind(row.id)
            .bind(row.old_location)
            .bind(destination)
            .bind(inspection_id)
            .bind(actor_id)
            .bind(&request.inspected_at)
            .execute(&mut **transaction)
            .await?;

            let parsed_inventory = parse_inventory_status(inventory_status)?;
            let parsed_quality = parse_quality_status(quality_status)?;
            if result.outcome == QualityOutcome::Passed {
                passed_count += 1;
            } else {
                failed_count += 1;
            }
            units.push(InspectedUnit {
                inventory_unit_id: row.id.to_string(),
                barcode: row.barcode,
                outcome: result.outcome,
                inventory_status: parsed_inventory,
                quality_status: parsed_quality,
                location_id: destination.to_string(),
                version: row.old_version as u64 + 1,
            });
        }

        let response = CompleteInspectionResponse {
            inspection_id: inspection_id.to_string(),
            inspection_no: request.inspection_no.clone(),
            inspected_count: units.len() as u32,
            passed_count,
            failed_count,
            units,
            idempotent_replay: false,
        };
        insert_audit(
            transaction,
            tenant_id,
            actor_id,
            membership_id,
            device_id,
            session_id,
            "quality_inspection.completed",
            inspection_id,
            &request.request_id,
            json!({
                "inspection_no": request.inspection_no,
                "inspection_kind": inspection_kind_name(request.inspection_kind),
                "inspected_count": response.inspected_count,
                "passed_count": response.passed_count,
                "failed_count": response.failed_count,
            }),
            &request.inspected_at,
        )
        .await?;
        finish_idempotency(
            transaction,
            tenant_id,
            INSPECTION_SCOPE,
            &request.idempotency_key,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn create_outbound_order(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: NetworkCreateOutboundOrderRequest,
    ) -> NetworkResult<CreateOutboundOrderResponse> {
        let request = normalize_create_order(request)?;
        let digest = request_digest(&request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_ORDER_WRITE)
            .await?;
        let identity = authorized.session();
        let actor_id = identity.identity.user_id;
        let membership_id = identity.identity.membership_id;
        let device_id = identity.device_id;
        let session_id = identity.session_id;
        let transaction = authorized.sqlx_transaction();
        if let Some(mut replay) = claim_idempotency::<CreateOutboundOrderResponse>(
            transaction,
            tenant_id,
            ORDER_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            authorized.commit().await?;
            return Ok(replay);
        }
        let receiver_id = upsert_party(
            transaction,
            tenant_id,
            &request.upstream_receiver_name,
            "upstream_receiver",
        )
        .await?;
        let sku_id =
            upsert_sku(transaction, tenant_id, &request.sku_code, &request.sku_name).await?;
        let order_id = Uuid::now_v7();
        let line_id = Uuid::now_v7();
        let now = UtcNow::value();
        sqlx::query(
            r#"
            INSERT INTO outbound_orders
                (tenant_id, id, order_no, upstream_receiver_id, required_at,
                 status, actor_id, idempotency_key, request_id)
            VALUES ($1, $2, $3, $4, $5::timestamptz, 'open', $6, $7, $8)
            "#,
        )
        .bind(tenant_id)
        .bind(order_id)
        .bind(&request.order_no)
        .bind(receiver_id)
        .bind(&request.required_at)
        .bind(actor_id)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .execute(&mut **transaction)
        .await
        .map_err(|error| conflict_or_sqlx("outbound_order", &request.order_no, error))?;
        sqlx::query(
            r#"
            INSERT INTO outbound_order_lines
                (tenant_id, id, outbound_order_id, sku_id, required_quantity)
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(tenant_id)
        .bind(line_id)
        .bind(order_id)
        .bind(sku_id)
        .bind(request.required_quantity as i32)
        .execute(&mut **transaction)
        .await?;
        let response = CreateOutboundOrderResponse {
            order_id: order_id.to_string(),
            order_line_id: line_id.to_string(),
            order_no: request.order_no.clone(),
            upstream_receiver_id: receiver_id.to_string(),
            sku_id: sku_id.to_string(),
            required_quantity: request.required_quantity,
            idempotent_replay: false,
        };
        insert_audit(
            transaction,
            tenant_id,
            actor_id,
            membership_id,
            device_id,
            session_id,
            "outbound_order.created",
            order_id,
            &request.request_id,
            json!({"order_no": request.order_no, "required_quantity": request.required_quantity}),
            &now,
        )
        .await?;
        finish_idempotency(
            transaction,
            tenant_id,
            ORDER_SCOPE,
            &request.idempotency_key,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn allocate_outbound_order(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: NetworkAllocateOutboundRequest,
    ) -> NetworkResult<super::outbound::AllocateOutboundResponse> {
        let request = normalize_allocate(request)?;
        let digest = request_digest(&request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_ALLOCATION_WRITE)
            .await?;
        let identity = authorized.session();
        let actor_id = identity.identity.user_id;
        let membership_id = identity.identity.membership_id;
        let device_id = identity.device_id;
        let session_id = identity.session_id;
        let transaction = authorized.sqlx_transaction();
        if let Some(mut replay) = claim_idempotency::<super::outbound::AllocateOutboundResponse>(
            transaction,
            tenant_id,
            ALLOCATION_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            authorized.commit().await?;
            return Ok(replay);
        }

        let line = sqlx::query(
            r#"
            SELECT outbound_order_id, sku_id, required_quantity,
                   allocated_quantity
              FROM outbound_order_lines
             WHERE tenant_id = $1 AND id = $2
             FOR UPDATE
            "#,
        )
        .bind(tenant_id)
        .bind(request.order_line_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| NetworkServiceError::Conflict {
            entity: "outbound_order_line".to_owned(),
            key: request.order_line_id.to_string(),
        })?;
        let line_order_id: Uuid = line.try_get("outbound_order_id")?;
        if line_order_id != request.order_id {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_order_line".to_owned(),
                key: request.order_line_id.to_string(),
            });
        }
        let order_status: String = sqlx::query_scalar(
            "SELECT status FROM outbound_orders WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(tenant_id)
        .bind(request.order_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| NetworkServiceError::Conflict {
            entity: "outbound_order".to_owned(),
            key: request.order_id.to_string(),
        })?;
        if matches!(order_status.as_str(), "voided" | "shipped" | "completed") {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_order_status".to_owned(),
                key: order_status,
            });
        }
        let sku_id: Uuid = line.try_get("sku_id")?;
        let required = i64::from(line.try_get::<i32, _>("required_quantity")?);
        let allocated = i64::from(line.try_get::<i32, _>("allocated_quantity")?);
        let remaining = required - allocated;
        if remaining <= 0 {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_order_line".to_owned(),
                key: "fully_allocated".to_owned(),
            });
        }
        if request.allow_mixed_skus && request.barcodes.is_empty() {
            return Err(NetworkServiceError::Invalid(
                "mixed SKU allocation requires scanned barcodes".to_owned(),
            ));
        }
        if request.allow_mixed_skus && allocated != 0 {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_order_line".to_owned(),
                key: "mixed_allocation_already_started".to_owned(),
            });
        }
        let shipping_location = location_for_kind(transaction, tenant_id, "shipping").await?;
        let candidates = load_allocation_candidates(
            transaction,
            tenant_id,
            sku_id,
            remaining,
            &request.barcodes,
        )
        .await?;
        if candidates.is_empty() {
            return Err(NetworkServiceError::Conflict {
                entity: "inventory".to_owned(),
                key: "no_quality_passed_available_unit".to_owned(),
            });
        }
        if candidates.len() as i64 > remaining {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_order_line".to_owned(),
                key: "allocation_exceeds_requirement".to_owned(),
            });
        }
        if request.allow_mixed_skus && candidates.len() as i64 != required {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_order_line".to_owned(),
                key: format!("scanned_{}_required_{}", candidates.len(), required),
            });
        }

        let mut candidate_plans: Vec<(AllocationCandidate, Uuid, Uuid, i64)> =
            Vec::with_capacity(candidates.len());
        let mut line_requirements: HashMap<Uuid, i64> = HashMap::new();
        let mut line_allocated: HashMap<Uuid, i64> = HashMap::new();
        if request.allow_mixed_skus {
            let mut groups: Vec<(Uuid, Vec<AllocationCandidate>)> = Vec::new();
            for candidate in candidates {
                if let Some((_, group_candidates)) = groups
                    .iter_mut()
                    .find(|(group_sku, _)| *group_sku == candidate.sku_id)
                {
                    group_candidates.push(candidate);
                } else {
                    groups.push((candidate.sku_id, vec![candidate]));
                }
            }
            for (index, (group_sku, group_candidates)) in groups.into_iter().enumerate() {
                let line_id = if index == 0 {
                    request.order_line_id
                } else {
                    Uuid::now_v7()
                };
                let group_required = i64::try_from(group_candidates.len()).map_err(|_| {
                    NetworkServiceError::Invalid(
                        "mixed SKU group exceeds supported quantity".to_owned(),
                    )
                })?;
                if index == 0 {
                    sqlx::query(
                        r#"
                        UPDATE outbound_order_lines
                           SET sku_id = $1, required_quantity = $2,
                               allocated_quantity = 0, shipped_quantity = 0,
                               delivered_quantity = 0
                         WHERE tenant_id = $3 AND id = $4
                           AND allocated_quantity = 0
                        "#,
                    )
                    .bind(group_sku)
                    .bind(group_required as i32)
                    .bind(tenant_id)
                    .bind(line_id)
                    .execute(&mut **transaction)
                    .await?;
                } else {
                    sqlx::query(
                        r#"
                        INSERT INTO outbound_order_lines
                            (tenant_id, id, outbound_order_id, sku_id,
                             required_quantity, allocated_quantity,
                             shipped_quantity, delivered_quantity)
                        VALUES ($1, $2, $3, $4, $5, 0, 0, 0)
                        "#,
                    )
                    .bind(tenant_id)
                    .bind(line_id)
                    .bind(request.order_id)
                    .bind(group_sku)
                    .bind(group_required as i32)
                    .execute(&mut **transaction)
                    .await?;
                }
                line_requirements.insert(line_id, group_required);
                line_allocated.insert(line_id, 0);
                for candidate in group_candidates {
                    candidate_plans.push((candidate, line_id, group_sku, group_required));
                }
            }
        } else {
            line_requirements.insert(request.order_line_id, required);
            line_allocated.insert(request.order_line_id, allocated);
            for candidate in candidates {
                candidate_plans.push((candidate, request.order_line_id, sku_id, required));
            }
        }

        let now = UtcNow::value();
        let mut allocations = Vec::with_capacity(candidate_plans.len());
        for (candidate, line_id, line_sku_id, line_required) in candidate_plans {
            if (!request.allow_mixed_skus && candidate.sku_id != sku_id)
                || candidate.inventory_status != "available"
                || !matches!(candidate.quality_status.as_str(), "passed" | "waived")
            {
                return Err(NetworkServiceError::Conflict {
                    entity: "inventory_barcode".to_owned(),
                    key: candidate.barcode,
                });
            }
            let current_line_allocated = *line_allocated.get(&line_id).ok_or_else(|| {
                NetworkServiceError::Invalid("missing outbound line allocation state".to_owned())
            })?;
            if current_line_allocated + 1 > line_required {
                return Err(NetworkServiceError::Conflict {
                    entity: "outbound_order_line".to_owned(),
                    key: "allocation_exceeds_requirement".to_owned(),
                });
            }
            let allocation_id = Uuid::now_v7();
            let updated = sqlx::query(
                r#"
                UPDATE inventory_units
                   SET inventory_status = 'reserved', location_id = $1,
                       version = version + 1, updated_at = CURRENT_TIMESTAMP
                 WHERE tenant_id = $2 AND id = $3 AND version = $4
                   AND inventory_status = 'available'
                   AND quality_status IN ('passed', 'waived')
                "#,
            )
            .bind(shipping_location)
            .bind(tenant_id)
            .bind(candidate.id)
            .bind(candidate.version)
            .execute(&mut **transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "inventory_barcode".to_owned(),
                    key: candidate.barcode,
                });
            }
            sqlx::query(
                r#"
                INSERT INTO outbound_allocations
                    (tenant_id, id, outbound_order_line_id, inventory_unit_id,
                     sku_id, status, allocated_by, allocated_at)
                VALUES ($1, $2, $3, $4, $5, 'active', $6, $7::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(allocation_id)
            .bind(line_id)
            .bind(candidate.id)
            .bind(line_sku_id)
            .bind(actor_id)
            .bind(&now)
            .execute(&mut **transaction)
            .await
            .map_err(|error| conflict_or_sqlx("outbound_allocation", &candidate.barcode, error))?;
            insert_movement(
                transaction,
                tenant_id,
                candidate.id,
                "reserved",
                Some(candidate.location_id),
                Some(shipping_location),
                "outbound_order_line",
                line_id,
                actor_id,
                &now,
            )
            .await?;
            allocations.push(AllocationItem {
                allocation_id: allocation_id.to_string(),
                barcode: candidate.barcode,
                owner_party_id: candidate.owner_party_id.to_string(),
                sku_id: candidate.sku_id.to_string(),
            });
            line_allocated.insert(line_id, current_line_allocated + 1);
        }
        for (line_id, allocated_quantity) in &line_allocated {
            let required_quantity = line_requirements.get(line_id).copied().ok_or_else(|| {
                NetworkServiceError::Invalid("missing outbound line requirement".to_owned())
            })?;
            sqlx::query(
                "UPDATE outbound_order_lines SET allocated_quantity = $1 WHERE tenant_id = $2 AND id = $3 AND $1 <= required_quantity",
            )
            .bind(*allocated_quantity as i32)
            .bind(tenant_id)
            .bind(*line_id)
            .execute(&mut **transaction)
            .await?;
            if *allocated_quantity > required_quantity {
                return Err(NetworkServiceError::Conflict {
                    entity: "outbound_order_line".to_owned(),
                    key: "allocation_exceeds_requirement".to_owned(),
                });
            }
        }
        let status = if request.allow_mixed_skus {
            "allocated"
        } else if line_allocated
            .get(&request.order_line_id)
            .copied()
            .unwrap_or_default()
            >= required
        {
            "allocated"
        } else {
            "partially_allocated"
        };
        sqlx::query("UPDATE outbound_orders SET status = $1 WHERE tenant_id = $2 AND id = $3")
            .bind(status)
            .bind(tenant_id)
            .bind(request.order_id)
            .execute(&mut **transaction)
            .await?;
        let response = super::outbound::AllocateOutboundResponse {
            order_id: request.order_id.to_string(),
            order_line_id: request.order_line_id.to_string(),
            allocated_count: allocations.len() as u32,
            order_status: status.to_owned(),
            allocations,
            idempotent_replay: false,
        };
        insert_audit(
            transaction,
            tenant_id,
            actor_id,
            membership_id,
            device_id,
            session_id,
            "outbound_order.allocated",
            request.order_id,
            &request.request_id,
            json!({"order_line_id": request.order_line_id, "allocated_count": response.allocated_count, "allow_mixed_skus": request.allow_mixed_skus}),
            &now,
        )
        .await?;
        finish_idempotency(
            transaction,
            tenant_id,
            ALLOCATION_SCOPE,
            &request.idempotency_key,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn ship_outbound_order(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: NetworkShipOutboundRequest,
    ) -> NetworkResult<ShipOutboundResponse> {
        let request = normalize_ship(request)?;
        let digest = request_digest(&request)?;
        let warranty = resolve_warranty(request.warranty.clone(), &request.shipped_at)
            .map_err(NetworkServiceError::Invalid)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_SHIPMENT_WRITE)
            .await?;
        let identity = authorized.session();
        let actor_id = identity.identity.user_id;
        let membership_id = identity.identity.membership_id;
        let device_id = identity.device_id;
        let session_id = identity.session_id;
        let transaction = authorized.sqlx_transaction();
        if let Some(mut replay) = claim_idempotency::<ShipOutboundResponse>(
            transaction,
            tenant_id,
            SHIPMENT_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            authorized.commit().await?;
            return Ok(replay);
        }
        let rows = load_shipment_allocations(transaction, tenant_id, &request).await?;
        if rows.is_empty() {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_allocation".to_owned(),
                key: "none_selected".to_owned(),
            });
        }
        let now = UtcNow::value();
        let shipment_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO outbound_shipments
                (tenant_id, id, shipment_no, outbound_order_id, status,
                 shipped_at, actor_id, idempotency_key, request_id,
                 warranty_duration_days, warranty_label_snapshot,
                 warranty_started_at, warranty_expires_at)
            VALUES ($1, $2, $3, $4, 'posted', $5::timestamptz, $6, $7, $8,
                    $9, $10, $11::timestamptz, $12::timestamptz)
            "#,
        )
        .bind(tenant_id)
        .bind(shipment_id)
        .bind(&request.shipment_no)
        .bind(request.order_id)
        .bind(&request.shipped_at)
        .bind(actor_id)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .bind(warranty.as_ref().map(|terms| terms.duration_days as i32))
        .bind(warranty.as_ref().map(|terms| terms.label_snapshot.as_str()))
        .bind(warranty.as_ref().map(|terms| terms.starts_at.as_str()))
        .bind(warranty.as_ref().map(|terms| terms.expires_at.as_str()))
        .execute(&mut **transaction)
        .await
        .map_err(|error| conflict_or_sqlx("outbound_shipment", &request.shipment_no, error))?;

        let mut items = Vec::with_capacity(rows.len());
        let mut shipped_by_line: HashMap<Uuid, i64> = HashMap::new();
        for row in rows {
            if row.order_id != request.order_id
                || row.inventory_status != "reserved"
                || !matches!(row.quality_status.as_str(), "passed" | "waived")
            {
                return Err(NetworkServiceError::Conflict {
                    entity: "outbound_allocation".to_owned(),
                    key: row.allocation_id.to_string(),
                });
            }
            let shipment_line_id = Uuid::now_v7();
            let allocation_updated = sqlx::query(
                "UPDATE outbound_allocations SET status = 'shipped' WHERE tenant_id = $1 AND id = $2 AND status = 'active'",
            )
            .bind(tenant_id)
            .bind(row.allocation_id)
            .execute(&mut **transaction)
            .await?;
            if allocation_updated.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "outbound_allocation".to_owned(),
                    key: row.allocation_id.to_string(),
                });
            }
            let updated = sqlx::query(
                r#"
                UPDATE inventory_units
                   SET inventory_status = 'shipped', version = version + 1,
                       updated_at = CURRENT_TIMESTAMP
                 WHERE tenant_id = $1 AND id = $2 AND version = $3
                   AND inventory_status = 'reserved'
                "#,
            )
            .bind(tenant_id)
            .bind(row.unit_id)
            .bind(row.version)
            .execute(&mut **transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "inventory_unit_version".to_owned(),
                    key: row.barcode.clone(),
                });
            }
            sqlx::query(
                r#"
                INSERT INTO outbound_shipment_lines
                    (tenant_id, id, outbound_shipment_id, outbound_allocation_id,
                     inventory_unit_id, scanned_barcode_snapshot, status)
                VALUES ($1, $2, $3, $4, $5, $6, 'shipped')
                "#,
            )
            .bind(tenant_id)
            .bind(shipment_line_id)
            .bind(shipment_id)
            .bind(row.allocation_id)
            .bind(row.unit_id)
            .bind(&row.barcode)
            .execute(&mut **transaction)
            .await
            .map_err(|error| conflict_or_sqlx("outbound_shipment_line", &row.barcode, error))?;
            insert_movement(
                transaction,
                tenant_id,
                row.unit_id,
                "shipped",
                None,
                None,
                "outbound_shipment",
                shipment_id,
                actor_id,
                &request.shipped_at,
            )
            .await?;
            *shipped_by_line.entry(row.order_line_id).or_default() += 1;
            items.push(ShipmentItem {
                shipment_line_id: shipment_line_id.to_string(),
                allocation_id: row.allocation_id.to_string(),
                barcode: row.barcode,
                owner_party_id: row.owner_party_id.to_string(),
                sku_id: row.sku_id.to_string(),
            });
        }
        for (line_id, count) in shipped_by_line {
            let updated = sqlx::query(
                r#"
                UPDATE outbound_order_lines
                   SET shipped_quantity = shipped_quantity + $1
                 WHERE tenant_id = $2 AND id = $3
                   AND shipped_quantity + $1 <= allocated_quantity
                "#,
            )
            .bind(count as i32)
            .bind(tenant_id)
            .bind(line_id)
            .execute(&mut **transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "outbound_order_line".to_owned(),
                    key: line_id.to_string(),
                });
            }
        }
        let order_status =
            order_status_after_ship(transaction, tenant_id, request.order_id).await?;
        sqlx::query("UPDATE outbound_orders SET status = $1 WHERE tenant_id = $2 AND id = $3")
            .bind(&order_status)
            .bind(tenant_id)
            .bind(request.order_id)
            .execute(&mut **transaction)
            .await?;
        let response = ShipOutboundResponse {
            shipment_id: shipment_id.to_string(),
            shipment_no: request.shipment_no.clone(),
            shipped_count: items.len() as u32,
            order_status,
            items,
            idempotent_replay: false,
        };
        insert_audit(
            transaction,
            tenant_id,
            actor_id,
            membership_id,
            device_id,
            session_id,
            "outbound_shipment.posted",
            shipment_id,
            &request.request_id,
            json!({"shipment_no": request.shipment_no, "shipped_count": response.shipped_count}),
            &now,
        )
        .await?;
        finish_idempotency(
            transaction,
            tenant_id,
            SHIPMENT_SCOPE,
            &request.idempotency_key,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn confirm_outbound_delivery(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: NetworkConfirmOutboundDeliveryRequest,
    ) -> NetworkResult<ConfirmOutboundDeliveryResponse> {
        let request = normalize_delivery(request)?;
        let digest = request_digest(&request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_DELIVERY_WRITE)
            .await?;
        let identity = authorized.session();
        let actor_id = identity.identity.user_id;
        let membership_id = identity.identity.membership_id;
        let device_id = identity.device_id;
        let session_id = identity.session_id;
        let transaction = authorized.sqlx_transaction();
        if let Some(mut replay) = claim_idempotency::<ConfirmOutboundDeliveryResponse>(
            transaction,
            tenant_id,
            DELIVERY_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            authorized.commit().await?;
            return Ok(replay);
        }
        let shipment_order_id: Uuid = sqlx::query_scalar(
            "SELECT outbound_order_id FROM outbound_shipments WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(tenant_id)
        .bind(request.shipment_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| NetworkServiceError::Conflict {
            entity: "outbound_shipment".to_owned(),
            key: request.shipment_id.to_string(),
        })?;
        sqlx::query("SELECT id FROM outbound_orders WHERE tenant_id = $1 AND id = $2 FOR UPDATE")
            .bind(tenant_id)
            .bind(shipment_order_id)
            .fetch_one(&mut **transaction)
            .await?;
        let rows = load_delivery_lines(
            transaction,
            tenant_id,
            request.shipment_id,
            &request.shipment_line_ids,
        )
        .await?;
        if rows.is_empty() {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_shipment_line".to_owned(),
                key: "none_pending_delivery".to_owned(),
            });
        }
        let now = UtcNow::value();
        let confirmation_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO delivery_confirmations
                (tenant_id, id, outbound_shipment_id, confirmation_code,
                 confirmed_by, confirmed_at, notes, idempotency_key, request_id)
            VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7, $8, $9)
            "#,
        )
        .bind(tenant_id)
        .bind(confirmation_id)
        .bind(request.shipment_id)
        .bind(&request.confirmation_code)
        .bind(actor_id)
        .bind(&request.confirmed_at)
        .bind(&request.notes)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .execute(&mut **transaction)
        .await
        .map_err(|error| {
            conflict_or_sqlx("delivery_confirmation", &request.confirmation_code, error)
        })?;
        for row in &rows {
            sqlx::query(
                r#"
                INSERT INTO delivery_confirmation_lines
                    (tenant_id, id, delivery_confirmation_id,
                     outbound_shipment_line_id, result, exception_notes)
                VALUES ($1, $2, $3, $4, 'accepted', NULL)
                "#,
            )
            .bind(tenant_id)
            .bind(Uuid::now_v7())
            .bind(confirmation_id)
            .bind(row.shipment_line_id)
            .execute(&mut **transaction)
            .await
            .map_err(|error| {
                conflict_or_sqlx(
                    "delivery_confirmation_line",
                    &row.shipment_line_id.to_string(),
                    error,
                )
            })?;
            let updated = sqlx::query(
                r#"
                UPDATE inventory_units
                   SET inventory_status = 'delivered', version = version + 1,
                       updated_at = CURRENT_TIMESTAMP
                 WHERE tenant_id = $1 AND id = $2 AND version = $3
                   AND inventory_status = 'shipped'
                "#,
            )
            .bind(tenant_id)
            .bind(row.unit_id)
            .bind(row.version)
            .execute(&mut **transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "inventory_unit_version".to_owned(),
                    key: row.unit_id.to_string(),
                });
            }
            let line_updated = sqlx::query(
                r#"
                UPDATE outbound_order_lines
                   SET delivered_quantity = delivered_quantity + 1
                 WHERE tenant_id = $1 AND id = $2
                   AND delivered_quantity + 1 <= shipped_quantity
                "#,
            )
            .bind(tenant_id)
            .bind(row.order_line_id)
            .execute(&mut **transaction)
            .await?;
            if line_updated.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "outbound_order_line".to_owned(),
                    key: row.order_line_id.to_string(),
                });
            }
            insert_movement(
                transaction,
                tenant_id,
                row.unit_id,
                "delivered",
                None,
                None,
                "delivery_confirmation",
                confirmation_id,
                actor_id,
                &request.confirmed_at,
            )
            .await?;
        }
        let shipment_status =
            shipment_status_after_delivery(transaction, tenant_id, request.shipment_id).await?;
        sqlx::query("UPDATE outbound_shipments SET status = $1 WHERE tenant_id = $2 AND id = $3")
            .bind(&shipment_status)
            .bind(tenant_id)
            .bind(request.shipment_id)
            .execute(&mut **transaction)
            .await?;
        let order_status =
            order_status_after_delivery(transaction, tenant_id, shipment_order_id).await?;
        sqlx::query("UPDATE outbound_orders SET status = $1 WHERE tenant_id = $2 AND id = $3")
            .bind(&order_status)
            .bind(tenant_id)
            .bind(shipment_order_id)
            .execute(&mut **transaction)
            .await?;
        let response = ConfirmOutboundDeliveryResponse {
            confirmation_id: confirmation_id.to_string(),
            confirmation_code: request.confirmation_code.clone(),
            delivered_count: rows.len() as u32,
            shipment_status,
            idempotent_replay: false,
        };
        insert_audit(
            transaction,
            tenant_id,
            actor_id,
            membership_id,
            device_id,
            session_id,
            "delivery_confirmation.created",
            confirmation_id,
            &request.request_id,
            json!({"confirmation_code": request.confirmation_code, "delivered_count": response.delivered_count}),
            &now,
        )
        .await?;
        finish_idempotency(
            transaction,
            tenant_id,
            DELIVERY_SCOPE,
            &request.idempotency_key,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn return_outbound_shipment(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: NetworkReturnOutboundShipmentRequest,
    ) -> NetworkResult<ReturnOutboundShipmentResponse> {
        let request = normalize_return(request)?;
        let digest = request_digest(&request)?;
        let mut authorized = self
            .database()
            .begin_authorized_request(tenant_id, session_token, PERMISSION_RETURN_WRITE)
            .await?;
        let identity = authorized.session();
        let actor_id = identity.identity.user_id;
        let membership_id = identity.identity.membership_id;
        let device_id = identity.device_id;
        let session_id = identity.session_id;
        let transaction = authorized.sqlx_transaction();
        if let Some(mut replay) = claim_idempotency::<ReturnOutboundShipmentResponse>(
            transaction,
            tenant_id,
            RETURN_SCOPE,
            &request.idempotency_key,
            &digest,
        )
        .await?
        {
            replay.idempotent_replay = true;
            authorized.commit().await?;
            return Ok(replay);
        }
        let quarantine_location = location_for_kind(transaction, tenant_id, "quarantine").await?;
        let rows = load_return_lines(
            transaction,
            tenant_id,
            request.shipment_id,
            &request.shipment_line_ids,
        )
        .await?;
        if rows.is_empty() {
            return Err(NetworkServiceError::Conflict {
                entity: "outbound_shipment_line".to_owned(),
                key: "none_returnable".to_owned(),
            });
        }
        let now = UtcNow::value();
        let batch_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO outbound_return_batches
                (tenant_id, id, return_no, returned_at, actor_id,
                 idempotency_key, request_id)
            VALUES ($1, $2, $3, $4::timestamptz, $5, $6, $7)
            "#,
        )
        .bind(tenant_id)
        .bind(batch_id)
        .bind(&request.return_no)
        .bind(&request.returned_at)
        .bind(actor_id)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .execute(&mut **transaction)
        .await
        .map_err(|error| conflict_or_sqlx("outbound_return_batch", &request.return_no, error))?;
        for row in &rows {
            sqlx::query(
                r#"
                INSERT INTO outbound_return_lines
                    (tenant_id, id, return_batch_id, outbound_shipment_line_id,
                     inventory_unit_id, reason, disposition)
                VALUES ($1, $2, $3, $4, $5, $6, 'quarantine')
                "#,
            )
            .bind(tenant_id)
            .bind(Uuid::now_v7())
            .bind(batch_id)
            .bind(row.shipment_line_id)
            .bind(row.unit_id)
            .bind(&request.reason)
            .execute(&mut **transaction)
            .await
            .map_err(|error| {
                conflict_or_sqlx(
                    "outbound_return_line",
                    &row.shipment_line_id.to_string(),
                    error,
                )
            })?;
            let allocation_released = sqlx::query(
                "UPDATE outbound_allocations SET status = 'released', released_at = CURRENT_TIMESTAMP WHERE tenant_id = $1 AND id = $2 AND status = 'shipped'",
            )
            .bind(tenant_id)
            .bind(row.allocation_id)
            .execute(&mut **transaction)
            .await?;
            if allocation_released.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "outbound_allocation".to_owned(),
                    key: row.allocation_id.to_string(),
                });
            }
            let updated = sqlx::query(
                r#"
                UPDATE inventory_units
                   SET inventory_status = 'quarantined', location_id = $1,
                       version = version + 1, updated_at = CURRENT_TIMESTAMP
                 WHERE tenant_id = $2 AND id = $3 AND version = $4
                   AND inventory_status IN ('shipped', 'delivered')
                "#,
            )
            .bind(quarantine_location)
            .bind(tenant_id)
            .bind(row.unit_id)
            .bind(row.version)
            .execute(&mut **transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(NetworkServiceError::Conflict {
                    entity: "inventory_unit_version".to_owned(),
                    key: row.unit_id.to_string(),
                });
            }
            insert_movement(
                transaction,
                tenant_id,
                row.unit_id,
                "returned",
                None,
                Some(quarantine_location),
                "outbound_return_batch",
                batch_id,
                actor_id,
                &request.returned_at,
            )
            .await?;
        }
        let response = ReturnOutboundShipmentResponse {
            return_batch_id: batch_id.to_string(),
            return_no: request.return_no.clone(),
            quarantined_count: rows.len() as u32,
            idempotent_replay: false,
        };
        insert_audit(
            transaction,
            tenant_id,
            actor_id,
            membership_id,
            device_id,
            session_id,
            "outbound_return.created",
            batch_id,
            &request.request_id,
            json!({"return_no": request.return_no, "quarantined_count": response.quarantined_count}),
            &now,
        )
        .await?;
        finish_idempotency(
            transaction,
            tenant_id,
            RETURN_SCOPE,
            &request.idempotency_key,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }
}

#[derive(Debug, Clone)]
struct InspectionRow {
    id: Uuid,
    barcode: String,
    old_location: Uuid,
    old_version: i64,
    result: NetworkInspectionResultInput,
    quality_label_id: Option<Uuid>,
    quality_label_snapshot: Option<String>,
}

#[derive(Debug, Clone)]
struct AllocationCandidate {
    id: Uuid,
    barcode: String,
    owner_party_id: Uuid,
    sku_id: Uuid,
    location_id: Uuid,
    version: i64,
    inventory_status: String,
    quality_status: String,
}

#[derive(Debug, Clone)]
struct ShipmentAllocation {
    allocation_id: Uuid,
    order_line_id: Uuid,
    order_id: Uuid,
    unit_id: Uuid,
    barcode: String,
    owner_party_id: Uuid,
    sku_id: Uuid,
    version: i64,
    inventory_status: String,
    quality_status: String,
}

#[derive(Debug, Clone)]
struct ShipmentLine {
    shipment_line_id: Uuid,
    allocation_id: Uuid,
    order_line_id: Uuid,
    unit_id: Uuid,
    version: i64,
}

fn empty_json_object() -> Value {
    json!({})
}

fn inspection_transition_allowed(kind: InspectionKind, inventory: &str, quality: &str) -> bool {
    match kind {
        InspectionKind::Initial => inventory == "received" && quality == "untested",
        InspectionKind::Retest => {
            inventory == "quarantined" && matches!(quality, "failed" | "passed" | "waived")
        }
    }
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

fn parse_inventory_status(value: &str) -> NetworkResult<InventoryStatus> {
    match value {
        "received" => Ok(InventoryStatus::Received),
        "available" => Ok(InventoryStatus::Available),
        "reserved" => Ok(InventoryStatus::Reserved),
        "shipped" => Ok(InventoryStatus::Shipped),
        "delivered" => Ok(InventoryStatus::Delivered),
        "quarantined" => Ok(InventoryStatus::Quarantined),
        "scrapped" => Ok(InventoryStatus::Scrapped),
        "returned_to_owner" => Ok(InventoryStatus::ReturnedToOwner),
        "voided" => Ok(InventoryStatus::Voided),
        other => Err(NetworkServiceError::Invalid(format!(
            "unknown inventory status {other}"
        ))),
    }
}

fn parse_quality_status(value: &str) -> NetworkResult<QualityStatus> {
    match value {
        "untested" => Ok(QualityStatus::Untested),
        "testing" => Ok(QualityStatus::Testing),
        "passed" => Ok(QualityStatus::Passed),
        "failed" => Ok(QualityStatus::Failed),
        "waived" => Ok(QualityStatus::Waived),
        other => Err(NetworkServiceError::Invalid(format!(
            "unknown quality status {other}"
        ))),
    }
}

fn required(field: &str, value: String) -> NetworkResult<String> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        Err(NetworkServiceError::Invalid(format!(
            "{field} must not be empty"
        )))
    } else {
        Ok(value)
    }
}

fn optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_owned();
        (!value.is_empty()).then_some(value)
    })
}

fn normalized_name(field: &str, value: String) -> NetworkResult<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    required(field, value)
}

fn normalize_inspection(
    mut request: NetworkCompleteInspectionRequest,
) -> NetworkResult<NetworkCompleteInspectionRequest> {
    request.request_id = required("request_id", request.request_id)?;
    request.idempotency_key = required("idempotency_key", request.idempotency_key)?;
    request.inspection_no = required("inspection_no", request.inspection_no)?.to_uppercase();
    request.inspected_at = required("inspected_at", request.inspected_at)?;
    if request.results.is_empty() {
        return Err(NetworkServiceError::Invalid(
            "results must not be empty".to_owned(),
        ));
    }
    let mut seen = HashSet::with_capacity(request.results.len());
    for result in &mut request.results {
        result.barcode = required("barcode", std::mem::take(&mut result.barcode))?.to_uppercase();
        result.quality_label_id = optional(result.quality_label_id.take());
        if let Some(quality_label_id) = &result.quality_label_id {
            Uuid::parse_str(quality_label_id).map_err(|_| {
                NetworkServiceError::Invalid("quality_label_id must be a UUID".to_owned())
            })?;
        }
        result.defect_code = optional(result.defect_code.take());
        result.notes = optional(result.notes.take());
        if !result.measurements.is_object() {
            return Err(NetworkServiceError::Invalid(
                "measurements must be a JSON object".to_owned(),
            ));
        }
        if !seen.insert(result.barcode.clone()) {
            return Err(NetworkServiceError::Conflict {
                entity: "inspection_barcode".to_owned(),
                key: result.barcode.clone(),
            });
        }
    }
    Ok(request)
}

fn normalize_create_order(
    mut request: NetworkCreateOutboundOrderRequest,
) -> NetworkResult<NetworkCreateOutboundOrderRequest> {
    request.request_id = required("request_id", request.request_id)?;
    request.idempotency_key = required("idempotency_key", request.idempotency_key)?;
    request.order_no = required("order_no", request.order_no)?.to_uppercase();
    request.upstream_receiver_name =
        normalized_name("upstream_receiver_name", request.upstream_receiver_name)?;
    request.sku_code = required("sku_code", request.sku_code)?.to_uppercase();
    request.sku_name = normalized_name("sku_name", request.sku_name)?;
    request.required_at = optional(request.required_at);
    if request.required_quantity == 0 {
        return Err(NetworkServiceError::Invalid(
            "required_quantity must be greater than zero".to_owned(),
        ));
    }
    if request.required_quantity > i32::MAX as u32 {
        return Err(NetworkServiceError::Invalid(
            "required_quantity exceeds PostgreSQL integer range".to_owned(),
        ));
    }
    Ok(request)
}

fn normalize_allocate(
    mut request: NetworkAllocateOutboundRequest,
) -> NetworkResult<NetworkAllocateOutboundRequest> {
    request.request_id = required("request_id", request.request_id)?;
    request.idempotency_key = required("idempotency_key", request.idempotency_key)?;
    for barcode in &mut request.barcodes {
        *barcode = required("barcode", std::mem::take(barcode))?.to_uppercase();
    }
    let mut seen = HashSet::new();
    for barcode in &request.barcodes {
        if !seen.insert(barcode) {
            return Err(NetworkServiceError::Conflict {
                entity: "allocation_barcode".to_owned(),
                key: barcode.clone(),
            });
        }
    }
    Ok(request)
}

fn normalize_ship(
    mut request: NetworkShipOutboundRequest,
) -> NetworkResult<NetworkShipOutboundRequest> {
    request.request_id = required("request_id", request.request_id)?;
    request.idempotency_key = required("idempotency_key", request.idempotency_key)?;
    request.shipment_no = required("shipment_no", request.shipment_no)?.to_uppercase();
    request.shipped_at = required("shipped_at", request.shipped_at)?;
    for barcode in &mut request.barcodes {
        *barcode = required("barcode", std::mem::take(barcode))?.to_uppercase();
    }
    let mut seen = HashSet::new();
    for allocation_id in &request.allocation_ids {
        if !seen.insert(*allocation_id) {
            return Err(NetworkServiceError::Conflict {
                entity: "allocation_id".to_owned(),
                key: allocation_id.to_string(),
            });
        }
    }
    let mut seen_barcodes = HashSet::new();
    for barcode in &request.barcodes {
        if !seen_barcodes.insert(barcode) {
            return Err(NetworkServiceError::Conflict {
                entity: "shipment_barcode".to_owned(),
                key: barcode.clone(),
            });
        }
    }
    if request.allocation_ids.is_empty() && request.barcodes.is_empty() {
        return Err(NetworkServiceError::Invalid(
            "allocation_ids or barcodes selector must not be empty".to_owned(),
        ));
    }
    if !request.allocation_ids.is_empty() && !request.barcodes.is_empty() {
        return Err(NetworkServiceError::Invalid(
            "allocation_ids and barcodes cannot both be supplied".to_owned(),
        ));
    }
    Ok(request)
}

fn normalize_delivery(
    mut request: NetworkConfirmOutboundDeliveryRequest,
) -> NetworkResult<NetworkConfirmOutboundDeliveryRequest> {
    request.request_id = required("request_id", request.request_id)?;
    request.idempotency_key = required("idempotency_key", request.idempotency_key)?;
    request.confirmation_code = required("confirmation_code", request.confirmation_code)?;
    request.confirmed_at = required("confirmed_at", request.confirmed_at)?;
    request.notes = optional(request.notes);
    Ok(request)
}

fn normalize_return(
    mut request: NetworkReturnOutboundShipmentRequest,
) -> NetworkResult<NetworkReturnOutboundShipmentRequest> {
    request.request_id = required("request_id", request.request_id)?;
    request.idempotency_key = required("idempotency_key", request.idempotency_key)?;
    request.return_no = required("return_no", request.return_no)?.to_uppercase();
    request.returned_at = required("returned_at", request.returned_at)?;
    request.reason = required("reason", request.reason)?;
    Ok(request)
}

async fn location_for_kind(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    kind: &str,
) -> NetworkResult<Uuid> {
    sqlx::query_scalar(
        "SELECT id FROM locations WHERE tenant_id = $1 AND kind = $2 ORDER BY id LIMIT 1",
    )
    .bind(tenant_id)
    .bind(kind)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| NetworkServiceError::Invalid(format!("tenant has no {kind} location")))
}

async fn upsert_party(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    display_name: &str,
    role: &str,
) -> NetworkResult<Uuid> {
    let normalized = display_name.to_lowercase();
    let id = sqlx::query_scalar(
        r#"
        INSERT INTO business_parties (tenant_id, id, normalized_name, display_name)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (tenant_id, normalized_name) DO UPDATE
            SET display_name = EXCLUDED.display_name
        RETURNING id
        "#,
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .bind(normalized)
    .bind(display_name)
    .fetch_one(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO party_roles (tenant_id, party_id, role) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(role)
    .execute(&mut **transaction)
    .await?;
    Ok(id)
}

async fn upsert_sku(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    code: &str,
    name: &str,
) -> NetworkResult<Uuid> {
    Ok(sqlx::query_scalar(
        r#"
        INSERT INTO skus (tenant_id, id, code, name, tracking_mode, active)
        VALUES ($1, $2, $3, $4, 'serial', true)
        ON CONFLICT (tenant_id, code) DO UPDATE SET name = EXCLUDED.name
        RETURNING id
        "#,
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .bind(code)
    .bind(name)
    .fetch_one(&mut **transaction)
    .await?)
}

async fn load_allocation_candidates(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    sku_id: Uuid,
    remaining: i64,
    barcodes: &[String],
) -> NetworkResult<Vec<AllocationCandidate>> {
    if barcodes.is_empty() {
        let rows = sqlx::query(
            r#"
            SELECT id, barcode, owner_party_id, sku_id, location_id, version,
                   inventory_status, quality_status
              FROM inventory_units
             WHERE tenant_id = $1 AND sku_id = $2
               AND inventory_status = 'available'
               AND quality_status IN ('passed', 'waived')
             ORDER BY received_at, id
             LIMIT $3
             FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(tenant_id)
        .bind(sku_id)
        .bind(remaining)
        .fetch_all(&mut **transaction)
        .await?;
        rows.into_iter().map(candidate_from_row).collect()
    } else {
        let mut result = Vec::with_capacity(barcodes.len());
        for barcode in barcodes {
            let row = sqlx::query(
                r#"
                SELECT id, barcode, owner_party_id, sku_id, location_id, version,
                       inventory_status, quality_status
                  FROM inventory_units
                 WHERE tenant_id = $1 AND barcode = $2
                 FOR UPDATE
                "#,
            )
            .bind(tenant_id)
            .bind(barcode)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or_else(|| NetworkServiceError::Conflict {
                entity: "inventory_barcode".to_owned(),
                key: barcode.clone(),
            })?;
            result.push(candidate_from_row(row)?);
        }
        Ok(result)
    }
}

fn candidate_from_row(row: sqlx::postgres::PgRow) -> NetworkResult<AllocationCandidate> {
    Ok(AllocationCandidate {
        id: row.try_get("id")?,
        barcode: row.try_get("barcode")?,
        owner_party_id: row.try_get("owner_party_id")?,
        sku_id: row.try_get("sku_id")?,
        location_id: row.try_get("location_id")?,
        version: row.try_get("version")?,
        inventory_status: row.try_get("inventory_status")?,
        quality_status: row.try_get("quality_status")?,
    })
}

async fn load_shipment_allocations(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    request: &NetworkShipOutboundRequest,
) -> NetworkResult<Vec<ShipmentAllocation>> {
    let mut rows = Vec::new();
    if !request.allocation_ids.is_empty() {
        for allocation_id in &request.allocation_ids {
            rows.push(
                sqlx::query(
                    r#"
                    SELECT oa.id AS allocation_id, l.id AS order_line_id,
                           o.id AS order_id, iu.id AS unit_id, iu.barcode,
                           iu.owner_party_id, iu.sku_id, iu.version,
                           iu.inventory_status, iu.quality_status
                      FROM outbound_allocations oa
                      JOIN outbound_order_lines l
                        ON l.tenant_id = oa.tenant_id
                       AND l.id = oa.outbound_order_line_id
                      JOIN outbound_orders o
                        ON o.tenant_id = l.tenant_id
                       AND o.id = l.outbound_order_id
                      JOIN inventory_units iu
                        ON iu.tenant_id = oa.tenant_id
                       AND iu.id = oa.inventory_unit_id
                     WHERE oa.tenant_id = $1 AND oa.id = $2
                       AND oa.status = 'active'
                     FOR UPDATE OF oa, l, o, iu
                    "#,
                )
                .bind(tenant_id)
                .bind(*allocation_id)
                .fetch_optional(&mut **transaction)
                .await?
                .ok_or_else(|| NetworkServiceError::Conflict {
                    entity: "outbound_allocation".to_owned(),
                    key: allocation_id.to_string(),
                })?,
            );
        }
    } else {
        let mut seen = HashSet::new();
        for barcode in &request.barcodes {
            if !seen.insert(barcode) {
                return Err(NetworkServiceError::Conflict {
                    entity: "shipment_barcode".to_owned(),
                    key: barcode.clone(),
                });
            }
            rows.push(
                sqlx::query(
                    r#"
                    SELECT oa.id AS allocation_id, l.id AS order_line_id,
                           o.id AS order_id, iu.id AS unit_id, iu.barcode,
                           iu.owner_party_id, iu.sku_id, iu.version,
                           iu.inventory_status, iu.quality_status
                      FROM outbound_allocations oa
                      JOIN outbound_order_lines l
                        ON l.tenant_id = oa.tenant_id
                       AND l.id = oa.outbound_order_line_id
                      JOIN outbound_orders o
                        ON o.tenant_id = l.tenant_id
                       AND o.id = l.outbound_order_id
                      JOIN inventory_units iu
                        ON iu.tenant_id = oa.tenant_id
                       AND iu.id = oa.inventory_unit_id
                     WHERE oa.tenant_id = $1 AND iu.barcode = $2
                       AND oa.status = 'active'
                     FOR UPDATE OF oa, l, o, iu
                    "#,
                )
                .bind(tenant_id)
                .bind(barcode)
                .fetch_optional(&mut **transaction)
                .await?
                .ok_or_else(|| NetworkServiceError::Conflict {
                    entity: "inventory_barcode".to_owned(),
                    key: barcode.clone(),
                })?,
            );
        }
    }
    rows.into_iter()
        .map(|row| {
            Ok(ShipmentAllocation {
                allocation_id: row.try_get("allocation_id")?,
                order_line_id: row.try_get("order_line_id")?,
                order_id: row.try_get("order_id")?,
                unit_id: row.try_get("unit_id")?,
                barcode: row.try_get("barcode")?,
                owner_party_id: row.try_get("owner_party_id")?,
                sku_id: row.try_get("sku_id")?,
                version: row.try_get("version")?,
                inventory_status: row.try_get("inventory_status")?,
                quality_status: row.try_get("quality_status")?,
            })
        })
        .collect()
}

async fn load_delivery_lines(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    shipment_id: Uuid,
    selected: &[Uuid],
) -> NetworkResult<Vec<ShipmentLine>> {
    let mut rows = Vec::new();
    if selected.is_empty() {
        rows = sqlx::query(
            r#"
            SELECT osl.id AS shipment_line_id,
                   osl.outbound_allocation_id AS allocation_id,
                   osl.inventory_unit_id AS unit_id,
                   oa.outbound_order_line_id AS order_line_id, iu.version
              FROM outbound_shipment_lines osl
              JOIN outbound_allocations oa
                ON oa.tenant_id = osl.tenant_id
               AND oa.id = osl.outbound_allocation_id
              JOIN inventory_units iu
                ON iu.tenant_id = osl.tenant_id
               AND iu.id = osl.inventory_unit_id
             WHERE osl.tenant_id = $1 AND osl.outbound_shipment_id = $2
               AND osl.status = 'shipped' AND iu.inventory_status = 'shipped'
               AND NOT EXISTS (
                   SELECT 1 FROM delivery_confirmation_lines dcl
                    WHERE dcl.tenant_id = osl.tenant_id
                      AND dcl.outbound_shipment_line_id = osl.id
               )
             FOR UPDATE OF osl, oa, iu
            "#,
        )
        .bind(tenant_id)
        .bind(shipment_id)
        .fetch_all(&mut **transaction)
        .await?;
    } else {
        let mut seen = HashSet::new();
        for line_id in selected {
            if !seen.insert(*line_id) {
                return Err(NetworkServiceError::Conflict {
                    entity: "shipment_line".to_owned(),
                    key: line_id.to_string(),
                });
            }
            rows.push(
                sqlx::query(
                    r#"
                    SELECT osl.id AS shipment_line_id,
                           osl.outbound_allocation_id AS allocation_id,
                           osl.inventory_unit_id AS unit_id,
                           oa.outbound_order_line_id AS order_line_id, iu.version
                      FROM outbound_shipment_lines osl
                      JOIN outbound_allocations oa
                        ON oa.tenant_id = osl.tenant_id
                       AND oa.id = osl.outbound_allocation_id
                      JOIN inventory_units iu
                        ON iu.tenant_id = osl.tenant_id
                       AND iu.id = osl.inventory_unit_id
                     WHERE osl.tenant_id = $1 AND osl.outbound_shipment_id = $2
                       AND osl.id = $3 AND osl.status = 'shipped'
                       AND iu.inventory_status = 'shipped'
                       AND NOT EXISTS (
                           SELECT 1 FROM delivery_confirmation_lines dcl
                            WHERE dcl.tenant_id = osl.tenant_id
                              AND dcl.outbound_shipment_line_id = osl.id
                       )
                     FOR UPDATE OF osl, oa, iu
                    "#,
                )
                .bind(tenant_id)
                .bind(shipment_id)
                .bind(*line_id)
                .fetch_optional(&mut **transaction)
                .await?
                .ok_or_else(|| NetworkServiceError::Conflict {
                    entity: "shipment_line".to_owned(),
                    key: line_id.to_string(),
                })?,
            );
        }
    }
    rows.into_iter()
        .map(|row| {
            Ok(ShipmentLine {
                shipment_line_id: row.try_get("shipment_line_id")?,
                allocation_id: row.try_get("allocation_id")?,
                order_line_id: row.try_get("order_line_id")?,
                unit_id: row.try_get("unit_id")?,
                version: row.try_get("version")?,
            })
        })
        .collect()
}

async fn load_return_lines(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    shipment_id: Uuid,
    selected: &[Uuid],
) -> NetworkResult<Vec<ShipmentLine>> {
    let mut rows = Vec::new();
    if selected.is_empty() {
        rows = sqlx::query(
            r#"
            SELECT osl.id AS shipment_line_id,
                   osl.outbound_allocation_id AS allocation_id,
                   osl.inventory_unit_id AS unit_id,
                   oa.outbound_order_line_id AS order_line_id, iu.version
              FROM outbound_shipment_lines osl
              JOIN outbound_allocations oa
                ON oa.tenant_id = osl.tenant_id
               AND oa.id = osl.outbound_allocation_id
              JOIN inventory_units iu
                ON iu.tenant_id = osl.tenant_id
               AND iu.id = osl.inventory_unit_id
             WHERE osl.tenant_id = $1 AND osl.outbound_shipment_id = $2
               AND osl.status IN ('shipped', 'delivered')
               AND iu.inventory_status IN ('shipped', 'delivered')
               AND NOT EXISTS (
                   SELECT 1 FROM outbound_return_lines rl
                    WHERE rl.tenant_id = osl.tenant_id
                      AND rl.outbound_shipment_line_id = osl.id
               )
             FOR UPDATE OF osl, oa, iu
            "#,
        )
        .bind(tenant_id)
        .bind(shipment_id)
        .fetch_all(&mut **transaction)
        .await?;
    } else {
        let mut seen = HashSet::new();
        for line_id in selected {
            if !seen.insert(*line_id) {
                return Err(NetworkServiceError::Conflict {
                    entity: "shipment_line".to_owned(),
                    key: line_id.to_string(),
                });
            }
            rows.push(
                sqlx::query(
                    r#"
                    SELECT osl.id AS shipment_line_id,
                           osl.outbound_allocation_id AS allocation_id,
                           osl.inventory_unit_id AS unit_id,
                           oa.outbound_order_line_id AS order_line_id, iu.version
                      FROM outbound_shipment_lines osl
                      JOIN outbound_allocations oa
                        ON oa.tenant_id = osl.tenant_id
                       AND oa.id = osl.outbound_allocation_id
                      JOIN inventory_units iu
                        ON iu.tenant_id = osl.tenant_id
                       AND iu.id = osl.inventory_unit_id
                     WHERE osl.tenant_id = $1 AND osl.outbound_shipment_id = $2
                       AND osl.id = $3 AND osl.status IN ('shipped', 'delivered')
                       AND iu.inventory_status IN ('shipped', 'delivered')
                       AND NOT EXISTS (
                           SELECT 1 FROM outbound_return_lines rl
                            WHERE rl.tenant_id = osl.tenant_id
                              AND rl.outbound_shipment_line_id = osl.id
                       )
                     FOR UPDATE OF osl, oa, iu
                    "#,
                )
                .bind(tenant_id)
                .bind(shipment_id)
                .bind(*line_id)
                .fetch_optional(&mut **transaction)
                .await?
                .ok_or_else(|| NetworkServiceError::Conflict {
                    entity: "shipment_line".to_owned(),
                    key: line_id.to_string(),
                })?,
            );
        }
    }
    rows.into_iter()
        .map(|row| {
            Ok(ShipmentLine {
                shipment_line_id: row.try_get("shipment_line_id")?,
                allocation_id: row.try_get("allocation_id")?,
                order_line_id: row.try_get("order_line_id")?,
                unit_id: row.try_get("unit_id")?,
                version: row.try_get("version")?,
            })
        })
        .collect()
}

async fn order_status_after_ship(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    order_id: Uuid,
) -> NetworkResult<String> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS total,
               COUNT(*) FILTER (WHERE shipped_quantity >= required_quantity) AS shipped
          FROM outbound_order_lines
         WHERE tenant_id = $1 AND outbound_order_id = $2
        "#,
    )
    .bind(tenant_id)
    .bind(order_id)
    .fetch_one(&mut **transaction)
    .await?;
    let total: i64 = row.try_get("total")?;
    let shipped: i64 = row.try_get("shipped")?;
    Ok(if total > 0 && shipped >= total {
        "shipped"
    } else {
        "partially_shipped"
    }
    .to_owned())
}

async fn shipment_status_after_delivery(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    shipment_id: Uuid,
) -> NetworkResult<String> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS total,
               COUNT(*) FILTER (WHERE status = 'delivered') AS delivered
          FROM outbound_shipment_lines
         WHERE tenant_id = $1 AND outbound_shipment_id = $2
        "#,
    )
    .bind(tenant_id)
    .bind(shipment_id)
    .fetch_one(&mut **transaction)
    .await?;
    let total: i64 = row.try_get("total")?;
    let delivered: i64 = row.try_get("delivered")?;
    Ok(if total > 0 && delivered >= total {
        "delivered"
    } else {
        "partially_delivered"
    }
    .to_owned())
}

async fn order_status_after_delivery(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    order_id: Uuid,
) -> NetworkResult<String> {
    let row = sqlx::query(
        r#"
        SELECT o.status,
               COUNT(l.id) AS total,
               COUNT(*) FILTER (WHERE l.delivered_quantity >= l.required_quantity) AS delivered
          FROM outbound_orders o
          JOIN outbound_order_lines l
            ON l.tenant_id = o.tenant_id AND l.outbound_order_id = o.id
         WHERE o.tenant_id = $1 AND o.id = $2
         GROUP BY o.status
        "#,
    )
    .bind(tenant_id)
    .bind(order_id)
    .fetch_one(&mut **transaction)
    .await?;
    let status: String = row.try_get("status")?;
    let total: i64 = row.try_get("total")?;
    let delivered: i64 = row.try_get("delivered")?;
    Ok(if total > 0 && delivered >= total {
        "completed".to_owned()
    } else {
        status
    })
}

#[allow(clippy::too_many_arguments)]
async fn insert_movement(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    inventory_unit_id: Uuid,
    movement_type: &str,
    from_location_id: Option<Uuid>,
    to_location_id: Option<Uuid>,
    source_type: &str,
    source_id: Uuid,
    actor_id: Uuid,
    occurred_at: &str,
) -> NetworkResult<()> {
    sqlx::query(
        r#"
        INSERT INTO stock_movements
            (tenant_id, id, inventory_unit_id, movement_type,
             from_location_id, to_location_id, source_type, source_id,
             actor_id, occurred_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::timestamptz)
        "#,
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .bind(inventory_unit_id)
    .bind(movement_type)
    .bind(from_location_id)
    .bind(to_location_id)
    .bind(source_type)
    .bind(source_id)
    .bind(actor_id)
    .bind(occurred_at)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_audit(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor_id: Uuid,
    membership_id: Uuid,
    device_id: Uuid,
    session_id: Uuid,
    action: &str,
    entity_id: Uuid,
    request_id: &str,
    details: Value,
    occurred_at: &str,
) -> NetworkResult<()> {
    sqlx::query(
        r#"
        INSERT INTO audit_logs
            (tenant_id, id, actor_id, membership_id, device_id, session_id,
             action, entity_type, entity_id, request_id, result, details_json,
             occurred_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'success', $11::jsonb,
                $12::timestamptz)
        "#,
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .bind(actor_id)
    .bind(membership_id)
    .bind(device_id)
    .bind(session_id)
    .bind(action)
    .bind(entity_type_for_action(action))
    .bind(entity_id)
    .bind(request_id)
    .bind(serde_json::to_string(&details)?)
    .bind(occurred_at)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn entity_type_for_action(action: &str) -> &'static str {
    if action.starts_with("quality_") {
        "quality_inspection"
    } else if action.starts_with("outbound_order") {
        "outbound_order"
    } else if action.starts_with("outbound_shipment") {
        "outbound_shipment"
    } else if action.starts_with("delivery_") {
        "delivery_confirmation"
    } else {
        "outbound_return_batch"
    }
}

async fn claim_idempotency<T: DeserializeOwned>(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    scope: &str,
    key: &str,
    digest: &str,
) -> NetworkResult<Option<T>> {
    let inserted = sqlx::query(
        r#"
        INSERT INTO idempotency_records
            (tenant_id, id, scope, idempotency_key, request_hash, response_json)
        VALUES ($1, $2, $3, $4, $5, '{"state":"in_progress"}'::jsonb)
        ON CONFLICT (tenant_id, scope, idempotency_key) DO NOTHING
        "#,
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .bind(scope)
    .bind(key)
    .bind(digest)
    .execute(&mut **transaction)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT request_hash, response_json::text AS response_json FROM idempotency_records WHERE tenant_id = $1 AND scope = $2 AND idempotency_key = $3 FOR UPDATE",
    )
    .bind(tenant_id)
    .bind(scope)
    .bind(key)
    .fetch_one(&mut **transaction)
    .await?;
    let stored: String = row.try_get("request_hash")?;
    if stored != digest {
        return Err(NetworkServiceError::Conflict {
            entity: "idempotency_key".to_owned(),
            key: key.to_owned(),
        });
    }
    let response_json: String = row.try_get("response_json")?;
    let response: T = serde_json::from_str(&response_json)?;
    Ok(Some(response))
}

async fn finish_idempotency<T: Serialize>(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    scope: &str,
    key: &str,
    response: &T,
) -> NetworkResult<()> {
    let updated = sqlx::query(
        "UPDATE idempotency_records SET response_json = $4::jsonb WHERE tenant_id = $1 AND scope = $2 AND idempotency_key = $3",
    )
    .bind(tenant_id)
    .bind(scope)
    .bind(key)
    .bind(serde_json::to_string(response)?)
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(NetworkServiceError::Invalid(
            "idempotency record disappeared".to_owned(),
        ));
    }
    Ok(())
}

fn request_digest<T: Serialize>(request: &T) -> NetworkResult<String> {
    let bytes = serde_json::to_vec(request)?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn conflict_or_sqlx(entity: &str, key: &str, error: sqlx::Error) -> NetworkServiceError {
    if error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| code == "23505")
    {
        NetworkServiceError::Conflict {
            entity: entity.to_owned(),
            key: key.to_owned(),
        }
    } else {
        NetworkServiceError::Sqlx(error)
    }
}

struct UtcNow;

impl UtcNow {
    fn value() -> String {
        // RFC3339 UTC without adding another time dependency. The SQL server
        // supplies the authoritative clock for persisted defaults; this value
        // is used only for audit/movement event timestamps supplied by a
        // request or for a create-order audit without a due date.
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::application::{
        CatalogPartyRole, CreateCatalogPartyRequest, CreateCatalogProductRequest,
    };
    use crate::v2::auth::PasswordService;
    use crate::v2::network::{
        LoginRequest, NetworkPostReceiptRequest, PERMISSION_NETWORK_ACCESS,
        PERMISSION_RECEIPT_WRITE,
    };
    use crate::v2::postgres::{NetworkDatabase, NetworkDatabaseConfig};
    use sqlx::postgres::PgPoolOptions;

    #[test]
    fn network_requests_do_not_require_actor_fields() {
        let request: NetworkCreateOutboundOrderRequest = serde_json::from_value(json!({
            "request_id": "req",
            "idempotency_key": "idem",
            "order_no": "o-1",
            "upstream_receiver_name": "Upstream",
            "sku_code": "sku-1",
            "sku_name": "Model",
            "required_quantity": 2
        }))
        .expect("request should deserialize without actor");
        assert_eq!(request.required_quantity, 2);
    }

    #[test]
    fn quality_transition_rejects_untested_retest_and_allows_return_retest() {
        assert!(!inspection_transition_allowed(
            InspectionKind::Retest,
            "received",
            "untested"
        ));
        assert!(inspection_transition_allowed(
            InspectionKind::Retest,
            "quarantined",
            "failed"
        ));
    }

    #[test]
    fn normalize_inspection_rejects_duplicate_serials() {
        let error = normalize_inspection(NetworkCompleteInspectionRequest {
            request_id: "req".to_owned(),
            idempotency_key: "idem".to_owned(),
            inspection_no: "i-1".to_owned(),
            inspection_kind: InspectionKind::Initial,
            inspected_at: "2026-08-03T01:00:00Z".to_owned(),
            results: vec![
                NetworkInspectionResultInput {
                    barcode: "sn-1".to_owned(),
                    outcome: QualityOutcome::Passed,
                    quality_label_id: None,
                    defect_code: None,
                    measurements: json!({}),
                    notes: None,
                },
                NetworkInspectionResultInput {
                    barcode: "SN-1".to_owned(),
                    outcome: QualityOutcome::Passed,
                    quality_label_id: None,
                    defect_code: None,
                    measurements: json!({}),
                    notes: None,
                },
            ],
        })
        .expect_err("duplicate serials must be rejected");
        assert!(
            matches!(error, NetworkServiceError::Conflict { entity, .. } if entity == "inspection_barcode")
        );
    }

    #[test]
    fn normalize_ship_requires_a_selector() {
        let error = normalize_ship(NetworkShipOutboundRequest {
            request_id: "req".to_owned(),
            idempotency_key: "idem".to_owned(),
            order_id: Uuid::now_v7(),
            shipment_no: "ship".to_owned(),
            allocation_ids: Vec::new(),
            barcodes: Vec::new(),
            shipped_at: "2026-08-03T01:00:00Z".to_owned(),
            warranty: None,
        })
        .expect_err("shipment must select allocations or serials");
        assert!(
            matches!(error, NetworkServiceError::Invalid(message) if message.contains("selector"))
        );
    }

    #[tokio::test]
    #[ignore = "requires INVENTORY_NETWORK_TEST_ADMIN_URL and INVENTORY_NETWORK_TEST_RUNTIME_URL"]
    async fn restricted_postgres_workflow_is_atomic_and_traceable() {
        let admin_url =
            std::env::var("INVENTORY_NETWORK_TEST_ADMIN_URL").expect("network test admin URL");
        let runtime_url =
            std::env::var("INVENTORY_NETWORK_TEST_RUNTIME_URL").expect("network test runtime URL");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&admin_url)
            .await
            .expect("connect setup database");

        let tenant_id = Uuid::now_v7();
        let workspace_id = Uuid::now_v7();
        let warehouse_id = Uuid::now_v7();
        let receiving_location_id = Uuid::now_v7();
        let storage_location_id = Uuid::now_v7();
        let shipping_location_id = Uuid::now_v7();
        let quarantine_location_id = Uuid::now_v7();
        let user_id = Uuid::now_v7();
        let membership_id = Uuid::now_v7();
        let role_id = Uuid::now_v7();
        let device_id = Uuid::now_v7();
        let password = "network-ops-test-password";
        let password_hash = PasswordService::recommended()
            .expect("password service")
            .hash_password(password)
            .expect("password hash");
        let mut setup = admin.begin().await.expect("begin setup");
        sqlx::query(
            "INSERT INTO tenants (id, slug, name) VALUES ($1, $2, 'Network Ops Test Tenant')",
        )
        .bind(tenant_id)
        .bind(format!("ops-test-{}", tenant_id.simple()))
        .execute(&mut *setup)
        .await
        .expect("insert tenant");
        sqlx::query("INSERT INTO workspaces (tenant_id, id, name, source_instance_id) VALUES ($1, $2, 'Network Ops', $3)")
            .bind(tenant_id)
            .bind(workspace_id)
            .bind(Uuid::now_v7())
            .execute(&mut *setup)
            .await
            .expect("insert workspace");
        sqlx::query(
            "INSERT INTO warehouses (tenant_id, id, code, name) VALUES ($1, $2, 'OPS', 'Ops')",
        )
        .bind(tenant_id)
        .bind(warehouse_id)
        .execute(&mut *setup)
        .await
        .expect("insert warehouse");
        for (id, code, kind) in [
            (receiving_location_id, "RECEIVING", "receiving"),
            (storage_location_id, "STORAGE", "storage"),
            (shipping_location_id, "SHIPPING", "shipping"),
            (quarantine_location_id, "QUARANTINE", "quarantine"),
        ] {
            sqlx::query("INSERT INTO locations (tenant_id, id, warehouse_id, code, name, kind) VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(tenant_id)
                .bind(id)
                .bind(warehouse_id)
                .bind(code)
                .bind(code)
                .bind(kind)
                .execute(&mut *setup)
                .await
                .expect("insert location");
        }
        sqlx::query("INSERT INTO users (tenant_id, id, login, normalized_login, display_name) VALUES ($1, $2, 'ops-operator', 'ops-operator', 'Ops Operator')")
            .bind(tenant_id)
            .bind(user_id)
            .execute(&mut *setup)
            .await
            .expect("insert user");
        sqlx::query(
            "INSERT INTO credentials (tenant_id, user_id, password_hash) VALUES ($1, $2, $3)",
        )
        .bind(tenant_id)
        .bind(user_id)
        .bind(password_hash)
        .execute(&mut *setup)
        .await
        .expect("insert credentials");
        sqlx::query("INSERT INTO memberships (tenant_id, id, user_id) VALUES ($1, $2, $3)")
            .bind(tenant_id)
            .bind(membership_id)
            .bind(user_id)
            .execute(&mut *setup)
            .await
            .expect("insert membership");
        sqlx::query("INSERT INTO roles (tenant_id, id, code, name) VALUES ($1, $2, 'ops-operator', 'Ops Operator')")
            .bind(tenant_id)
            .bind(role_id)
            .execute(&mut *setup)
            .await
            .expect("insert role");
        let permissions = [
            PERMISSION_NETWORK_ACCESS,
            PERMISSION_RECEIPT_WRITE,
            PERMISSION_QUALITY_WRITE,
            PERMISSION_ORDER_WRITE,
            PERMISSION_ALLOCATION_WRITE,
            PERMISSION_SHIPMENT_WRITE,
            PERMISSION_DELIVERY_WRITE,
            PERMISSION_RETURN_WRITE,
        ];
        for permission in permissions {
            let permission_id: Uuid = sqlx::query_scalar("INSERT INTO permissions (tenant_id, id, code, description) VALUES ($1, $2, $3, $4) ON CONFLICT (tenant_id, code) DO UPDATE SET description = EXCLUDED.description RETURNING id")
                .bind(tenant_id)
                .bind(Uuid::now_v7())
                .bind(permission)
                .bind(permission)
                .fetch_one(&mut *setup)
                .await
                .expect("insert permission");
            sqlx::query("INSERT INTO role_permissions (tenant_id, role_id, permission_id) VALUES ($1, $2, $3)")
                .bind(tenant_id)
                .bind(role_id)
                .bind(permission_id)
                .execute(&mut *setup)
                .await
                .expect("assign permission");
        }
        sqlx::query(
            "INSERT INTO membership_roles (tenant_id, membership_id, role_id) VALUES ($1, $2, $3)",
        )
        .bind(tenant_id)
        .bind(membership_id)
        .bind(role_id)
        .execute(&mut *setup)
        .await
        .expect("assign role");
        sqlx::query("INSERT INTO devices (tenant_id, id, membership_id, user_id, device_fingerprint, display_name) VALUES ($1, $2, $3, $4, $5, 'Ops Test Device')")
            .bind(tenant_id)
            .bind(device_id)
            .bind(membership_id)
            .bind(user_id)
            .bind(format!("ops-device-{device_id}"))
            .execute(&mut *setup)
            .await
            .expect("insert device");
        sqlx::query("INSERT INTO license_entitlements (tenant_id, id, license_id, edition, status, seat_limit, starts_at, expires_at, issuer, signature, key_id, claims_hash, verified_at) VALUES ($1, $2, $3, 'network', 'active', 5, CURRENT_TIMESTAMP - INTERVAL '1 hour', CURRENT_TIMESTAMP + INTERVAL '1 day', 'integration-test', 'test-signature', 'test-key', $4, CURRENT_TIMESTAMP)")
            .bind(tenant_id)
            .bind(Uuid::now_v7())
            .bind(format!("OPS-{}", tenant_id.simple()))
            .bind("b".repeat(64))
            .execute(&mut *setup)
            .await
            .expect("insert entitlement");
        setup.commit().await.expect("commit setup");

        let database = NetworkDatabase::connect(&NetworkDatabaseConfig::new(runtime_url))
            .await
            .expect("connect restricted runtime");
        let service = NetworkService::new(database).expect("network service");
        let login = service
            .login(LoginRequest {
                tenant_id,
                login: "ops-operator".to_owned(),
                password: password.to_owned(),
                device_id,
            })
            .await
            .expect("login");
        let suffix = Uuid::now_v7().simple().to_string();
        let barcode = format!("OPS-SN-{suffix}");
        service
            .create_catalog_party(
                tenant_id,
                &login.session_token,
                CreateCatalogPartyRequest {
                    display_name: "Owner OPS".to_owned(),
                    role: CatalogPartyRole::GoodsOwner,
                },
            )
            .await
            .expect("create operations receipt owner");
        service
            .create_catalog_party(
                tenant_id,
                &login.session_token,
                CreateCatalogPartyRequest {
                    display_name: "Supplier OPS".to_owned(),
                    role: CatalogPartyRole::Supplier,
                },
            )
            .await
            .expect("create operations receipt supplier");
        service
            .create_catalog_product(
                tenant_id,
                &login.session_token,
                CreateCatalogProductRequest {
                    code: format!("OPS-SKU-{suffix}"),
                    name: "Ops Model".to_owned(),
                    serial_prefix: None,
                    serial_forbidden_chars: String::new(),
                },
            )
            .await
            .expect("create operations receipt product");
        let receipt = service
            .post_receipt(
                tenant_id,
                &login.session_token,
                NetworkPostReceiptRequest {
                    request_id: format!("receipt-req-{suffix}"),
                    idempotency_key: format!("receipt-idem-{suffix}"),
                    receipt_no: format!("OPS-R-{suffix}"),
                    owner_name: "Owner OPS".to_owned(),
                    supplier_name: "Supplier OPS".to_owned(),
                    sku_code: format!("OPS-SKU-{suffix}"),
                    sku_name: "Ops Model".to_owned(),
                    warehouse_id,
                    source_reference: None,
                    received_at: "2026-08-03T01:00:00Z".to_owned(),
                    barcodes: vec![barcode.clone()],
                    notes: None,
                    warranty: None,
                },
            )
            .await
            .expect("post receipt");
        let inspection_request = NetworkCompleteInspectionRequest {
            request_id: format!("inspection-req-{suffix}"),
            idempotency_key: format!("inspection-idem-{suffix}"),
            inspection_no: format!("OPS-I-{suffix}"),
            inspection_kind: InspectionKind::Initial,
            inspected_at: "2026-08-03T02:00:00Z".to_owned(),
            results: vec![NetworkInspectionResultInput {
                barcode: barcode.clone(),
                outcome: QualityOutcome::Passed,
                quality_label_id: None,
                defect_code: None,
                measurements: json!({"voltage": 1.2}),
                notes: None,
            }],
        };
        let inspected = service
            .complete_quality_inspection(
                tenant_id,
                &login.session_token,
                inspection_request.clone(),
            )
            .await
            .expect("complete initial inspection");
        assert_eq!(inspected.passed_count, 1);
        let replay = service
            .complete_quality_inspection(tenant_id, &login.session_token, inspection_request)
            .await
            .expect("replay inspection");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.inspection_id, inspected.inspection_id);

        let order = service
            .create_outbound_order(
                tenant_id,
                &login.session_token,
                NetworkCreateOutboundOrderRequest {
                    request_id: format!("order-req-{suffix}"),
                    idempotency_key: format!("order-idem-{suffix}"),
                    order_no: format!("OPS-O-{suffix}"),
                    upstream_receiver_name: "Upstream OPS".to_owned(),
                    sku_code: format!("OPS-SKU-{suffix}"),
                    sku_name: "Ops Model".to_owned(),
                    required_quantity: 1,
                    required_at: None,
                },
            )
            .await
            .expect("create order");
        let allocation = service
            .allocate_outbound_order(
                tenant_id,
                &login.session_token,
                NetworkAllocateOutboundRequest {
                    request_id: format!("allocation-req-{suffix}"),
                    idempotency_key: format!("allocation-idem-{suffix}"),
                    order_id: order.order_id.parse().expect("order uuid"),
                    order_line_id: order.order_line_id.parse().expect("line uuid"),
                    barcodes: Vec::new(),
                    allow_mixed_skus: false,
                },
            )
            .await
            .expect("allocate FIFO");
        assert_eq!(allocation.allocated_count, 1);
        let allocation_id = allocation.allocations[0]
            .allocation_id
            .parse()
            .expect("allocation uuid");
        let shipment = service
            .ship_outbound_order(
                tenant_id,
                &login.session_token,
                NetworkShipOutboundRequest {
                    request_id: format!("ship-req-{suffix}"),
                    idempotency_key: format!("ship-idem-{suffix}"),
                    order_id: order.order_id.parse().expect("order uuid"),
                    shipment_no: format!("OPS-S-{suffix}"),
                    allocation_ids: vec![allocation_id],
                    barcodes: Vec::new(),
                    shipped_at: "2026-08-03T03:00:00Z".to_owned(),
                    warranty: None,
                },
            )
            .await
            .expect("ship order");
        let shipment_id = shipment.shipment_id.parse().expect("shipment uuid");
        let line_id = shipment.items[0]
            .shipment_line_id
            .parse()
            .expect("shipment line uuid");
        let delivery = service
            .confirm_outbound_delivery(
                tenant_id,
                &login.session_token,
                NetworkConfirmOutboundDeliveryRequest {
                    request_id: format!("delivery-req-{suffix}"),
                    idempotency_key: format!("delivery-idem-{suffix}"),
                    shipment_id,
                    confirmation_code: format!("OPS-D-{suffix}"),
                    shipment_line_ids: vec![line_id],
                    confirmed_at: "2026-08-03T04:00:00Z".to_owned(),
                    notes: None,
                },
            )
            .await
            .expect("confirm delivery");
        assert_eq!(delivery.delivered_count, 1);
        let returned = service
            .return_outbound_shipment(
                tenant_id,
                &login.session_token,
                NetworkReturnOutboundShipmentRequest {
                    request_id: format!("return-req-{suffix}"),
                    idempotency_key: format!("return-idem-{suffix}"),
                    shipment_id,
                    shipment_line_ids: vec![line_id],
                    return_no: format!("OPS-RET-{suffix}"),
                    returned_at: "2026-08-03T05:00:00Z".to_owned(),
                    reason: "customer return".to_owned(),
                },
            )
            .await
            .expect("quarantine return");
        assert_eq!(returned.quarantined_count, 1);
        let retest = service
            .complete_quality_inspection(
                tenant_id,
                &login.session_token,
                NetworkCompleteInspectionRequest {
                    request_id: format!("retest-req-{suffix}"),
                    idempotency_key: format!("retest-idem-{suffix}"),
                    inspection_no: format!("OPS-RETEST-{suffix}"),
                    inspection_kind: InspectionKind::Retest,
                    inspected_at: "2026-08-03T06:00:00Z".to_owned(),
                    results: vec![NetworkInspectionResultInput {
                        barcode: barcode.clone(),
                        outcome: QualityOutcome::Failed,
                        quality_label_id: None,
                        defect_code: Some("returned-damaged".to_owned()),
                        measurements: json!({}),
                        notes: None,
                    }],
                },
            )
            .await
            .expect("retest returned unit");
        assert_eq!(retest.failed_count, 1);
        let retest_pass = service
            .complete_quality_inspection(
                tenant_id,
                &login.session_token,
                NetworkCompleteInspectionRequest {
                    request_id: format!("retest-pass-req-{suffix}"),
                    idempotency_key: format!("retest-pass-idem-{suffix}"),
                    inspection_no: format!("OPS-RETEST-PASS-{suffix}"),
                    inspection_kind: InspectionKind::Retest,
                    inspected_at: "2026-08-03T07:00:00Z".to_owned(),
                    results: vec![NetworkInspectionResultInput {
                        barcode: barcode.clone(),
                        outcome: QualityOutcome::Passed,
                        quality_label_id: None,
                        defect_code: None,
                        measurements: json!({}),
                        notes: Some("retest passed".to_owned()),
                    }],
                },
            )
            .await
            .expect("retest returned unit as passed");
        assert_eq!(retest_pass.passed_count, 1);
        let second_order = service
            .create_outbound_order(
                tenant_id,
                &login.session_token,
                NetworkCreateOutboundOrderRequest {
                    request_id: format!("order-two-req-{suffix}"),
                    idempotency_key: format!("order-two-idem-{suffix}"),
                    order_no: format!("OPS-O-TWO-{suffix}"),
                    upstream_receiver_name: "Upstream OPS Two".to_owned(),
                    sku_code: format!("OPS-SKU-{suffix}"),
                    sku_name: "Ops Model".to_owned(),
                    required_quantity: 1,
                    required_at: None,
                },
            )
            .await
            .expect("create replacement order");
        let second_allocation = service
            .allocate_outbound_order(
                tenant_id,
                &login.session_token,
                NetworkAllocateOutboundRequest {
                    request_id: format!("allocation-two-req-{suffix}"),
                    idempotency_key: format!("allocation-two-idem-{suffix}"),
                    order_id: second_order.order_id.parse().expect("second order uuid"),
                    order_line_id: second_order
                        .order_line_id
                        .parse()
                        .expect("second line uuid"),
                    barcodes: vec![barcode.clone()],
                    allow_mixed_skus: false,
                },
            )
            .await
            .expect("reallocate returned unit");
        assert_eq!(second_allocation.allocated_count, 1);
        let second_shipment = service
            .ship_outbound_order(
                tenant_id,
                &login.session_token,
                NetworkShipOutboundRequest {
                    request_id: format!("ship-two-req-{suffix}"),
                    idempotency_key: format!("ship-two-idem-{suffix}"),
                    order_id: second_order.order_id.parse().expect("second order uuid"),
                    shipment_no: format!("OPS-S-TWO-{suffix}"),
                    allocation_ids: vec![second_allocation.allocations[0]
                        .allocation_id
                        .parse()
                        .expect("second allocation uuid")],
                    barcodes: Vec::new(),
                    shipped_at: "2026-08-03T08:00:00Z".to_owned(),
                    warranty: None,
                },
            )
            .await
            .expect("ship replacement order");
        assert_eq!(second_shipment.shipped_count, 1);
        let trace = service
            .inventory_trace(tenant_id, &login.session_token, &barcode)
            .await
            .expect("query complete network trace");
        assert_eq!(trace.owner_name, "Owner OPS");
        assert_eq!(trace.inspections.len(), 3);
        assert_eq!(trace.outbound.len(), 2);
        assert_eq!(trace.outbound[0].upstream_receiver_name, "Upstream OPS");
        assert!(trace.outbound[0].confirmation_code.is_some());
        assert!(trace.outbound[0].return_no.is_some());
        assert_eq!(
            trace.outbound[1].shipment_no.as_deref(),
            Some(second_shipment.shipment_no.as_str())
        );

        let mut verification = admin.begin().await.expect("begin verification");
        let status: (String, String) = sqlx::query_as(
            "SELECT inventory_status, quality_status FROM inventory_units WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(receipt.units[0].inventory_unit_id.parse::<Uuid>().expect("unit uuid"))
        .fetch_one(&mut *verification)
        .await
        .expect("verify final unit");
        assert_eq!(status, ("shipped".to_owned(), "passed".to_owned()));
        let movement_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM stock_movements WHERE tenant_id = $1 AND inventory_unit_id = $2",
        )
        .bind(tenant_id)
        .bind(
            receipt.units[0]
                .inventory_unit_id
                .parse::<Uuid>()
                .expect("unit uuid"),
        )
        .fetch_one(&mut *verification)
        .await
        .expect("verify movements");
        assert_eq!(movement_count, 10);
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_logs WHERE tenant_id = $1 AND actor_id = $2 AND membership_id = $3 AND device_id = $4 AND session_id = $5",
        )
        .bind(tenant_id)
        .bind(user_id)
        .bind(membership_id)
        .bind(device_id)
        .bind(login.session_id)
        .fetch_one(&mut *verification)
        .await
        .expect("verify authenticated audits");
        assert_eq!(audit_count, 12);
        verification.commit().await.expect("commit verification");
        admin.close().await;
    }
}
