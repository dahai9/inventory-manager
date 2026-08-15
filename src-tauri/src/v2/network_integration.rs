use super::application::{
    CatalogPartyRole, CreateCatalogPartyRequest, CreateCatalogProductRequest, InventoryListQuery,
    InventorySummaryQuery,
};
use super::auth::{hash_token, AuthError, AuthorizationDenial, PasswordService, TokenKind};
use super::network::{
    LoginRequest, NetworkPostReceiptRequest, NetworkService, NetworkServiceError, RefreshRequest,
    PERMISSION_NETWORK_ACCESS, PERMISSION_RECEIPT_WRITE,
};
use super::postgres::{NetworkDatabase, NetworkDatabaseConfig, NetworkDatabaseError};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires INVENTORY_NETWORK_TEST_ADMIN_URL and INVENTORY_NETWORK_TEST_RUNTIME_URL"]
async fn restricted_postgres_role_can_login_and_post_an_idempotent_receipt() {
    let admin_url =
        std::env::var("INVENTORY_NETWORK_TEST_ADMIN_URL").expect("network test admin URL");
    let runtime_url =
        std::env::var("INVENTORY_NETWORK_TEST_RUNTIME_URL").expect("network test runtime URL");
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&admin_url)
        .await
        .expect("connect admin database");
    let tenant_id = Uuid::now_v7();
    let workspace_id = Uuid::now_v7();
    let warehouse_id = Uuid::now_v7();
    let location_id = Uuid::now_v7();
    let user_id = Uuid::now_v7();
    let membership_id = Uuid::now_v7();
    let role_id = Uuid::now_v7();
    let access_permission_id = Uuid::now_v7();
    let receipt_permission_id = Uuid::now_v7();
    let device_id = Uuid::now_v7();
    let password = "network-test-password";
    let password_hash = PasswordService::recommended()
        .expect("password service")
        .hash_password(password)
        .expect("password hash");

    let mut setup = admin.begin().await.expect("begin setup");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *setup)
        .await
        .expect("set tenant context");
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, 'Network Test Tenant')")
        .bind(tenant_id)
        .bind(format!("test-{}", tenant_id.simple()))
        .execute(&mut *setup)
        .await
        .expect("insert tenant");
    sqlx::query("INSERT INTO workspaces (tenant_id, id, name, source_instance_id) VALUES ($1, $2, 'Network Test', $3)")
        .bind(tenant_id).bind(workspace_id).bind(Uuid::now_v7()).execute(&mut *setup).await.expect("insert workspace");
    sqlx::query(
        "INSERT INTO warehouses (tenant_id, id, code, name) VALUES ($1, $2, 'DEFAULT', 'Default')",
    )
    .bind(tenant_id)
    .bind(warehouse_id)
    .execute(&mut *setup)
    .await
    .expect("insert warehouse");
    sqlx::query("INSERT INTO locations (tenant_id, id, warehouse_id, code, name, kind) VALUES ($1, $2, $3, 'RECEIVING', 'Receiving', 'receiving')")
        .bind(tenant_id).bind(location_id).bind(warehouse_id).execute(&mut *setup).await.expect("insert location");
    sqlx::query("INSERT INTO users (tenant_id, id, login, normalized_login, display_name) VALUES ($1, $2, 'operator', 'operator', 'Operator')")
        .bind(tenant_id).bind(user_id).execute(&mut *setup).await.expect("insert user");
    sqlx::query("INSERT INTO credentials (tenant_id, user_id, password_hash) VALUES ($1, $2, $3)")
        .bind(tenant_id)
        .bind(user_id)
        .bind(password_hash)
        .execute(&mut *setup)
        .await
        .expect("insert credential");
    sqlx::query("INSERT INTO memberships (tenant_id, id, user_id) VALUES ($1, $2, $3)")
        .bind(tenant_id)
        .bind(membership_id)
        .bind(user_id)
        .execute(&mut *setup)
        .await
        .expect("insert membership");
    sqlx::query(
        "INSERT INTO roles (tenant_id, id, code, name) VALUES ($1, $2, 'operator', 'Operator')",
    )
    .bind(tenant_id)
    .bind(role_id)
    .execute(&mut *setup)
    .await
    .expect("insert role");
    for (proposed_id, code, description) in [
        (
            access_permission_id,
            PERMISSION_NETWORK_ACCESS,
            "Network access",
        ),
        (
            receipt_permission_id,
            PERMISSION_RECEIPT_WRITE,
            "Receipt write",
        ),
    ] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO permissions (tenant_id, id, code, description) VALUES ($1, $2, $3, $4) ON CONFLICT (tenant_id, code) DO UPDATE SET description = EXCLUDED.description RETURNING id",
        )
        .bind(tenant_id)
        .bind(proposed_id)
        .bind(code)
        .bind(description)
        .fetch_one(&mut *setup)
        .await
        .expect("insert permission");
        sqlx::query(
            "INSERT INTO role_permissions (tenant_id, role_id, permission_id) VALUES ($1, $2, $3)",
        )
        .bind(tenant_id)
        .bind(role_id)
        .bind(id)
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
    sqlx::query("INSERT INTO license_entitlements (tenant_id, id, license_id, edition, status, seat_limit, starts_at, expires_at, issuer, signature, key_id, claims_hash, verified_at) VALUES ($1, $2, $3, 'network', 'active', 5, CURRENT_TIMESTAMP - INTERVAL '1 hour', CURRENT_TIMESTAMP + INTERVAL '1 day', 'integration-test', 'test-signature', 'test-key', $4, CURRENT_TIMESTAMP)")
        .bind(tenant_id).bind(Uuid::now_v7()).bind(format!("TEST-{tenant_id}")).bind("a".repeat(64)).execute(&mut *setup).await.expect("insert entitlement");
    setup.commit().await.expect("commit setup");

    let database = NetworkDatabase::connect(&NetworkDatabaseConfig::new(runtime_url))
        .await
        .expect("restricted runtime database");
    let service = NetworkService::new(database).expect("network service");
    let login = service
        .login(LoginRequest {
            tenant_id,
            login: "operator".to_owned(),
            password: password.to_owned(),
            device_id,
        })
        .await
        .expect("login");
    let owner = service
        .create_catalog_party(
            tenant_id,
            &login.session_token,
            CreateCatalogPartyRequest {
                display_name: "Owner A".to_owned(),
                role: CatalogPartyRole::GoodsOwner,
            },
        )
        .await
        .expect("create receipt owner");
    let supplier = service
        .create_catalog_party(
            tenant_id,
            &login.session_token,
            CreateCatalogPartyRequest {
                display_name: "Supplier A".to_owned(),
                role: CatalogPartyRole::Supplier,
            },
        )
        .await
        .expect("create receipt supplier");
    let product = service
        .create_catalog_product(
            tenant_id,
            &login.session_token,
            CreateCatalogProductRequest {
                code: "SKU-X".to_owned(),
                name: "Model X".to_owned(),
                serial_prefix: None,
                serial_forbidden_chars: String::new(),
            },
        )
        .await
        .expect("create receipt product");
    let request_id = Uuid::now_v7().to_string();
    let request = NetworkPostReceiptRequest {
        request_id: request_id.clone(),
        idempotency_key: format!("receipt:{request_id}"),
        receipt_no: format!("RK-{request_id}"),
        owner_name: "Owner A".to_owned(),
        supplier_name: "Supplier A".to_owned(),
        sku_code: "SKU-X".to_owned(),
        sku_name: "Model X".to_owned(),
        warehouse_id,
        source_reference: None,
        received_at: "2026-08-03T01:00:00Z".to_owned(),
        barcodes: vec![format!("SN-{request_id}")],
        notes: None,
        warranty: None,
    };
    let receipt_barcode = request.barcodes[0].clone();
    let mut mismatched_name = request.clone();
    mismatched_name.request_id = Uuid::now_v7().to_string();
    mismatched_name.idempotency_key = format!("receipt:{}", mismatched_name.request_id);
    mismatched_name.receipt_no = format!("RK-{}", mismatched_name.request_id);
    mismatched_name.sku_name = "Model X typo".to_owned();
    mismatched_name.barcodes = vec![format!("SN-{}", mismatched_name.request_id)];
    let mismatch = service
        .post_receipt(tenant_id, &login.session_token, mismatched_name)
        .await
        .expect_err("receipt product name must match the catalog");
    assert!(matches!(
        mismatch,
        NetworkServiceError::Invalid(message) if message.contains("does not match catalog")
    ));
    let first = service
        .post_receipt(tenant_id, &login.session_token, request.clone())
        .await
        .expect("post receipt");
    let replay = service
        .post_receipt(tenant_id, &login.session_token, request)
        .await
        .expect("replay receipt");
    assert_eq!(first.received_count, 1);
    assert_eq!(first.owner_party_id, owner.party_id);
    assert_eq!(
        first.supplier_party_id.as_deref(),
        Some(supplier.party_id.as_str())
    );
    assert_eq!(first.sku_id, product.sku_id);
    assert!(replay.idempotent_replay);
    assert_eq!(first.receipt_id, replay.receipt_id);

    let existing_barcode = service
        .inventory_barcode_exists(
            tenant_id,
            &login.session_token,
            &format!("  {}\r\n", receipt_barcode.to_lowercase()),
        )
        .await
        .expect("find exact network inventory barcode");
    assert_eq!(existing_barcode.barcode, receipt_barcode.to_uppercase());
    assert!(existing_barcode.exists);
    let similar_missing_barcode = service
        .inventory_barcode_exists(
            tenant_id,
            &login.session_token,
            &format!("{receipt_barcode}-EXTRA"),
        )
        .await
        .expect("reject fuzzy network inventory barcode match");
    assert!(!similar_missing_barcode.exists);

    let inventory = service
        .list_inventory(
            tenant_id,
            &login.session_token,
            InventoryListQuery {
                search: Some("sku-x".to_owned()),
                ..InventoryListQuery::default()
            },
        )
        .await
        .expect("query network inventory");
    assert_eq!(inventory.total, 1);
    assert_eq!(inventory.items[0].receipt_id, first.receipt_id);
    assert_eq!(inventory.items[0].quality_status.to_string(), "untested");
    let summary = service
        .inventory_summary(
            tenant_id,
            &login.session_token,
            InventorySummaryQuery::default(),
        )
        .await
        .expect("query network inventory summary");
    assert_eq!(summary.total_units, 1);
    assert_eq!(summary.inventory.received, 1);
    assert_eq!(summary.quality.untested, 1);
    assert_eq!(summary.products.len(), 1);
    assert_eq!(summary.products[0].sku_code, "SKU-X");
    assert_eq!(summary.products[0].on_hand_units, 1);
    assert_eq!(summary.products[0].suppliers.len(), 1);
    assert_eq!(summary.products[0].suppliers[0].supplier_name, "Supplier A");
    assert_eq!(summary.products[0].suppliers[0].on_hand_units, 1);
    let cross_tenant = service
        .inventory_summary(
            Uuid::now_v7(),
            &login.session_token,
            InventorySummaryQuery::default(),
        )
        .await
        .expect_err("session token must not authorize another tenant selector");
    assert!(matches!(
        cross_tenant,
        super::network::NetworkServiceError::Database(_)
    ));

    let old_refresh_token = login.refresh_token.clone();
    let refreshed = service
        .refresh(RefreshRequest {
            tenant_id,
            refresh_token: old_refresh_token.clone(),
        })
        .await
        .expect("rotate refresh token");
    assert_ne!(refreshed.session_token, login.session_token);
    assert_ne!(refreshed.refresh_token, old_refresh_token);
    let old_refresh_error = service
        .refresh(RefreshRequest {
            tenant_id,
            refresh_token: old_refresh_token,
        })
        .await
        .expect_err("refresh token must be single use");
    assert!(matches!(
        old_refresh_error,
        super::network::NetworkServiceError::Auth(AuthError::InvalidRefreshToken)
    ));
    service
        .logout(tenant_id, &refreshed.session_token)
        .await
        .expect("logout");
    let logout_error = service
        .post_receipt(
            tenant_id,
            &refreshed.session_token,
            NetworkPostReceiptRequest {
                request_id: Uuid::now_v7().to_string(),
                idempotency_key: format!("after-logout:{}", Uuid::now_v7()),
                receipt_no: format!("AFTER-LOGOUT-{}", Uuid::now_v7()),
                owner_name: "Owner A".to_owned(),
                supplier_name: "Supplier A".to_owned(),
                sku_code: "SKU-X".to_owned(),
                sku_name: "Model X".to_owned(),
                warehouse_id,
                source_reference: None,
                received_at: "2026-08-03T01:00:00Z".to_owned(),
                barcodes: vec![format!("SN-after-logout-{}", Uuid::now_v7())],
                notes: None,
                warranty: None,
            },
        )
        .await
        .expect_err("logged out session must be rejected");
    assert!(matches!(
        logout_error,
        super::network::NetworkServiceError::Database(_)
    ));

    let mut verification = admin.begin().await.expect("begin verification");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *verification)
        .await
        .expect("set verification tenant");
    let facts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM inventory_units WHERE tenant_id = $1), (SELECT count(*) FROM stock_movements WHERE tenant_id = $1), (SELECT count(*) FROM audit_logs WHERE tenant_id = $1 AND actor_id = $2)",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_one(&mut *verification)
    .await
    .expect("verify facts");
    assert_eq!(facts, (1, 1, 1));
    verification.commit().await.expect("commit verification");
    admin.close().await;
}

