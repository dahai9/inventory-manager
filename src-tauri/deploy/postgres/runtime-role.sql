-- Apply after all PostgreSQL migrations with the schema owner account.
-- Usage:
--   psql "$INVENTORY_MIGRATION_DATABASE_URL" \
--     -v ON_ERROR_STOP=1 -v runtime_role=inventory_runtime \
--     -f src-tauri/deploy/postgres/runtime-role.sql
--
-- The role must already exist as LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE
-- NOINHERIT NOBYPASSRLS. Password creation stays in the deployment secret
-- manager and is intentionally not represented in this repository.

BEGIN;

SELECT format(
    'GRANT CONNECT ON DATABASE %I TO %I',
    current_database(),
    :'runtime_role'
) \gexec
GRANT USAGE ON SCHEMA public, app TO :"runtime_role";

GRANT SELECT
    ON ALL TABLES IN SCHEMA public
    TO :"runtime_role";

REVOKE INSERT, UPDATE, DELETE
    ON ALL TABLES IN SCHEMA public
    FROM :"runtime_role";

GRANT INSERT, UPDATE
    ON TABLE public.workspaces,
             public.business_parties,
             public.party_roles,
             public.skus,
             public.warehouses,
             public.locations,
             public.inbound_receipts,
             public.inbound_receipt_lines,
             public.inventory_units,
             public.quality_labels,
             public.quality_inspections,
             public.quality_inspection_results,
             public.quality_waivers,
             public.outbound_orders,
             public.outbound_order_lines,
             public.outbound_allocations,
             public.outbound_shipments,
             public.outbound_shipment_lines,
             public.delivery_confirmations,
             public.delivery_confirmation_lines,
             public.outbound_return_batches,
             public.outbound_return_lines,
             public.idempotency_records
    TO :"runtime_role";

GRANT INSERT
    ON TABLE public.stock_movements,
             public.audit_logs,
             public.quality_label_name_history,
             public.migration_packages
    TO :"runtime_role";

GRANT UPDATE (created_at)
    ON TABLE public.migration_packages
    TO :"runtime_role";

GRANT INSERT, UPDATE
    ON TABLE public.users,
             public.credentials,
             public.memberships,
             public.sessions,
             public.refresh_tokens
    TO :"runtime_role";

GRANT INSERT, DELETE
    ON TABLE public.membership_roles
    TO :"runtime_role";

-- Authorization takes row-level FOR SHARE locks on these catalogs. PostgreSQL
-- requires UPDATE privilege on at least one column for that lock mode, so keep
-- it limited to non-authoritative creation metadata.
GRANT UPDATE (created_at)
    ON TABLE public.tenants,
             public.roles,
             public.permissions,
             public.role_permissions
    TO :"runtime_role";

GRANT UPDATE (assigned_at)
    ON TABLE public.membership_roles
    TO :"runtime_role";

GRANT INSERT, UPDATE (last_seen_at)
    ON TABLE public.devices
    TO :"runtime_role";

GRANT UPDATE (issued_at)
    ON TABLE public.license_entitlements
    TO :"runtime_role";

-- UUID primary keys mean the current schema does not use sequences, but this
-- default keeps future migration-created identity columns deployable without
-- widening table ownership.
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO :"runtime_role";

ALTER DEFAULT PRIVILEGES IN SCHEMA public
    REVOKE INSERT, UPDATE, DELETE ON TABLES FROM :"runtime_role";
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT ON TABLES TO :"runtime_role";
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO :"runtime_role";

COMMIT;

SELECT rolname, rolsuper, rolcreatedb, rolcreaterole, rolinherit, rolbypassrls
  FROM pg_roles
 WHERE rolname = :'runtime_role';
