//! Network-edition service boundary backed by PostgreSQL.
//!
//! Route adapters call these methods after decoding HTTP/gRPC payloads. The
//! service owns permission names and derives tenant actors from bearer
//! sessions, so clients cannot forge authorization context in business DTOs.

use super::application::{PostReceiptResponse, ReceiptUnit};
use super::auth::{
    authenticate_password, issue_session, revoke_session, rotate_refresh_token, AuthError,
    LockoutPolicy, PasswordService, SessionPolicy,
};
use super::postgres::{NetworkDatabase, NetworkDatabaseError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use std::collections::HashSet;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

pub const PERMISSION_NETWORK_ACCESS: &str = "inventory.access";
pub const PERMISSION_RECEIPT_WRITE: &str = "inventory.receipt.write";
const RECEIPT_SCOPE: &str = "post_inbound_receipt";

#[derive(Clone)]
pub struct NetworkService {
    database: NetworkDatabase,
    passwords: Arc<PasswordService>,
    lockout_policy: LockoutPolicy,
    session_policy: SessionPolicy,
}

impl NetworkService {
    pub fn new(database: NetworkDatabase) -> Result<Self, NetworkServiceError> {
        Ok(Self {
            database,
            passwords: Arc::new(PasswordService::recommended()?),
            lockout_policy: LockoutPolicy::default(),
            session_policy: SessionPolicy::default(),
        })
    }

    pub async fn login(&self, request: LoginRequest) -> NetworkResult<LoginResponse> {
        let normalized_login = required("login", request.login)?;
        let password = required("password", request.password)?;
        let identity = authenticate_password(
            self.database.pool(),
            &self.passwords,
            request.tenant_id,
            &normalized_login,
            &password,
            self.lockout_policy,
        )
        .await?;
        let session = issue_session(
            self.database.pool(),
            &identity,
            request.device_id,
            PERMISSION_NETWORK_ACCESS,
            self.session_policy,
        )
        .await?;
        Ok(LoginResponse {
            tenant_id: identity.tenant_id,
            user_id: identity.user_id,
            membership_id: identity.membership_id,
            session_id: session.session_id,
            session_token: session.session_token,
            refresh_token: session.refresh_token,
            session_ttl_seconds: session.session_ttl_seconds,
            refresh_ttl_seconds: session.refresh_ttl_seconds,
        })
    }

    pub async fn post_receipt(
        &self,
        tenant_id: Uuid,
        session_token: &str,
        request: NetworkPostReceiptRequest,
    ) -> NetworkResult<PostReceiptResponse> {
        let request = normalize_receipt(request)?;
        let digest = request_digest(&request)?;
        let mut authorized = self
            .database
            .begin_authorized_request(tenant_id, session_token, PERMISSION_RECEIPT_WRITE)
            .await?;
        let actor_id = authorized.session().identity.user_id;
        let membership_id = authorized.session().identity.membership_id;
        let device_id = authorized.session().device_id;
        let session_id = authorized.session().session_id;
        let transaction = authorized.sqlx_transaction();

        if let Some(mut replay) = claim_idempotency(
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

        ensure_warehouse_and_receiving_location(transaction, tenant_id, request.warehouse_id)
            .await?;
        let receiving_location_id: Uuid = sqlx::query_scalar(
            "SELECT id FROM locations WHERE tenant_id = $1 AND warehouse_id = $2 AND kind = 'receiving' ORDER BY id LIMIT 1",
        )
        .bind(tenant_id)
        .bind(request.warehouse_id)
        .fetch_one(&mut **transaction)
        .await?;
        let owner_id =
            upsert_party(transaction, tenant_id, &request.owner_name, "goods_owner").await?;
        let sku_id =
            upsert_sku(transaction, tenant_id, &request.sku_code, &request.sku_name).await?;
        let receipt_id = Uuid::now_v7();
        let receipt_line_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO inbound_receipts
                (tenant_id, id, receipt_no, owner_party_id, warehouse_id,
                 source_reference, received_at, status, actor_id,
                 idempotency_key, request_id)
            VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz, 'posted',
                    $8, $9, $10)
            "#,
        )
        .bind(tenant_id)
        .bind(receipt_id)
        .bind(&request.receipt_no)
        .bind(owner_id)
        .bind(request.warehouse_id)
        .bind(&request.source_reference)
        .bind(&request.received_at)
        .bind(actor_id)
        .bind(&request.idempotency_key)
        .bind(&request.request_id)
        .execute(&mut **transaction)
        .await
        .map_err(|error| conflict_or_database("receipt", &request.receipt_no, error))?;
        sqlx::query(
            r#"
            INSERT INTO inbound_receipt_lines
                (tenant_id, id, receipt_id, sku_id, declared_quantity,
                 scanned_quantity, notes)
            VALUES ($1, $2, $3, $4, $5, $5, $6)
            "#,
        )
        .bind(tenant_id)
        .bind(receipt_line_id)
        .bind(receipt_id)
        .bind(sku_id)
        .bind(i32::try_from(request.barcodes.len()).map_err(|_| {
            NetworkServiceError::Invalid("too many barcodes in one receipt".to_owned())
        })?)
        .bind(&request.notes)
        .execute(&mut **transaction)
        .await?;

        let mut units = Vec::with_capacity(request.barcodes.len());
        for barcode in &request.barcodes {
            let unit_id = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO inventory_units
                    (tenant_id, id, barcode, inbound_receipt_line_id,
                     owner_party_id, sku_id, location_id, inventory_status,
                     quality_status, version, received_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, 'received', 'untested',
                        1, $8::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(unit_id)
            .bind(barcode)
            .bind(receipt_line_id)
            .bind(owner_id)
            .bind(sku_id)
            .bind(receiving_location_id)
            .bind(&request.received_at)
            .execute(&mut **transaction)
            .await
            .map_err(|error| conflict_or_database("barcode", barcode, error))?;
            sqlx::query(
                r#"
                INSERT INTO stock_movements
                    (tenant_id, id, inventory_unit_id, movement_type,
                     to_location_id, source_type, source_id, actor_id,
                     occurred_at)
                VALUES ($1, $2, $3, 'received', $4, 'inbound_receipt', $5,
                        $6, $7::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(Uuid::now_v7())
            .bind(unit_id)
            .bind(receiving_location_id)
            .bind(receipt_id)
            .bind(actor_id)
            .bind(&request.received_at)
            .execute(&mut **transaction)
            .await?;
            units.push(ReceiptUnit {
                inventory_unit_id: unit_id.to_string(),
                barcode: barcode.clone(),
            });
        }

        let response = PostReceiptResponse {
            receipt_id: receipt_id.to_string(),
            receipt_line_id: receipt_line_id.to_string(),
            receipt_no: request.receipt_no.clone(),
            owner_party_id: owner_id.to_string(),
            sku_id: sku_id.to_string(),
            received_count: units.len() as u32,
            units,
            idempotent_replay: false,
        };
        let audit_details = serde_json::json!({
            "receipt_no": request.receipt_no,
            "received_count": response.received_count,
            "owner_party_id": owner_id,
            "sku_id": sku_id,
        });
        sqlx::query(
            r#"
            INSERT INTO audit_logs
                (tenant_id, id, actor_id, membership_id, device_id, session_id,
                 action, entity_type, entity_id, request_id, result, details_json)
            VALUES ($1, $2, $3, $4, $5, $6,
                    'inbound_receipt.posted', 'inbound_receipt',
                    $7, $8, 'success', $9::jsonb)
            "#,
        )
        .bind(tenant_id)
        .bind(Uuid::now_v7())
        .bind(actor_id)
        .bind(membership_id)
        .bind(device_id)
        .bind(session_id)
        .bind(receipt_id)
        .bind(&request.request_id)
        .bind(serde_json::to_string(&audit_details)?)
        .execute(&mut **transaction)
        .await?;
        finish_idempotency(
            transaction,
            tenant_id,
            RECEIPT_SCOPE,
            &request.idempotency_key,
            &response,
        )
        .await?;
        authorized.commit().await?;
        Ok(response)
    }

    pub async fn refresh(&self, request: RefreshRequest) -> NetworkResult<RefreshResponse> {
        let refresh_token = required("refresh_token", request.refresh_token)?;
        let (identity, session) = rotate_refresh_token(
            self.database.pool(),
            request.tenant_id,
            &refresh_token,
            PERMISSION_NETWORK_ACCESS,
            self.session_policy,
        )
        .await?;
        Ok(RefreshResponse {
            tenant_id: identity.tenant_id,
            user_id: identity.user_id,
            membership_id: identity.membership_id,
            session_id: session.session_id,
            session_token: session.session_token,
            refresh_token: session.refresh_token,
            session_ttl_seconds: session.session_ttl_seconds,
            refresh_ttl_seconds: session.refresh_ttl_seconds,
        })
    }

    pub async fn logout(&self, tenant_id: Uuid, session_token: &str) -> NetworkResult<()> {
        revoke_session(
            self.database.pool(),
            tenant_id,
            session_token,
            "user_logout",
        )
        .await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoginRequest {
    pub tenant_id: Uuid,
    pub login: String,
    pub password: String,
    pub device_id: Uuid,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoginResponse {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub membership_id: Uuid,
    pub session_id: Uuid,
    pub session_token: String,
    pub refresh_token: String,
    pub session_ttl_seconds: i64,
    pub refresh_ttl_seconds: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RefreshRequest {
    pub tenant_id: Uuid,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RefreshResponse {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub membership_id: Uuid,
    pub session_id: Uuid,
    pub session_token: String,
    pub refresh_token: String,
    pub session_ttl_seconds: i64,
    pub refresh_ttl_seconds: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkPostReceiptRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub receipt_no: String,
    pub owner_name: String,
    pub sku_code: String,
    pub sku_name: String,
    pub warehouse_id: Uuid,
    pub source_reference: Option<String>,
    pub received_at: String,
    pub barcodes: Vec<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Error)]
pub enum NetworkServiceError {
    #[error("invalid network request: {0}")]
    Invalid(String),
    #[error("network business conflict for {entity} {key}")]
    Conflict { entity: String, key: String },
    #[error("authentication or authorization failed: {0}")]
    Auth(#[from] AuthError),
    #[error("network database failed: {0}")]
    Database(#[from] NetworkDatabaseError),
    #[error("PostgreSQL operation failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("network response JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub type NetworkResult<T> = Result<T, NetworkServiceError>;

fn normalize_receipt(
    mut request: NetworkPostReceiptRequest,
) -> NetworkResult<NetworkPostReceiptRequest> {
    request.request_id = required("request_id", request.request_id)?;
    request.idempotency_key = required("idempotency_key", request.idempotency_key)?;
    request.receipt_no = required("receipt_no", request.receipt_no)?;
    request.owner_name = required("owner_name", request.owner_name)?;
    request.sku_code = required("sku_code", request.sku_code)?;
    request.sku_name = required("sku_name", request.sku_name)?;
    request.received_at = required("received_at", request.received_at)?;
    if request.barcodes.is_empty() {
        return Err(NetworkServiceError::Invalid(
            "barcodes must not be empty".to_owned(),
        ));
    }
    let mut seen = HashSet::new();
    request.barcodes = request
        .barcodes
        .into_iter()
        .map(|barcode| required("barcode", barcode))
        .collect::<NetworkResult<_>>()?;
    for barcode in &request.barcodes {
        if !seen.insert(barcode.clone()) {
            return Err(NetworkServiceError::Invalid(format!(
                "duplicate barcode {barcode}"
            )));
        }
    }
    Ok(request)
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

async fn ensure_warehouse_and_receiving_location(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    warehouse_id: Uuid,
) -> NetworkResult<()> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM warehouses w JOIN locations l ON l.tenant_id = w.tenant_id AND l.warehouse_id = w.id AND l.kind = 'receiving' WHERE w.tenant_id = $1 AND w.id = $2)",
    )
    .bind(tenant_id)
    .bind(warehouse_id)
    .fetch_one(&mut **transaction)
    .await?;
    if !exists {
        return Err(NetworkServiceError::Invalid(
            "warehouse does not have a receiving location".to_owned(),
        ));
    }
    Ok(())
}

async fn upsert_party(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    display_name: &str,
    role: &str,
) -> NetworkResult<Uuid> {
    let normalized = display_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let party_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO business_parties
            (tenant_id, id, normalized_name, display_name)
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
    .bind(party_id)
    .bind(role)
    .execute(&mut **transaction)
    .await?;
    Ok(party_id)
}

async fn upsert_sku(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    code: &str,
    name: &str,
) -> NetworkResult<Uuid> {
    sqlx::query_scalar(
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
    .await
    .map_err(Into::into)
}

async fn claim_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    scope: &str,
    key: &str,
    digest: &str,
) -> NetworkResult<Option<PostReceiptResponse>> {
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
    if row.try_get::<String, _>("request_hash")? != digest {
        return Err(NetworkServiceError::Conflict {
            entity: "idempotency_key".to_owned(),
            key: key.to_owned(),
        });
    }
    let response_json: String = row.try_get("response_json")?;
    let response = serde_json::from_str(&response_json).map_err(|_| {
        NetworkServiceError::Invalid("idempotency record has no committed response".to_owned())
    })?;
    Ok(Some(response))
}

async fn finish_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    scope: &str,
    key: &str,
    response: &PostReceiptResponse,
) -> NetworkResult<()> {
    let result = sqlx::query(
        "UPDATE idempotency_records SET response_json = $4::jsonb WHERE tenant_id = $1 AND scope = $2 AND idempotency_key = $3",
    )
    .bind(tenant_id)
    .bind(scope)
    .bind(key)
    .bind(serde_json::to_string(response)?)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(NetworkServiceError::Invalid(
            "idempotency claim disappeared".to_owned(),
        ));
    }
    Ok(())
}

fn request_digest<T: Serialize>(request: &T) -> NetworkResult<String> {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(request)?);
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn conflict_or_database(entity: &str, key: &str, error: sqlx::Error) -> NetworkServiceError {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_receipt_rejects_duplicate_barcodes_before_database_access() {
        let error = normalize_receipt(NetworkPostReceiptRequest {
            request_id: "request".to_owned(),
            idempotency_key: "key".to_owned(),
            receipt_no: "RK-1".to_owned(),
            owner_name: "Owner".to_owned(),
            sku_code: "SKU".to_owned(),
            sku_name: "Model".to_owned(),
            warehouse_id: Uuid::now_v7(),
            source_reference: None,
            received_at: "2026-08-03T01:00:00Z".to_owned(),
            barcodes: vec!["SN-1".to_owned(), "SN-1".to_owned()],
            notes: None,
        })
        .expect_err("duplicates must fail");
        assert!(
            matches!(error, NetworkServiceError::Invalid(message) if message.contains("duplicate"))
        );
    }
}
