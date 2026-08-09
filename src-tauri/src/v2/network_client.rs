//! Thin HTTP client for the network edition.
//!
//! The desktop client deliberately knows only the API endpoint and the
//! short-lived session returned by the server.  PostgreSQL credentials,
//! tenant authorization and actor identity stay on the server side.

use super::application::{
    CatalogParty, CatalogProduct, CompleteInspectionResponse, CreateCatalogPartyRequest,
    CreateCatalogProductRequest, InventoryListResponse, InventorySummaryResponse,
    PostReceiptResponse, ReferenceCatalog,
};
use super::identity_admin::{
    CreateTenantUserRequest, CreateTenantUserResponse, DisableTenantUserRequest,
    DisableTenantUserResponse, ListTenantUsersRequest, ListTenantUsersResponse,
    MembershipPermissionsRequest, MembershipPermissionsResponse, ReplaceMembershipRolesRequest,
    TenantRoleSummary,
};
use super::network::{
    LoginRequest, LoginResponse, NetworkPostReceiptRequest, NetworkWarehouse, RefreshRequest,
    RefreshResponse,
};
use super::network_ops::{
    NetworkAllocateOutboundRequest, NetworkCompleteInspectionRequest,
    NetworkConfirmOutboundDeliveryRequest, NetworkCreateOutboundOrderRequest,
    NetworkReturnOutboundShipmentRequest, NetworkShipOutboundRequest,
};
use super::traceability::{InventoryBarcodeExistsResponse, InventoryTrace};
use super::upgrade::{NetworkUpgradeImportRequest, NetworkUpgradeImportResponse};
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
pub struct NetworkClient {
    http: Client,
    base_url: Arc<Mutex<Option<String>>>,
    session: Arc<Mutex<Option<NetworkSession>>>,
}

impl Default for NetworkClient {
    fn default() -> Self {
        Self {
            http: Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("network HTTP client configuration must be valid"),
            base_url: Arc::new(Mutex::new(None)),
            session: Arc::new(Mutex::new(None)),
        }
    }
}

