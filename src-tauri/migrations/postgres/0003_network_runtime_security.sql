-- Network runtime hardening and cross-document inventory constraints.

-- Version 1 deliberately supports one business workspace per tenant. This
-- makes the offline-to-network target unambiguous until workspace_id is added
-- to every network business table in a later schema version.
CREATE UNIQUE INDEX workspaces_one_per_tenant_idx ON workspaces (tenant_id);

ALTER TABLE audit_logs
    ADD COLUMN membership_id uuid,
    ADD COLUMN device_id uuid,
    ADD COLUMN session_id uuid,
    ADD COLUMN request_ip inet,
    ADD CONSTRAINT audit_logs_membership_fk
        FOREIGN KEY (tenant_id, membership_id)
        REFERENCES memberships (tenant_id, id) ON DELETE RESTRICT,
    ADD CONSTRAINT audit_logs_device_fk
        FOREIGN KEY (tenant_id, device_id)
        REFERENCES devices (tenant_id, id) ON DELETE RESTRICT,
    ADD CONSTRAINT audit_logs_session_fk
        FOREIGN KEY (tenant_id, session_id)
        REFERENCES sessions (tenant_id, id) ON DELETE RESTRICT;

ALTER TABLE license_entitlements
    ADD COLUMN key_id text,
    ADD COLUMN claims_hash text,
    ADD COLUMN verified_at timestamptz,
    ADD COLUMN grace_until timestamptz,
    ADD CONSTRAINT license_entitlements_claims_hash_check CHECK (
        claims_hash IS NULL OR (
            length(claims_hash) = 64
            AND claims_hash ~ '^[0-9a-f]+$'
        )
    ),
    ADD CONSTRAINT license_entitlements_verification_check CHECK (
        verified_at IS NULL OR (
            key_id IS NOT NULL
            AND length(btrim(key_id)) > 0
            AND claims_hash IS NOT NULL
        )
    );

ALTER TABLE migration_packages
    ADD COLUMN migration_id text,
    ADD COLUMN source_instance_id uuid,
    ADD COLUMN source_workspace_id uuid,
    ADD COLUMN actor_id uuid;

CREATE UNIQUE INDEX migration_packages_migration_id_idx
    ON migration_packages (tenant_id, migration_id)
    WHERE migration_id IS NOT NULL;

-- A shipment line is active only while the physical unit remains shipped or
-- delivered. A returned line stays immutable and a new allocation/shipment
-- may later be created after quarantine and retest.
ALTER TABLE outbound_shipment_lines
    DROP CONSTRAINT outbound_shipment_lines_tenant_id_inventory_unit_id_key,
    ADD COLUMN status text NOT NULL DEFAULT 'shipped'
        CHECK (status IN ('shipped', 'delivered', 'returned'));

CREATE UNIQUE INDEX outbound_shipment_lines_one_active_unit_idx
    ON outbound_shipment_lines (tenant_id, inventory_unit_id)
    WHERE status IN ('shipped', 'delivered');

CREATE OR REPLACE FUNCTION app.reject_append_only_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME
        USING ERRCODE = '55000';
END
$$;

CREATE TRIGGER stock_movements_append_only
BEFORE UPDATE OR DELETE ON stock_movements
FOR EACH ROW EXECUTE FUNCTION app.reject_append_only_mutation();

CREATE TRIGGER audit_logs_append_only
BEFORE UPDATE OR DELETE ON audit_logs
FOR EACH ROW EXECUTE FUNCTION app.reject_append_only_mutation();

REVOKE UPDATE, DELETE ON stock_movements FROM PUBLIC;
REVOKE UPDATE, DELETE ON audit_logs FROM PUBLIC;

CREATE OR REPLACE FUNCTION app.validate_shipment_line_links()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    allocation_unit uuid;
    allocation_order uuid;
    shipment_order uuid;