#[tokio::test]
#[ignore = "requires INVENTORY_NETWORK_TEST_ADMIN_URL and INVENTORY_NETWORK_TEST_RUNTIME_URL"]
async fn disabled_principal_invalidates_an_existing_access_session_immediately() {
    let admin_url =
        std::env::var("INVENTORY_NETWORK_TEST_ADMIN_URL").expect("network test admin URL");
    let runtime_url =
        std::env::var("INVENTORY_NETWORK_TEST_RUNTIME_URL").expect("network test runtime URL");
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&admin_url)
        .await
        .expect("connect admin database");

    let tenant_id = Uuid::now_v7();
    let user_id = Uuid::now_v7();
    let membership_id = Uuid::now_v7();
    let role_id = Uuid::now_v7();
    let access_permission_id = Uuid::now_v7();
    let device_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let session_token = format!("disable-session-{session_id}");

    let mut setup = admin.begin().await.expect("begin disabled-principal setup");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *setup)
        .await
        .expect("set setup tenant context");
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, 'Disable Session Tenant')")
        .bind(tenant_id)
        .bind(format!("disable-session-{}", tenant_id.simple()))
        .execute(&mut *setup)
        .await
        .expect("insert tenant");
    sqlx::query("INSERT INTO users (tenant_id, id, login, normalized_login, display_name) VALUES ($1, $2, 'disable-operator', 'disable-operator', 'Disable Operator')")
        .bind(tenant_id)
        .bind(user_id)
        .execute(&mut *setup)
        .await
        .expect("insert user");
    sqlx::query("INSERT INTO memberships (tenant_id, id, user_id) VALUES ($1, $2, $3)")
        .bind(tenant_id)
        .bind(membership_id)
        .bind(user_id)
        .execute(&mut *setup)
        .await
        .expect("insert membership");
    sqlx::query("INSERT INTO roles (tenant_id, id, code, name) VALUES ($1, $2, 'disable-operator', 'Disable Operator')")
        .bind(tenant_id)
        .bind(role_id)
        .execute(&mut *setup)
        .await
        .expect("insert role");
    for (proposed_id, code) in [(access_permission_id, PERMISSION_NETWORK_ACCESS)] {
        let permission_id: Uuid = sqlx::query_scalar(
            "INSERT INTO permissions (tenant_id, id, code, description) VALUES ($1, $2, $3, $3) ON CONFLICT (tenant_id, code) DO UPDATE SET description = EXCLUDED.description RETURNING id",
        )
        .bind(tenant_id)
        .bind(proposed_id)
        .bind(code)
        .fetch_one(&mut *setup)
        .await
        .expect("insert permission");
        sqlx::query(
            "INSERT INTO role_permissions (tenant_id, role_id, permission_id) VALUES ($1, $2, $3)",
        )
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
    sqlx::query("INSERT INTO devices (tenant_id, id, membership_id, user_id, device_fingerprint, display_name) VALUES ($1, $2, $3, $4, $5, 'Disable Session Device')")
        .bind(tenant_id)
        .bind(device_id)
        .bind(membership_id)
        .bind(user_id)
        .bind(format!("disable-device-{device_id}"))
        .execute(&mut *setup)
        .await
        .expect("insert device");
    sqlx::query("INSERT INTO license_entitlements (tenant_id, id, license_id, edition, status, seat_limit, starts_at, expires_at, issuer, signature, key_id, claims_hash, verified_at) VALUES ($1, $2, $3, 'network', 'active', 5, CURRENT_TIMESTAMP - INTERVAL '1 hour', CURRENT_TIMESTAMP + INTERVAL '1 day', 'integration-test', 'test-signature', 'test-key', $4, CURRENT_TIMESTAMP)")
        .bind(tenant_id)
        .bind(Uuid::now_v7())
        .bind(format!("DISABLE-{tenant_id}"))
        .bind("d".repeat(64))
        .execute(&mut *setup)
        .await
        .expect("insert entitlement");
    sqlx::query("INSERT INTO sessions (tenant_id, id, membership_id, user_id, device_id, token_hash, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP + INTERVAL '1 hour')")
        .bind(tenant_id)
        .bind(session_id)
        .bind(membership_id)
        .bind(user_id)
        .bind(device_id)
        .bind(hash_token(TokenKind::Session, &session_token).as_slice())
        .execute(&mut *setup)
        .await
        .expect("insert session");
    setup
        .commit()
        .await
        .expect("commit disabled-principal setup");

    let database = NetworkDatabase::connect(&NetworkDatabaseConfig::new(runtime_url))
        .await
        .expect("connect restricted runtime database");
    let service = NetworkService::new(database).expect("network service");
    service
        .inventory_summary(tenant_id, &session_token, InventorySummaryQuery::default())
        .await
        .expect("session works before principal is disabled");

    sqlx::query("UPDATE memberships SET status = 'suspended' WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(membership_id)
        .execute(&admin)
        .await
        .expect("suspend membership");
    let membership_error = service
        .inventory_summary(tenant_id, &session_token, InventorySummaryQuery::default())
        .await
        .expect_err("old access token must fail after membership suspension");
    assert!(matches!(
        membership_error,
        NetworkServiceError::Database(NetworkDatabaseError::Authorization(
            AuthError::AccessDenied(AuthorizationDenial::MembershipInactive)
        ))
    ));

    sqlx::query("UPDATE memberships SET status = 'active' WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(membership_id)
        .execute(&admin)
        .await
        .expect("reactivate membership");
    service
        .inventory_summary(tenant_id, &session_token, InventorySummaryQuery::default())
        .await
        .expect("same session works after explicit membership reactivation");

    sqlx::query("UPDATE users SET status = 'disabled' WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(user_id)
        .execute(&admin)
        .await
        .expect("disable user");
    let account_error = service
        .inventory_summary(tenant_id, &session_token, InventorySummaryQuery::default())
        .await
        .expect_err("old access token must fail after account disable");
    assert!(matches!(
        account_error,
        NetworkServiceError::Database(NetworkDatabaseError::Authorization(
            AuthError::AccessDenied(AuthorizationDenial::AccountDisabled)
        ))
    ));

    admin.close().await;
}

#[tokio::test]
#[ignore = "requires INVENTORY_NETWORK_TEST_ADMIN_URL and INVENTORY_NETWORK_TEST_RUNTIME_URL"]
async fn restricted_postgres_role_cannot_cross_tenant_rls_for_direct_sql() {
    let admin_url =
        std::env::var("INVENTORY_NETWORK_TEST_ADMIN_URL").expect("network test admin URL");
    let runtime_url =
        std::env::var("INVENTORY_NETWORK_TEST_RUNTIME_URL").expect("network test runtime URL");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect admin database");
    let runtime = PgPoolOptions::new()
        .max_connections(1)
        .connect(&runtime_url)
        .await
        .expect("connect restricted runtime database");
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let workspace_a = Uuid::now_v7();
    let workspace_b = Uuid::now_v7();

    for (tenant_id, workspace_id, suffix) in
        [(tenant_a, workspace_a, "a"), (tenant_b, workspace_b, "b")]
    {
        sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $3)")
            .bind(tenant_id)
            .bind(format!("rls-{suffix}-{}", tenant_id.simple()))
            .bind(format!("RLS tenant {suffix}"))
            .execute(&admin)
            .await
            .expect("insert RLS tenant");
        sqlx::query(
            "INSERT INTO workspaces (tenant_id, id, name, source_instance_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(format!("RLS workspace {suffix}"))
        .bind(Uuid::now_v7())
        .execute(&admin)
        .await
        .expect("insert RLS workspace");
    }

    let mut read = runtime.begin().await.expect("begin cross-tenant read");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_a.to_string())
        .execute(&mut *read)
        .await
        .expect("set tenant A context");
    let visible_b: i64 = sqlx::query_scalar("SELECT count(*) FROM workspaces WHERE tenant_id = $1")
        .bind(tenant_b)
        .fetch_one(&mut *read)
        .await
        .expect("query tenant B from tenant A context");
    assert_eq!(visible_b, 0, "tenant B rows must be invisible to tenant A");
    read.rollback().await.expect("rollback read check");

    let mut insert = runtime.begin().await.expect("begin cross-tenant insert");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_a.to_string())
        .execute(&mut *insert)
        .await
        .expect("set tenant A context");
    let insert_error = sqlx::query(
        "INSERT INTO warehouses (tenant_id, id, code, name) VALUES ($1, $2, 'CROSS', 'Cross tenant')",
    )
    .bind(tenant_b)
    .bind(Uuid::now_v7())
    .execute(&mut *insert)
    .await
    .expect_err("tenant A context must not insert tenant B rows");
    assert!(
        insert_error
            .as_database_error()
            .is_some_and(|error| error.message().contains("row-level security")),
        "expected an RLS policy error, got {insert_error}"
    );
    insert
        .rollback()
        .await
        .expect("rollback rejected insert transaction");

    let mut update = runtime.begin().await.expect("begin cross-tenant update");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_a.to_string())
        .execute(&mut *update)
        .await
        .expect("set tenant A context");
    let updated = sqlx::query(
        "UPDATE workspaces SET name = 'cross-tenant mutation' WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_b)
    .bind(workspace_b)
    .execute(&mut *update)
    .await
    .expect("cross-tenant update is filtered by RLS");
    assert_eq!(
        updated.rows_affected(),
        0,
        "tenant A context must not update tenant B rows"
    );
    update.rollback().await.expect("rollback update check");

    runtime.close().await;
    admin.close().await;
}