impl fmt::Debug for NetworkClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetworkClient")
            .field("base_url", &self.base_url().ok().flatten())
            .field("authenticated", &self.session().ok().flatten().is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSession {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub membership_id: Uuid,
    pub session_id: Uuid,
    pub session_token: String,
    pub refresh_token: String,
    pub session_ttl_seconds: i64,
    pub refresh_ttl_seconds: i64,
}

impl From<LoginResponse> for NetworkSession {
    fn from(response: LoginResponse) -> Self {
        Self {
            tenant_id: response.tenant_id,
            user_id: response.user_id,
            membership_id: response.membership_id,
            session_id: response.session_id,
            session_token: response.session_token,
            refresh_token: response.refresh_token,
            session_ttl_seconds: response.session_ttl_seconds,
            refresh_ttl_seconds: response.refresh_ttl_seconds,
        }
    }
}

impl From<RefreshResponse> for NetworkSession {
    fn from(response: RefreshResponse) -> Self {
        Self {
            tenant_id: response.tenant_id,
            user_id: response.user_id,
            membership_id: response.membership_id,
            session_id: response.session_id,
            session_token: response.session_token,
            refresh_token: response.refresh_token,
            session_ttl_seconds: response.session_ttl_seconds,
            refresh_ttl_seconds: response.refresh_ttl_seconds,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkClientError {
    #[error("network API address is not configured")]
    NotConfigured,
    #[error("network client state is unavailable: {0}")]
    State(String),
    #[error("network request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("network request JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("network API returned HTTP {status}: {body}")]
    Api { status: u16, body: String },
    #[error("network session is not authenticated")]
    Unauthenticated,
    #[error("invalid network API address: {0}")]
    InvalidUrl(String),
    #[error("network response is too large")]
    ResponseTooLarge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ErrorResponse {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl NetworkClient {
    /// Store an endpoint without a trailing slash.  Keeping the optional path
    /// prefix (for example `https://host/inventory-api`) makes both `/v1` and
    /// `/api/v1` deployments work without URL::join dropping the prefix.
    pub fn configure(&self, base_url: String) -> Result<String, NetworkClientError> {
        let trimmed = base_url.trim().trim_end_matches('/').to_owned();
        if trimmed.is_empty() {
            return Err(NetworkClientError::NotConfigured);
        }
        let parsed = reqwest::Url::parse(&trimmed)
            .map_err(|error| NetworkClientError::InvalidUrl(error.to_string()))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(NetworkClientError::InvalidUrl(
                "API address must use http or https".to_owned(),
            ));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(NetworkClientError::InvalidUrl(
                "API address must not contain credentials".to_owned(),
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(NetworkClientError::InvalidUrl(
                "API address must not contain a query or fragment".to_owned(),
            ));
        }
        if parsed.scheme() == "http"
            && !matches!(
                parsed.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            )
        {
            return Err(NetworkClientError::InvalidUrl(
                "非本机网络服务必须使用 HTTPS".to_owned(),
            ));
        }
        *self
            .base_url
            .lock()
            .map_err(|error| NetworkClientError::State(error.to_string()))? = Some(trimmed.clone());
        Ok(trimmed)
    }

    pub fn base_url(&self) -> Result<Option<String>, NetworkClientError> {
        self.base_url
            .lock()
            .map(|value| value.clone())
            .map_err(|error| NetworkClientError::State(error.to_string()))
    }

    pub fn session(&self) -> Result<Option<NetworkSession>, NetworkClientError> {
        self.session
            .lock()
            .map(|value| value.clone())
            .map_err(|error| NetworkClientError::State(error.to_string()))
    }

    pub fn clear_session(&self) -> Result<(), NetworkClientError> {
        *self
            .session
            .lock()
            .map_err(|error| NetworkClientError::State(error.to_string()))? = None;
        Ok(())
    }

    pub async fn login(
        &self,
        tenant_id: Uuid,
        login: String,
        password: String,
        device_id: Uuid,
    ) -> Result<NetworkSession, NetworkClientError> {
        let response: LoginResponse = self
            .public_json(
                Method::POST,
                "/v1/auth/login",
                &LoginRequest {
                    tenant_id,
                    login,
                    password,
                    device_id,
                },
            )
            .await?;
        let session = NetworkSession::from(response);
        self.store_session(session.clone())?;
        Ok(session)
    }

    pub async fn refresh(&self) -> Result<NetworkSession, NetworkClientError> {
        let current = self.session()?.ok_or(NetworkClientError::Unauthenticated)?;
        let response: RefreshResponse = self
            .public_json(
                Method::POST,
                "/v1/auth/refresh",
                &RefreshRequest {
                    tenant_id: current.tenant_id,
                    refresh_token: current.refresh_token,
                },
            )
            .await?;
        let session = NetworkSession::from(response);
        self.store_session(session.clone())?;
        Ok(session)
    }

    pub async fn logout(&self) -> Result<(), NetworkClientError> {
        let current = self.session()?.ok_or(NetworkClientError::Unauthenticated)?;
        let result: Value = self
            .authorized_json(Method::POST, "/v1/auth/logout", &Value::Null)
            .await?;
        let _ = result;
        self.clear_session()?;
        let _ = current;
        Ok(())
    }

    pub async fn post_receipt(
        &self,
        request: &NetworkPostReceiptRequest,
    ) -> Result<PostReceiptResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/inbound/receipts", request)
            .await
    }

    pub async fn list_reference_catalog(&self) -> Result<ReferenceCatalog, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/reference/catalog/query", &Value::Null)
            .await
    }

    pub async fn create_catalog_product(
        &self,
        request: &CreateCatalogProductRequest,
    ) -> Result<CatalogProduct, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/reference/products", request)
            .await
    }

    pub async fn create_catalog_party(
        &self,
        request: &CreateCatalogPartyRequest,
    ) -> Result<CatalogParty, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/reference/parties", request)
            .await
    }

    pub async fn list_warehouses(&self) -> Result<Vec<NetworkWarehouse>, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/reference/warehouses/query", &Value::Null)
            .await
    }

    pub async fn inventory_trace(
        &self,
        barcode: &str,
    ) -> Result<InventoryTrace, NetworkClientError> {
        self.authorized_json(
            Method::POST,
            "/v1/inventory/trace",
            &serde_json::json!({ "barcode": barcode }),
        )
        .await
    }

    pub async fn inventory_barcode_exists(
        &self,
        barcode: &str,
    ) -> Result<InventoryBarcodeExistsResponse, NetworkClientError> {
        self.authorized_json(
            Method::POST,
            "/v1/inventory/barcodes/exists",
            &serde_json::json!({ "barcode": barcode }),
        )
        .await
    }

    pub async fn complete_quality_inspection(
        &self,
        request: &NetworkCompleteInspectionRequest,
    ) -> Result<CompleteInspectionResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/quality/inspections", request)
            .await
    }

    pub async fn create_outbound_order(
        &self,
        request: &NetworkCreateOutboundOrderRequest,
    ) -> Result<super::outbound::CreateOutboundOrderResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/outbound/orders", request)
            .await
    }

    pub async fn allocate_outbound_order(
        &self,
        request: &NetworkAllocateOutboundRequest,
    ) -> Result<super::outbound::AllocateOutboundResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/outbound/allocations", request)
            .await
    }

    pub async fn ship_outbound_order(
        &self,
        request: &NetworkShipOutboundRequest,
    ) -> Result<super::outbound::ShipOutboundResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/outbound/shipments", request)
            .await
    }

    pub async fn confirm_outbound_delivery(
        &self,
        request: &NetworkConfirmOutboundDeliveryRequest,
    ) -> Result<super::outbound::ConfirmOutboundDeliveryResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/outbound/deliveries", request)
            .await
    }

    pub async fn return_outbound_shipment(
        &self,
        request: &NetworkReturnOutboundShipmentRequest,
    ) -> Result<super::outbound::ReturnOutboundShipmentResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/outbound/returns", request)
            .await
    }

    pub async fn list_inventory(
        &self,
        query: &super::application::InventoryListQuery,
    ) -> Result<InventoryListResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/inventory/query", query)
            .await
    }

    pub async fn dashboard(
        &self,
        query: &super::application::InventorySummaryQuery,
    ) -> Result<InventorySummaryResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/inventory/summary", query)
            .await
    }

    pub async fn import_upgrade_package(
        &self,
        request: &NetworkUpgradeImportRequest,
    ) -> Result<NetworkUpgradeImportResponse, NetworkClientError> {
        self.authorized_json_with_timeout(
            Method::POST,
            "/v1/upgrades/offline-imports",
            request,
            Duration::from_secs(5 * 60),
        )
        .await
    }

    pub async fn list_tenant_users(
        &self,
        request: &ListTenantUsersRequest,
    ) -> Result<ListTenantUsersResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/admin/users/query", request)
            .await
    }

    pub async fn create_tenant_user(
        &self,
        request: &CreateTenantUserRequest,
    ) -> Result<CreateTenantUserResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/admin/users", request)
            .await
    }

    pub async fn disable_tenant_user(
        &self,
        request: &DisableTenantUserRequest,
    ) -> Result<DisableTenantUserResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/admin/users/disable", request)
            .await
    }

    pub async fn list_tenant_roles(&self) -> Result<Vec<TenantRoleSummary>, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/admin/roles/query", &Value::Null)
            .await
    }

    pub async fn replace_membership_roles(
        &self,
        request: &ReplaceMembershipRolesRequest,
    ) -> Result<MembershipPermissionsResponse, NetworkClientError> {
        self.authorized_json(Method::POST, "/v1/admin/memberships/roles", request)
            .await
    }

    pub async fn membership_effective_permissions(
        &self,
        request: &MembershipPermissionsRequest,
    ) -> Result<MembershipPermissionsResponse, NetworkClientError> {
        self.authorized_json(
            Method::POST,
            "/v1/admin/memberships/permissions/query",
            request,
        )
        .await
    }

    async fn public_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T, NetworkClientError> {
        self.request_json(method, path, serde_json::to_value(body)?, None)
            .await
    }

    async fn authorized_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T, NetworkClientError> {
        self.authorized_json_with_timeout(method, path, body, Duration::from_secs(30))
            .await
    }

    async fn authorized_json_with_timeout<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<T, NetworkClientError> {
        let session = self.session()?.ok_or(NetworkClientError::Unauthenticated)?;
        let body = serde_json::to_value(body)?;
        match self
            .request_json_with_timeout(method.clone(), path, body.clone(), Some(&session), timeout)
            .await
        {
            Err(NetworkClientError::Api { status: 401, .. }) => {
                // Keep the serialized request, including its idempotency key,
                // byte-for-byte identical across the one allowed retry.
                let refreshed = self.refresh().await?;
                self.request_json_with_timeout(method, path, body, Some(&refreshed), timeout)
                    .await
            }
            result => result,
        }
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Value,
        session: Option<&NetworkSession>,
    ) -> Result<T, NetworkClientError> {
        self.request_json_with_timeout(method, path, body, session, Duration::from_secs(30))
            .await
    }

    async fn request_json_with_timeout<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Value,
        session: Option<&NetworkSession>,
        timeout: Duration,
    ) -> Result<T, NetworkClientError> {
        let url = self.endpoint(path)?;
        let mut request = self.http.request(method, url).timeout(timeout).json(&body);
        if let Some(session) = session {
            request = request
                .bearer_auth(&session.session_token)
                .header("x-tenant-id", session.tenant_id.to_string());
        }
        let response = request.send().await?;
        let status = response.status();
        let payload = response.text().await?;
        if payload.len() > 2 * 1024 * 1024 {
            return Err(NetworkClientError::ResponseTooLarge);
        }
        if !status.is_success() {
            return Err(NetworkClientError::Api {
                status: status.as_u16(),
                body: parse_error_body(&payload),
            });
        }
        if status == StatusCode::NO_CONTENT || payload.trim().is_empty() {
            return serde_json::from_value(Value::Null).map_err(Into::into);
        }
        Ok(serde_json::from_str(&payload)?)
    }

    fn endpoint(&self, path: &str) -> Result<String, NetworkClientError> {
        let base = self.base_url()?.ok_or(NetworkClientError::NotConfigured)?;
        let suffix = path.trim_start_matches('/');
        Ok(format!("{base}/{suffix}"))
    }

    fn store_session(&self, session: NetworkSession) -> Result<(), NetworkClientError> {
        *self
            .session
            .lock()
            .map_err(|error| NetworkClientError::State(error.to_string()))? = Some(session);
        Ok(())
    }
}