BEGIN
    SELECT a.inventory_unit_id, l.outbound_order_id
      INTO allocation_unit, allocation_order
      FROM outbound_allocations a
      JOIN outbound_order_lines l
        ON l.tenant_id = a.tenant_id
       AND l.id = a.outbound_order_line_id
     WHERE a.tenant_id = NEW.tenant_id
       AND a.id = NEW.outbound_allocation_id;

    SELECT outbound_order_id INTO shipment_order
      FROM outbound_shipments
     WHERE tenant_id = NEW.tenant_id
       AND id = NEW.outbound_shipment_id;

    IF allocation_unit IS NULL
       OR allocation_unit <> NEW.inventory_unit_id
       OR allocation_order IS NULL
       OR allocation_order <> shipment_order THEN
        RAISE EXCEPTION 'shipment line allocation, unit and order do not match'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER outbound_shipment_lines_validate_links
BEFORE INSERT OR UPDATE OF outbound_shipment_id, outbound_allocation_id, inventory_unit_id
ON outbound_shipment_lines
FOR EACH ROW EXECUTE FUNCTION app.validate_shipment_line_links();

CREATE OR REPLACE FUNCTION app.validate_delivery_line_links()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    confirmation_shipment uuid;
    line_shipment uuid;
BEGIN
    SELECT outbound_shipment_id INTO confirmation_shipment
      FROM delivery_confirmations
     WHERE tenant_id = NEW.tenant_id
       AND id = NEW.delivery_confirmation_id;
    SELECT outbound_shipment_id INTO line_shipment
      FROM outbound_shipment_lines
     WHERE tenant_id = NEW.tenant_id
       AND id = NEW.outbound_shipment_line_id;
    IF confirmation_shipment IS NULL
       OR line_shipment IS NULL
       OR confirmation_shipment <> line_shipment THEN
        RAISE EXCEPTION 'delivery line does not belong to confirmation shipment'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER delivery_confirmation_lines_validate_links
BEFORE INSERT OR UPDATE OF delivery_confirmation_id, outbound_shipment_line_id
ON delivery_confirmation_lines
FOR EACH ROW EXECUTE FUNCTION app.validate_delivery_line_links();

CREATE OR REPLACE FUNCTION app.mark_shipment_line_delivered()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.result = 'accepted' THEN
        UPDATE outbound_shipment_lines
           SET status = 'delivered'
         WHERE tenant_id = NEW.tenant_id
           AND id = NEW.outbound_shipment_line_id
           AND status = 'shipped';
        IF NOT FOUND THEN
            RAISE EXCEPTION 'shipment line is not pending delivery'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER delivery_confirmation_lines_update_status
AFTER INSERT ON delivery_confirmation_lines
FOR EACH ROW EXECUTE FUNCTION app.mark_shipment_line_delivered();

CREATE OR REPLACE FUNCTION app.validate_return_line_links()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    shipment_unit uuid;
BEGIN
    SELECT inventory_unit_id INTO shipment_unit
      FROM outbound_shipment_lines
     WHERE tenant_id = NEW.tenant_id
       AND id = NEW.outbound_shipment_line_id;
    IF shipment_unit IS NULL OR shipment_unit <> NEW.inventory_unit_id THEN
        RAISE EXCEPTION 'return line unit does not match original shipment line'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER outbound_return_lines_validate_links
BEFORE INSERT OR UPDATE OF outbound_shipment_line_id, inventory_unit_id
ON outbound_return_lines
FOR EACH ROW EXECUTE FUNCTION app.validate_return_line_links();

CREATE OR REPLACE FUNCTION app.mark_shipment_line_returned()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    UPDATE outbound_shipment_lines
       SET status = 'returned'
     WHERE tenant_id = NEW.tenant_id
       AND id = NEW.outbound_shipment_line_id
       AND status IN ('shipped', 'delivered');
    IF NOT FOUND THEN
        RAISE EXCEPTION 'shipment line is not returnable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER outbound_return_lines_update_status
AFTER INSERT ON outbound_return_lines
FOR EACH ROW EXECUTE FUNCTION app.mark_shipment_line_returned();
