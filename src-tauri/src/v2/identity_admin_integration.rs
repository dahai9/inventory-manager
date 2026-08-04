use super::application::InventorySummaryQuery;
use super::auth::{AuthError, AuthorizationDenial, PasswordService};
use super::identity_admin::{
    CreateTenantUserRequest, DisableTenantUserRequest, ListTenantUsersRequest,
    MembershipPermissionsRequest, ReplaceMembershipRolesRequest,
};
use super::network::{
    LoginRequest, NetworkService, NetworkServiceError, RefreshRequest, PERMISSION_NETWORK_ACCESS,
};
use super::postgres::{NetworkDatabase, NetworkDatabaseConfig, NetworkDatabaseError};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires INVENTORY_NETWORK_TEST_ADMIN_URL and INVENTORY_NETWORK_TEST_RUNTIME_URL"]
async fn tenant_administrator_workflow_is_audited_and_tenant_scoped() {
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
    let other_tenant_id = Uuid::now_v7();
    for (id, suffix) in [(tenant_id, "primary"), (other_tenant_id, "other")] {
        sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(format!("identity-{suffix}-{}", id.simple()))
            .bind(format!("Identity {suffix} tenant"))
            .execute(&admin)
            .await
            .expect("provision tenant");
    }

    let actor_user_id = Uuid::now_v7();
    let actor_membership_id = Uuid::now_v7();
    let delegated_user_id = Uuid::now_v7();
    let delegated_membership_id = Uuid::now_v7();
    let operator_role_id = Uuid::now_v7();
    let delegated_role_id = Uuid::now_v7();
    let actor_password = "identity-actor-password";
    let delegated_password = "identity-delegated-password";
    let passwords = PasswordService::recommended().expect("password service");

    let mut setup = admin.begin().await.expect("begin identity setup");
    set_tenant_context(&mut setup, tenant_id).await;
    let tenant_admin_role_id: Uuid =
        sqlx::query_scalar("SELECT id FROM roles WHERE tenant_id = $1 AND code = 'tenant_admin'")
            .bind(tenant_id)
            .fetch_one(&mut *setup)
            .await
            .expect("migration seeded tenant administrator role");
    let permission_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM permissions WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&mut *setup)
            .await
            .expect("count seeded permissions");
    assert_eq!(permission_count, 13);

    for (role_id, code, name) in [
        (operator_role_id, "operator", "Operator"),
        (
            delegated_role_id,
            "delegated_identity_admin",
            "Delegated identity administrator",
        ),
    ] {
        sqlx::query("INSERT INTO roles (tenant_id, id, code, name) VALUES ($1, $2, $3, $4)")
            .bind(tenant_id)
            .bind(role_id)
            .bind(code)
            .bind(name)
            .execute(&mut *setup)
            .await
            .expect("insert test role");
    }
    sqlx::query(
        r#"
        INSERT INTO role_permissions (tenant_id, role_id, permission_id)
        SELECT $1, $2, id
          FROM permissions
         WHERE tenant_id = $1 AND code = $3
        "#,
    )
    .bind(tenant_id)
    .bind(operator_role_id)
    .bind(PERMISSION_NETWORK_ACCESS)
    .execute(&mut *setup)
    .await
    .expect("grant operator network access");
    sqlx::query(
        r#"
        INSERT INTO role_permissions (tenant_id, role_id, permission_id)
        SELECT $1, $2, id FROM permissions WHERE tenant_id = $1
        "#,
    )
    .bind(tenant_id)
    .bind(delegated_role_id)
    .execute(&mut *setup)
    .await
    .expect("grant delegated identity permissions");

    insert_principal(
        &mut setup,
        tenant_id,
        actor_user_id,
        actor_membership_id,
        "identity-owner",
        &passwords.hash_password(actor_password).expect("actor hash"),
        tenant_admin_role_id,
    )
    .await;
    insert_principal(
        &mut setup,
        tenant_id,
        delegated_user_id,
        delegated_membership_id,
        "identity-delegated",
        &passwords
            .hash_password(delegated_password)
            .expect("delegated hash"),
        delegated_role_id,
    )
    .await;
    sqlx::query(
        r#"
        INSERT INTO license_entitlements
            (tenant_id, id, license_id, edition, status, seat_limit,
             starts_at, expires_at, issuer, signature, key_id, claims_hash,
             verified_at)
        VALUES
            ($1, $2, $3, 'network', 'active', 3,
             CURRENT_TIMESTAMP - INTERVAL '1 hour',
             CURRENT_TIMESTAMP + INTERVAL '1 day',
             'integration-test', 'test-signature', 'test-key', $4,
             CURRENT_TIMESTAMP)
        "#,
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .bind(format!("IDENTITY-{tenant_id}"))
    .bind("e".repeat(64))
    .execute(&mut *setup)
    .await
    .expect("insert entitlement");
    setup.commit().await.expect("commit identity setup");

    let runtime_probe = PgPoolOptions::new()
        .max_connections(1)
        .connect(&runtime_url)
        .await
        .expect("connect runtime probe");
    let mut probe = runtime_probe.begin().await.expect("begin runtime probe");
    set_tenant_context(&mut probe, tenant_id).await;
    let seed_executable: bool = sqlx::query_scalar(
        "SELECT has_function_privilege(current_user, 'app.seed_tenant_identity_catalog(uuid)', 'EXECUTE')",
    )
    .fetch_one(&mut *probe)
    .await
    .expect("check catalog seed privilege");
    assert!(!seed_executable);
    let license_mutation_privileges: (bool, bool, bool) = sqlx::query_as(
        r#"
        SELECT has_table_privilege(current_user, 'public.license_entitlements', 'INSERT'),
               has_table_privilege(current_user, 'public.license_entitlements', 'UPDATE'),
               has_table_privilege(current_user, 'public.license_entitlements', 'DELETE')
        "#,
    )
    .fetch_one(&mut *probe)
    .await
    .expect("check license catalog privileges");
    assert_eq!(license_mutation_privileges, (false, false, false));
    let role_mutation = sqlx::query(
        "INSERT INTO roles (tenant_id, id, code, name) VALUES ($1, $2, 'forbidden', 'Forbidden')",
    )
    .bind(tenant_id)
    .bind(Uuid::now_v7())
    .execute(&mut *probe)
    .await
    .expect_err("runtime role must not mutate the authorization catalog");
    assert!(role_mutation
        .as_database_error()
        .and_then(|error| error.code())
        .is_some_and(|code| code == "42501"));
    probe.rollback().await.expect("rollback runtime probe");
    runtime_probe.close().await;

    let database = NetworkDatabase::connect(&NetworkDatabaseConfig::new(runtime_url))
        .await
        .expect("connect restricted runtime database");
    let service = NetworkService::new(database).expect("network service");
    let actor_device_id = Uuid::now_v7();
    let actor = service
        .login(LoginRequest {
            tenant_id,
            login: "identity-owner".to_owned(),
            password: actor_password.to_owned(),
            device_id: actor_device_id,
        })
        .await
        .expect("tenant administrator login");
    let delegated = service
        .login(LoginRequest {
            tenant_id,
            login: "identity-delegated".to_owned(),
            password: delegated_password.to_owned(),
            device_id: Uuid::now_v7(),
        })
        .await
        .expect("delegated user login");

    let role_catalog = service
        .list_tenant_roles(tenant_id, &actor.session_token)
        .await
        .expect("list tenant roles");
    for (code, expected_permissions) in [
        (
            "tenant_admin",
            vec![
                "identity.memberships.write",
                "identity.permissions.read",
                "identity.users.read",
                "identity.users.write",
                "inventory.access",
                "inventory.upgrade.import",
            ],
        ),
        (
            "inbound_operator",
            vec!["inventory.access", "inventory.receipt.write"],
        ),
        (
            "quality_inspector",
            vec!["inventory.access", "inventory.quality.write"],
        ),
        (
            "outbound_operator",
            vec![
                "inventory.access",
                "inventory.allocation.write",
                "inventory.delivery.write",
                "inventory.order.write",
                "inventory.return.write",
                "inventory.shipment.write",
            ],
        ),
        (
            "warehouse_supervisor",
            vec![
                "inventory.access",
                "inventory.allocation.write",
                "inventory.delivery.write",
                "inventory.order.write",
                "inventory.quality.write",
                "inventory.receipt.write",
                "inventory.return.write",
                "inventory.shipment.write",
            ],
        ),
    ] {
        let role = role_catalog
            .iter()
            .find(|role| role.code == code)
            .unwrap_or_else(|| panic!("migration must seed {code}"));
        assert_eq!(
            role.permission_codes,
            expected_permissions
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
    }

    let self_disable_error = service
        .disable_tenant_user(
            tenant_id,
            &actor.session_token,
            DisableTenantUserRequest {
                request_id: format!("identity-self-disable-{}", Uuid::now_v7()),
                user_id: actor.user_id,
            },
        )
        .await
        .expect_err("the current session must not disable its own user");
    assert!(matches!(
        self_disable_error,
        NetworkServiceError::Conflict { ref entity, ref key }
            if entity == "user" && key == "current_session"
    ));

    let create_a_request_id = format!("identity-create-a-{}", Uuid::now_v7());
    let create_b_request_id = format!("identity-create-b-{}", Uuid::now_v7());
    let create_a_request = CreateTenantUserRequest {
        request_id: create_a_request_id.clone(),
        login: format!("created-a-{}", Uuid::now_v7().simple()),
        display_name: "Created administrator A".to_owned(),
        email: None,
        password: "created-user-password".to_owned(),
        role_ids: vec![tenant_admin_role_id],
    };
    let create_b_request = CreateTenantUserRequest {
        request_id: create_b_request_id.clone(),
        login: format!("created-b-{}", Uuid::now_v7().simple()),
        display_name: "Created administrator B".to_owned(),
        email: None,
        password: "created-user-password".to_owned(),
        role_ids: vec![tenant_admin_role_id],
    };
    let create_a =
        service.create_tenant_user(tenant_id, &actor.session_token, create_a_request.clone());
    let create_b =
        service.create_tenant_user(tenant_id, &actor.session_token, create_b_request.clone());
    let (create_a_result, create_b_result) = tokio::join!(create_a, create_b);
    let (created, create_request, seat_error) = match (create_a_result, create_b_result) {
        (Ok(created), Err(error)) => (created, create_a_request, error),
        (Err(error), Ok(created)) => (created, create_b_request, error),
        results => panic!("exactly one concurrent user creation must succeed: {results:?}"),
    };
    let create_request_id = create_request.request_id.clone();
    assert_eq!(created.user.role_codes, vec!["tenant_admin"]);
    assert!(matches!(
        seat_error,
        NetworkServiceError::Conflict { ref entity, .. } if entity == "license_seat"
    ));
    let mut conflicting_create_replay = create_request.clone();
    conflicting_create_replay.password = "created-user-passw0rd".to_owned();
    let create_replay = service
        .create_tenant_user(tenant_id, &actor.session_token, create_request)
        .await
        .expect("replay create tenant user");
    assert_eq!(create_replay.user.user_id, created.user.user_id);
    let conflicting_replay_error = service
        .create_tenant_user(tenant_id, &actor.session_token, conflicting_create_replay)
        .await
        .expect_err("same idempotency key with another password must conflict");
    assert!(matches!(
        conflicting_replay_error,
        NetworkServiceError::Conflict { ref entity, .. } if entity == "idempotency_key"
    ));

    let users = service
        .list_tenant_users(
            tenant_id,
            &actor.session_token,
            ListTenantUsersRequest::default(),
        )
        .await
        .expect("list tenant users");
    assert_eq!(users.users.len(), 3);
    assert!(users
        .users
        .iter()
        .any(|user| user.user_id == created.user.user_id));

    let initial_permissions = service
        .membership_effective_permissions(
            tenant_id,
            &actor.session_token,
            MembershipPermissionsRequest {
                membership_id: created.user.membership_id,
            },
        )
        .await
        .expect("read effective permissions");
    assert!(initial_permissions
        .permission_codes
        .iter()
        .any(|permission| permission == "identity.users.write"));

    let peer_admin_user_id = Uuid::now_v7();
    let peer_admin_membership_id = Uuid::now_v7();
    let peer_admin_login = format!("peer-admin-{}", Uuid::now_v7().simple());
    let peer_admin_password = "peer-admin-password";
    let mut peer_setup = admin.begin().await.expect("begin peer administrator setup");
    set_tenant_context(&mut peer_setup, tenant_id).await;
    insert_principal(
        &mut peer_setup,
        tenant_id,
        peer_admin_user_id,
        peer_admin_membership_id,
        &peer_admin_login,
        &passwords
            .hash_password(peer_admin_password)
            .expect("peer administrator hash"),
        tenant_admin_role_id,
    )
    .await;
    sqlx::query(
        "UPDATE memberships SET consumes_license_seat = false WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(peer_admin_membership_id)
    .execute(&mut *peer_setup)
    .await
    .expect("make peer administrator non-seat principal");
    peer_setup
        .commit()
        .await
        .expect("commit peer administrator setup");

    let created_admin_session = service
        .login(LoginRequest {
            tenant_id,
            login: created.user.login.clone(),
            password: "created-user-password".to_owned(),
            device_id: Uuid::now_v7(),
        })
        .await
        .expect("created administrator login");
    let peer_admin_session = service
        .login(LoginRequest {
            tenant_id,
            login: peer_admin_login,
            password: peer_admin_password.to_owned(),
            device_id: Uuid::now_v7(),
        })
        .await
        .expect("peer administrator login");
    let created_demotes_peer_request_id = format!("identity-concurrent-a-{}", Uuid::now_v7());
    let peer_demotes_created_request_id = format!("identity-concurrent-b-{}", Uuid::now_v7());
    let created_demotes_peer = service.replace_membership_roles(
        tenant_id,
        &created_admin_session.session_token,
        ReplaceMembershipRolesRequest {
            request_id: created_demotes_peer_request_id.clone(),
            membership_id: peer_admin_membership_id,
            role_ids: vec![operator_role_id],
        },
    );
    let peer_demotes_created = service.replace_membership_roles(
        tenant_id,
        &peer_admin_session.session_token,
        ReplaceMembershipRolesRequest {
            request_id: peer_demotes_created_request_id.clone(),
            membership_id: created.user.membership_id,
            role_ids: vec![operator_role_id],
        },
    );
    let (created_result, peer_result) = tokio::join!(created_demotes_peer, peer_demotes_created);
    let (concurrent_request_id, concurrent_denial) = match (created_result, peer_result) {
        (Ok(_), Err(error)) => (created_demotes_peer_request_id, error),
        (Err(error), Ok(_)) => (peer_demotes_created_request_id, error),
        results => {
            panic!("exactly one concurrent administrator mutation must succeed: {results:?}")
        }
    };
    assert!(matches!(
        concurrent_denial,
        NetworkServiceError::Database(NetworkDatabaseError::Authorization(
            AuthError::AccessDenied(AuthorizationDenial::MissingPermission)
        ))
    ));

    let replace_request_id = format!("identity-replace-{}", Uuid::now_v7());
    let replace_request = ReplaceMembershipRolesRequest {
        request_id: replace_request_id.clone(),
        membership_id: created.user.membership_id,
        role_ids: vec![operator_role_id],
    };
    let replaced = service
        .replace_membership_roles(tenant_id, &actor.session_token, replace_request.clone())
        .await
        .expect("replace membership roles");
    assert_eq!(replaced.role_codes, vec!["operator"]);
    assert_eq!(
        replaced.permission_codes,
        vec![PERMISSION_NETWORK_ACCESS.to_owned()]
    );
    let replace_replay = service
        .replace_membership_roles(tenant_id, &actor.session_token, replace_request)
        .await
        .expect("replay membership role replacement");
    assert_eq!(replace_replay.role_codes, replaced.role_codes);
    assert_eq!(replace_replay.permission_codes, replaced.permission_codes);

    let replace_peer_request_id = format!("identity-replace-peer-{}", Uuid::now_v7());
    let replaced_peer = service
        .replace_membership_roles(
            tenant_id,
            &actor.session_token,
            ReplaceMembershipRolesRequest {
                request_id: replace_peer_request_id.clone(),
                membership_id: peer_admin_membership_id,
                role_ids: vec![operator_role_id],
            },
        )
        .await
        .expect("remove any remaining peer administrator role");
    assert_eq!(replaced_peer.role_codes, vec!["operator"]);

    let delegated_error = service
        .list_tenant_users(
            tenant_id,
            &delegated.session_token,
            ListTenantUsersRequest::default(),
        )
        .await
        .expect_err("named permissions must not replace tenant_admin role");
    assert!(matches!(
        delegated_error,
        NetworkServiceError::Auth(AuthError::AccessDenied(
            AuthorizationDenial::MissingPermission
        ))
    ));

    let last_admin_error = service
        .replace_membership_roles(
            tenant_id,
            &actor.session_token,
            ReplaceMembershipRolesRequest {
                request_id: format!("identity-last-admin-{}", Uuid::now_v7()),
                membership_id: actor.membership_id,
                role_ids: vec![operator_role_id],
            },
        )
        .await
        .expect_err("last active tenant administrator must be protected");
    assert!(matches!(
        last_admin_error,
        NetworkServiceError::Conflict { ref entity, ref key }
            if entity == "tenant_admin" && key == "last_active_administrator"
    ));

    let cross_tenant_error = service
        .list_tenant_users(
            other_tenant_id,
            &actor.session_token,
            ListTenantUsersRequest::default(),
        )
        .await
        .expect_err("tenant selector must not move an authenticated session");
    assert!(matches!(
        cross_tenant_error,
        NetworkServiceError::Database(NetworkDatabaseError::Authorization(
            AuthError::InvalidSession
        ))
    ));

    service
        .inventory_summary(
            tenant_id,
            &created_admin_session.session_token,
            InventorySummaryQuery::default(),
        )
        .await
        .expect("operator session works before disable");

    let disable_request_id = format!("identity-disable-{}", Uuid::now_v7());
    let disable_request = DisableTenantUserRequest {
        request_id: disable_request_id.clone(),
        user_id: created.user.user_id,
    };
    let disabled = service
        .disable_tenant_user(tenant_id, &actor.session_token, disable_request.clone())
        .await
        .expect("disable tenant user");
    assert_eq!(disabled.revoked_session_count, 1);
    assert_eq!(disabled.account_status, "disabled");
    assert_eq!(disabled.membership_status, "suspended");
    let disable_replay = service
        .disable_tenant_user(tenant_id, &actor.session_token, disable_request)
        .await
        .expect("replay disable tenant user");
    assert_eq!(disable_replay.revoked_session_count, 1);

    let revoked_session_error = service
        .inventory_summary(
            tenant_id,
            &created_admin_session.session_token,
            InventorySummaryQuery::default(),
        )
        .await
        .expect_err("disabled user's session must be revoked");
    assert!(matches!(
        revoked_session_error,
        NetworkServiceError::Database(NetworkDatabaseError::Authorization(
            AuthError::InvalidSession
        ))
    ));
    let revoked_refresh_error = service
        .refresh(RefreshRequest {
            tenant_id,
            refresh_token: created_admin_session.refresh_token,
        })
        .await
        .expect_err("disabled user's refresh token must be revoked");
    assert!(matches!(
        revoked_refresh_error,
        NetworkServiceError::Auth(AuthError::InvalidRefreshToken)
    ));

    let disabled_users = service
        .list_tenant_users(
            tenant_id,
            &actor.session_token,
            ListTenantUsersRequest {
                include_disabled: true,
                ..ListTenantUsersRequest::default()
            },
        )
        .await
        .expect("list disabled users");
    let disabled_summary = disabled_users
        .users
        .iter()
        .find(|user| user.user_id == created.user.user_id)
        .expect("disabled user remains queryable");
    assert_eq!(disabled_summary.account_status, "disabled");
    assert_eq!(disabled_summary.membership_status, "suspended");

    let mut verification = admin.begin().await.expect("begin audit verification");
    set_tenant_context(&mut verification, tenant_id).await;
    let audit_count: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
          FROM audit_logs
         WHERE tenant_id = $1
           AND request_id = ANY($2)
           AND actor_id = $3
           AND membership_id = $4
           AND device_id = $5
           AND session_id = $6
           AND result = 'success'
        "#,
    )
    .bind(tenant_id)
    .bind(vec![
        create_request_id,
        replace_request_id,
        replace_peer_request_id,
        disable_request_id,
    ])
    .bind(actor.user_id)
    .bind(actor.membership_id)
    .bind(actor_device_id)
    .bind(actor.session_id)
    .fetch_one(&mut *verification)
    .await
    .expect("verify identity audits");
    assert_eq!(audit_count, 4);
    let concurrent_audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs WHERE tenant_id = $1 AND request_id = $2 AND action = 'identity.membership.roles_replaced' AND result = 'success'",
    )
    .bind(tenant_id)
    .bind(concurrent_request_id)
    .fetch_one(&mut *verification)
    .await
    .expect("verify concurrent administrator audit");
    assert_eq!(concurrent_audit_count, 1);
    verification
        .rollback()
        .await
        .expect("rollback audit verification");
    admin.close().await;
}

