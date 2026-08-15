CREATE TABLE document_voids (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    document_kind text NOT NULL CHECK (document_kind IN ('inbound_receipt', 'outbound_order')),
    inbound_receipt_id uuid,
    outbound_order_id uuid,
    reason text NOT NULL CHECK (length(btrim(reason)) > 0),
    actor_id uuid NOT NULL,
    source_actor_id text CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0),
    voided_at timestamptz NOT NULL,
    request_id text NOT NULL CHECK (length(btrim(request_id)) > 0),
    idempotency_key text NOT NULL CHECK (length(btrim(idempotency_key)) > 0),
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id),
    FOREIGN KEY (tenant_id, inbound_receipt_id)
        REFERENCES inbound_receipts (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, outbound_order_id)
        REFERENCES outbound_orders (tenant_id, id) ON DELETE RESTRICT,
    CHECK (
        (document_kind = 'inbound_receipt' AND inbound_receipt_id IS NOT NULL AND outbound_order_id IS NULL)
        OR
        (document_kind = 'outbound_order' AND outbound_order_id IS NOT NULL AND inbound_receipt_id IS NULL)
    ),
    UNIQUE (tenant_id, document_kind, idempotency_key)
);

CREATE UNIQUE INDEX document_voids_receipt_once_idx
    ON document_voids (tenant_id, inbound_receipt_id)
    WHERE inbound_receipt_id IS NOT NULL;

CREATE UNIQUE INDEX document_voids_order_once_idx
    ON document_voids (tenant_id, outbound_order_id)
    WHERE outbound_order_id IS NOT NULL;

CREATE INDEX document_voids_tenant_time_idx
    ON document_voids (tenant_id, voided_at DESC, id);

ALTER TABLE document_voids ENABLE ROW LEVEL SECURITY;
ALTER TABLE document_voids FORCE ROW LEVEL SECURITY;
CREATE POLICY document_voids_current_context ON document_voids
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

CREATE TRIGGER document_voids_append_only
BEFORE UPDATE OR DELETE ON document_voids
FOR EACH ROW EXECUTE FUNCTION app.reject_append_only_mutation();

REVOKE UPDATE, DELETE ON document_voids FROM PUBLIC;

CREATE OR REPLACE FUNCTION app.seed_document_void_catalog(target_tenant_id uuid)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public, app
AS $$
DECLARE
    previous_tenant text;
    permission_id uuid;
BEGIN
    previous_tenant := current_setting('app.tenant_id', true);
    PERFORM set_config('app.tenant_id', target_tenant_id::text, true);

    permission_id := (
        substr(md5(target_tenant_id::text || ':permission:inventory.document.void'), 1, 8) || '-' ||
        substr(md5(target_tenant_id::text || ':permission:inventory.document.void'), 9, 4) || '-' ||
        substr(md5(target_tenant_id::text || ':permission:inventory.document.void'), 13, 4) || '-' ||
        substr(md5(target_tenant_id::text || ':permission:inventory.document.void'), 17, 4) || '-' ||
        substr(md5(target_tenant_id::text || ':permission:inventory.document.void'), 21, 12)
    )::uuid;

    INSERT INTO permissions (tenant_id, id, code, description)
    VALUES (target_tenant_id, permission_id, 'inventory.document.void',
            'Void inbound receipts and outbound orders after password confirmation')
    ON CONFLICT (tenant_id, code) DO UPDATE SET description = EXCLUDED.description;

    INSERT INTO role_permissions (tenant_id, role_id, permission_id)
    SELECT target_tenant_id, r.id, permission_id
      FROM roles r
     WHERE r.tenant_id = target_tenant_id
       AND r.code IN ('tenant_admin', 'warehouse_supervisor')
    ON CONFLICT DO NOTHING;

    PERFORM set_config('app.tenant_id', COALESCE(previous_tenant, ''), true);
EXCEPTION WHEN OTHERS THEN
    PERFORM set_config('app.tenant_id', COALESCE(previous_tenant, ''), true);
    RAISE;
END
$$;

REVOKE ALL ON FUNCTION app.seed_document_void_catalog(uuid) FROM PUBLIC;

ALTER TABLE tenants NO FORCE ROW LEVEL SECURITY;

DO $$
DECLARE
    existing_tenant_id uuid;
BEGIN
    FOR existing_tenant_id IN SELECT id FROM tenants ORDER BY id LOOP
        PERFORM app.seed_document_void_catalog(existing_tenant_id);
    END LOOP;
END
$$;

ALTER TABLE tenants FORCE ROW LEVEL SECURITY;

CREATE OR REPLACE FUNCTION app.seed_new_tenant_identity_catalog()
RETURNS trigger
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public, app
AS $$
BEGIN
    PERFORM app.seed_tenant_identity_catalog(NEW.id);
    PERFORM app.seed_document_void_catalog(NEW.id);
    RETURN NEW;
END
$$;

REVOKE ALL ON FUNCTION app.seed_new_tenant_identity_catalog() FROM PUBLIC;
