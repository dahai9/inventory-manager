use axum::body::to_bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use inventory_manager_lib::v2::auth::{AuthError, AuthorizationDenial};
use inventory_manager_lib::v2::network::{
    LoginRequest, NetworkPostReceiptRequest, NetworkService, NetworkServiceError, RefreshRequest,
};
use inventory_manager_lib::v2::network_ops::{
    NetworkAllocateOutboundRequest, NetworkCompleteInspectionRequest,
    NetworkConfirmOutboundDeliveryRequest, NetworkCreateOutboundOrderRequest,
    NetworkReturnOutboundShipmentRequest, NetworkShipOutboundRequest,
};
use inventory_manager_lib::v2::postgres::{NetworkDatabase, NetworkDatabaseConfig};
use inventory_manager_lib::v2::upgrade::{NetworkUpgradeImportRequest, MAX_NETWORK_REQUEST_BYTES};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use uuid::Uuid;

const MAX_STANDARD_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_BEARER_TOKEN_BYTES: usize = 1024;

#[derive(Clone)]
struct ServerState {
    service: Arc<NetworkService>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ErrorResponse {
    code: &'static str,
    message: &'static str,
}

#[derive(Debug)]
struct ApiError(NetworkServiceError);

impl From<NetworkServiceError> for ApiError {
    fn from(error: NetworkServiceError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self.0 {
            NetworkServiceError::Invalid(_) => {
                (StatusCode::BAD_REQUEST, "invalid_request", "请求数据无效")
            }
            NetworkServiceError::Conflict { .. } => {
                (StatusCode::CONFLICT, "business_conflict", "业务数据冲突")
            }
            NetworkServiceError::Auth(AuthError::InvalidCredentials)
            | NetworkServiceError::Auth(AuthError::LoginLocked)
            | NetworkServiceError::Auth(AuthError::InvalidRefreshToken)
            | NetworkServiceError::Auth(AuthError::InvalidSession)
            | NetworkServiceError::Database(
                inventory_manager_lib::v2::postgres::NetworkDatabaseError::Authorization(
                    AuthError::InvalidSession,
                ),
            ) => (StatusCode::UNAUTHORIZED, "unauthorized", "认证失败"),
            NetworkServiceError::Auth(AuthError::AccessDenied(reason))
            | NetworkServiceError::Database(
                inventory_manager_lib::v2::postgres::NetworkDatabaseError::Authorization(
                    AuthError::AccessDenied(reason),
                ),
            ) => (
                StatusCode::FORBIDDEN,
                denial_code(*reason),
                "当前账号、权限或租户授权不可用",
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "服务暂时不可用",
            ),
        };
        if status.is_server_error() {
            eprintln!("network request failed: {}", self.0);
        }
        (status, Json(ErrorResponse { code, message })).into_response()
    }
}

fn denial_code(reason: AuthorizationDenial) -> &'static str {
    match reason {
        AuthorizationDenial::TenantInactive => "tenant_inactive",
        AuthorizationDenial::AccountDisabled => "account_disabled",
        AuthorizationDenial::MembershipInactive => "membership_inactive",
        AuthorizationDenial::NoActiveRole => "role_unavailable",
        AuthorizationDenial::LicenseUnavailable => "license_unavailable",
        AuthorizationDenial::MissingPermission => "permission_denied",
        AuthorizationDenial::UnknownPrincipal => "unknown_principal",
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = required_env("INVENTORY_DATABASE_URL")?;
    let mut database_config = NetworkDatabaseConfig::new(database_url);
    database_config.migration_url = std::env::var("INVENTORY_MIGRATION_DATABASE_URL").ok();
    let database = NetworkDatabase::connect(&database_config).await?;
    let state = ServerState {
        service: Arc::new(NetworkService::new(database)?),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(readiness))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/logout", post(logout))
        .route("/v1/inventory/query", post(list_inventory))
        .route("/v1/inventory/summary", post(inventory_summary))
        .route("/v1/inventory/trace", post(inventory_trace))
        .route("/v1/reference/warehouses/query", post(list_warehouses))
        .route("/v1/inbound/receipts", post(post_receipt))
        .route(
            "/v1/upgrades/offline-imports",
            post(import_upgrade_package).layer(DefaultBodyLimit::max(MAX_NETWORK_REQUEST_BYTES)),
        )
        .route("/v1/quality/inspections", post(complete_quality_inspection))
        .route("/v1/outbound/orders", post(create_outbound_order))
        .route("/v1/outbound/allocations", post(allocate_outbound_order))
        .route("/v1/outbound/shipments", post(ship_outbound_order))
        .route("/v1/outbound/deliveries", post(confirm_outbound_delivery))
        .route("/v1/outbound/returns", post(return_outbound_shipment))
        .layer(DefaultBodyLimit::max(MAX_STANDARD_REQUEST_BYTES))
        .with_state(state);
    let bind_address = std::env::var("INVENTORY_BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3100".to_owned())
        .parse::<SocketAddr>()?;
    let listener = tokio::net::TcpListener::bind(bind_address).await?;
    println!("inventory network server listening on {bind_address}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn readiness(State(state): State<ServerState>) -> Response {
    match state.service.readiness().await {
        Ok(()) => (StatusCode::OK, Json(HealthResponse { status: "ready" })).into_response(),
        Err(error) => {
            eprintln!("network readiness check failed: {error}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(HealthResponse {
                    status: "unavailable",
                }),
            )
                .into_response()
        }
    }
}

