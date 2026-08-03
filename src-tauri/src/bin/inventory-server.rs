use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use inventory_manager_lib::v2::auth::{AuthError, AuthorizationDenial};
use inventory_manager_lib::v2::network::{
    LoginRequest, NetworkPostReceiptRequest, NetworkService, NetworkServiceError, RefreshRequest,
};
use inventory_manager_lib::v2::postgres::{NetworkDatabase, NetworkDatabaseConfig};
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::Arc;
use uuid::Uuid;

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
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/logout", post(logout))
        .route("/v1/inbound/receipts", post(post_receipt))
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
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ApiError(NetworkServiceError::Auth(AuthError::InvalidSession)))
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
