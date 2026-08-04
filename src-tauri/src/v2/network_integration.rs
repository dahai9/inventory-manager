use super::application::{InventoryListQuery, InventorySummaryQuery};
use super::auth::{AuthError, PasswordService};
use super::network::{
    LoginRequest, NetworkPostReceiptRequest, NetworkService, RefreshRequest,
    PERMISSION_NETWORK_ACCESS, PERMISSION_RECEIPT_WRITE,
};
use super::postgres::{NetworkDatabase, NetworkDatabaseConfig};
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
    for (id, code, description) in [
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
        sqlx::query(
            "INSERT INTO permissions (tenant_id, id, code, description) VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(code)
        .bind(description)
        .execute(&mut *setup)
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
    let request_id = Uuid::now_v7().to_string();
    let request = NetworkPostReceiptRequest {
        request_id: request_id.clone(),
        idempotency_key: format!("receipt:{request_id}"),
        receipt_no: format!("RK-{request_id}"),
        owner_name: "Owner A".to_owned(),
        sku_code: "SKU-X".to_owned(),
        sku_name: "Model X".to_owned(),
        warehouse_id,
        source_reference: None,
        received_at: "2026-08-03T01:00:00Z".to_owned(),
        barcodes: vec![format!("SN-{request_id}")],
        notes: None,
    };
    let first = service
        .post_receipt(tenant_id, &login.session_token, request.clone())
        .await
        .expect("post receipt");
    let replay = service
        .post_receipt(tenant_id, &login.session_token, request)
        .await
        .expect("replay receipt");
    assert_eq!(first.received_count, 1);
    assert!(replay.idempotent_replay);
    assert_eq!(first.receipt_id, replay.receipt_id);

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
                sku_code: "SKU-X".to_owned(),
                sku_name: "Model X".to_owned(),
                warehouse_id,
                source_reference: None,
                received_at: "2026-08-03T01:00:00Z".to_owned(),
                barcodes: vec![format!("SN-after-logout-{}", Uuid::now_v7())],
                notes: None,
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