async fn login(
    State(state): State<ServerState>,
    Json(request): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .service
        .login(request)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn refresh(
    State(state): State<ServerState>,
    Json(request): Json<RefreshRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .service
        .refresh(request)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn logout(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .logout(tenant_id, bearer)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(Into::into)
}

async fn post_receipt(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<NetworkPostReceiptRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .post_receipt(tenant_id, bearer, request)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn import_upgrade_package(
    State(state): State<ServerState>,
    headers: HeaderMap,
    request: Request,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .authorize_upgrade_import(tenant_id, bearer)
        .await?;
    let payload = to_bytes(request.into_body(), MAX_NETWORK_REQUEST_BYTES)
        .await
        .map_err(|_| {
            ApiError(NetworkServiceError::Invalid(
                "upgrade request body is invalid or too large".to_owned(),
            ))
        })?;
    let request: NetworkUpgradeImportRequest = serde_json::from_slice(&payload).map_err(|_| {
        ApiError(NetworkServiceError::Invalid(
            "upgrade request JSON is invalid".to_owned(),
        ))
    })?;
    state
        .service
        .import_upgrade_package(tenant_id, bearer, request)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn list_inventory(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(query): Json<inventory_manager_lib::v2::application::InventoryListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .list_inventory(tenant_id, bearer, query)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn inventory_summary(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(query): Json<inventory_manager_lib::v2::application::InventorySummaryQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .inventory_summary(tenant_id, bearer, query)
        .await
        .map(Json)
        .map_err(Into::into)
}

#[derive(Deserialize)]
struct InventoryTraceRequest {
    barcode: String,
}

async fn inventory_trace(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<InventoryTraceRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .inventory_trace(tenant_id, bearer, &request.barcode)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn list_warehouses(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .list_warehouses(tenant_id, bearer)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn complete_quality_inspection(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<NetworkCompleteInspectionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .complete_quality_inspection(tenant_id, bearer, request)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn create_outbound_order(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<NetworkCreateOutboundOrderRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .create_outbound_order(tenant_id, bearer, request)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn allocate_outbound_order(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<NetworkAllocateOutboundRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .allocate_outbound_order(tenant_id, bearer, request)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn ship_outbound_order(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<NetworkShipOutboundRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .ship_outbound_order(tenant_id, bearer, request)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn confirm_outbound_delivery(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<NetworkConfirmOutboundDeliveryRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .confirm_outbound_delivery(tenant_id, bearer, request)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn return_outbound_shipment(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<NetworkReturnOutboundShipmentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant_id = tenant_id(&headers)?;
    let bearer = bearer_token(&headers)?;
    state
        .service
        .return_outbound_shipment(tenant_id, bearer, request)
        .await
        .map(Json)
        .map_err(Into::into)
}

fn tenant_id(headers: &HeaderMap) -> Result<Uuid, ApiError> {
    headers
        .get("x-tenant-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| {
            ApiError(NetworkServiceError::Invalid(
                "invalid x-tenant-id".to_owned(),
            ))
        })
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, ApiError> {
    let value = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError(NetworkServiceError::Auth(AuthError::InvalidSession)))?;
    let (scheme, token) = value
        .split_once(' ')
        .ok_or_else(|| ApiError(NetworkServiceError::Auth(AuthError::InvalidSession)))?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.len() > MAX_BEARER_TOKEN_BYTES
        || !token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ApiError(NetworkServiceError::Auth(
            AuthError::InvalidSession,
        )));
    }
    Ok(token)
}

fn required_env(name: &str) -> Result<String, std::io::Error> {
    std::env::var(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("required environment variable {name} is missing"),
        )
    })
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn bearer_scheme_is_case_insensitive_and_token_is_returned() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("bearer abc_123"));
        assert_eq!(bearer_token(&headers).expect("valid bearer"), "abc_123");
    }

    #[test]
    fn bearer_rejects_whitespace_and_oversized_tokens() {
        for value in ["Bearer", "Bearer  abc", "Bearer abc def", "Basic abc"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "authorization",
                HeaderValue::from_str(value).expect("test header"),
            );
            assert!(bearer_token(&headers).is_err(), "accepted {value:?}");
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!(
                "Bearer {}",
                "a".repeat(MAX_BEARER_TOKEN_BYTES + 1)
            ))
            .expect("large test header"),
        );
        assert!(bearer_token(&headers).is_err());
    }
}
