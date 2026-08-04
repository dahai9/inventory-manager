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

GRANT SELECT, INSERT, UPDATE, DELETE
    ON ALL TABLES IN SCHEMA public
    TO :"runtime_role";

-- Business facts and security audit are append-only. Trigger protection is
-- still present as a second line of defense for privileged maintenance paths.
REVOKE UPDATE, DELETE ON TABLE public.stock_movements FROM :"runtime_role";
REVOKE UPDATE, DELETE ON TABLE public.audit_logs FROM :"runtime_role";

-- UUID primary keys mean the current schema does not use sequences, but this
-- default keeps future migration-created identity columns deployable without
-- widening table ownership.
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO :"runtime_role";

ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO :"runtime_role";
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO :"runtime_role";

COMMIT;

SELECT rolname, rolsuper, rolcreatedb, rolcreaterole, rolinherit, rolbypassrls
  FROM pg_roles
 WHERE rolname = :'runtime_role';