async fn set_tenant_context(transaction: &mut Transaction<'_, Postgres>, tenant_id: Uuid) {
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut **transaction)
        .await
        .expect("set tenant context");
}

async fn insert_principal(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    membership_id: Uuid,
    login: &str,
    password_hash: &str,
    role_id: Uuid,
) {
    sqlx::query(
        "INSERT INTO users (tenant_id, id, login, normalized_login, display_name) VALUES ($1, $2, $3, $3, $3)",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(login)
    .execute(&mut **transaction)
    .await
    .expect("insert principal user");
    sqlx::query("INSERT INTO credentials (tenant_id, user_id, password_hash) VALUES ($1, $2, $3)")
        .bind(tenant_id)
        .bind(user_id)
        .bind(password_hash)
        .execute(&mut **transaction)
        .await
        .expect("insert principal credential");
    sqlx::query("INSERT INTO memberships (tenant_id, id, user_id) VALUES ($1, $2, $3)")
        .bind(tenant_id)
        .bind(membership_id)
        .bind(user_id)
        .execute(&mut **transaction)
        .await
        .expect("insert principal membership");
    sqlx::query(
        "INSERT INTO membership_roles (tenant_id, membership_id, role_id) VALUES ($1, $2, $3)",
    )
    .bind(tenant_id)
    .bind(membership_id)
    .bind(role_id)
    .execute(&mut **transaction)
    .await
    .expect("assign principal role");
}