fn parse_error_body(payload: &str) -> String {
    serde_json::from_str::<ErrorResponse>(payload)
        .ok()
        .and_then(|response| response.error.or(response.message))
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| payload.chars().take(500).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Json;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::Router;

    #[test]
    fn configure_keeps_path_prefix_and_removes_only_trailing_slashes() {
        let client = NetworkClient::default();
        client
            .configure("https://inventory.example/api///".to_owned())
            .unwrap();
        assert_eq!(
            client.endpoint("/v1/auth/login").unwrap(),
            "https://inventory.example/api/v1/auth/login"
        );
    }

    #[test]
    fn configure_rejects_non_http_schemes() {
        let client = NetworkClient::default();
        assert!(matches!(
            client.configure("postgres://localhost/db".to_owned()),
            Err(NetworkClientError::InvalidUrl(_))
        ));
    }

    #[test]
    fn configure_rejects_credentials_query_and_fragment() {
        let client = NetworkClient::default();
        for address in [
            "https://user:secret@inventory.example",
            "https://inventory.example?tenant=other",
            "https://inventory.example#fragment",
        ] {
            assert!(
                matches!(
                    client.configure(address.to_owned()),
                    Err(NetworkClientError::InvalidUrl(_))
                ),
                "accepted unsafe endpoint {address}"
            );
        }
    }

    #[tokio::test]
    async fn inventory_barcode_exists_uses_dedicated_authorized_endpoint() {
        async fn lookup(headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
            assert_eq!(
                headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer test-session-token")
            );
            assert_eq!(
                headers
                    .get("x-tenant-id")
                    .and_then(|value| value.to_str().ok()),
                Some("00000000-0000-0000-0000-000000000000")
            );
            assert_eq!(body, serde_json::json!({ "barcode": "  sn-001\r\n" }));
            Json(serde_json::json!({
                "barcode": "SN-001",
                "exists": true
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test API");
        let address = listener.local_addr().expect("test API address");
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/inventory/barcodes/exists", post(lookup)),
            )
            .await
            .expect("serve test API");
        });

        let client = NetworkClient::default();
        client
            .configure(format!("http://{address}"))
            .expect("configure local test API");
        client
            .store_session(NetworkSession {
                tenant_id: Uuid::nil(),
                user_id: Uuid::now_v7(),
                membership_id: Uuid::now_v7(),
                session_id: Uuid::now_v7(),
                session_token: "test-session-token".to_owned(),
                refresh_token: "test-refresh-token".to_owned(),
                session_ttl_seconds: 300,
                refresh_ttl_seconds: 3_600,
            })
            .expect("store test session");

        assert_eq!(
            client
                .inventory_barcode_exists("  sn-001\r\n")
                .await
                .expect("lookup barcode through network client"),
            InventoryBarcodeExistsResponse {
                barcode: "SN-001".to_owned(),
                exists: true,
            }
        );

        server.abort();
        let _ = server.await;
    }
}
