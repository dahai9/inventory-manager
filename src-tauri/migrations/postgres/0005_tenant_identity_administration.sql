-- Tenant identity administration catalog.
--
-- Platform provisioning remains outside tenant sessions and uses the
-- migration/admin connection. Tenant administration APIs operate only after
-- a bearer session establishes app.tenant_id and app.actor_user_id.

CREATE OR REPLACE FUNCTION app.seed_tenant_identity_catalog(target_tenant_id uuid)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public, app
AS $$
DECLARE
    previous_tenant text;
    tenant_admin_role_id uuid;
BEGIN
    previous_tenant := current_setting('app.tenant_id', true);
    PERFORM set_config('app.tenant_id', target_tenant_id::text, true);

    INSERT INTO permissions (tenant_id, id, code, description)
    SELECT target_tenant_id,
           (
               substr(md5(target_tenant_id::text || ':permission:' || p.code), 1, 8) || '-' ||
               substr(md5(target_tenant_id::text || ':permission:' || p.code), 9, 4) || '-' ||
               substr(md5(target_tenant_id::text || ':permission:' || p.code), 13, 4) || '-' ||
               substr(md5(target_tenant_id::text || ':permission:' || p.code), 17, 4) || '-' ||
               substr(md5(target_tenant_id::text || ':permission:' || p.code), 21, 12)
           )::uuid,
           p.code,
           p.description
      FROM (VALUES
          ('inventory.access', 'Read network inventory and reference data'),
          ('inventory.receipt.write', 'Create and post inbound receipts'),
          ('inventory.quality.write', 'Complete initial and repeat quality inspections'),
          ('inventory.order.write', 'Create upstream outbound orders'),
          ('inventory.allocation.write', 'Allocate inventory to outbound orders'),
          ('inventory.shipment.write', 'Post outbound shipments'),
          ('inventory.delivery.write', 'Confirm upstream delivery'),
          ('inventory.return.write', 'Register outbound returns'),
          ('inventory.upgrade.import', 'Import a one-time offline upgrade package'),
          ('identity.users.read', 'List tenant users and memberships'),
          ('identity.users.write', 'Create and disable tenant users'),
          ('identity.memberships.write', 'Assign tenant membership roles'),
          ('identity.permissions.read', 'View roles and effective permissions')
      ) AS p(code, description)
    ON CONFLICT (tenant_id, code) DO NOTHING;

    tenant_admin_role_id := (
        substr(md5(target_tenant_id::text || ':role:tenant_admin'), 1, 8) || '-' ||
        substr(md5(target_tenant_id::text || ':role:tenant_admin'), 9, 4) || '-' ||
        substr(md5(target_tenant_id::text || ':role:tenant_admin'), 13, 4) || '-' ||
        substr(md5(target_tenant_id::text || ':role:tenant_admin'), 17, 4) || '-' ||
        substr(md5(target_tenant_id::text || ':role:tenant_admin'), 21, 12)
    )::uuid;
    INSERT INTO roles
        (tenant_id, id, code, name, description, active, system_role)
    VALUES
        (target_tenant_id, tenant_admin_role_id, 'tenant_admin',
         'Tenant administrator',
         'Tenant-scoped user, membership, role and permission administration',
         true, true)
    ON CONFLICT (tenant_id, code) DO UPDATE SET
        name = EXCLUDED.name,
        description = EXCLUDED.description,
        active = true,
        system_role = true
    RETURNING id INTO tenant_admin_role_id;

    INSERT INTO role_permissions (tenant_id, role_id, permission_id)
    SELECT target_tenant_id, tenant_admin_role_id, p.id
      FROM permissions p
     WHERE p.tenant_id = target_tenant_id
       AND p.code IN (
           'inventory.access',
           'inventory.upgrade.import',
           'identity.users.read',
           'identity.users.write',
           'identity.memberships.write',
           'identity.permissions.read'
       )
    ON CONFLICT DO NOTHING;

    INSERT INTO roles
        (tenant_id, id, code, name, description, active, system_role)
    SELECT target_tenant_id,
           (
               substr(md5(target_tenant_id::text || ':role:' || r.code), 1, 8) || '-' ||
               substr(md5(target_tenant_id::text || ':role:' || r.code), 9, 4) || '-' ||
               substr(md5(target_tenant_id::text || ':role:' || r.code), 13, 4) || '-' ||
               substr(md5(target_tenant_id::text || ':role:' || r.code), 17, 4) || '-' ||
               substr(md5(target_tenant_id::text || ':role:' || r.code), 21, 12)
           )::uuid,
           r.code,
           r.name,
           r.description,
           true,
           true
      FROM (VALUES
          ('inbound_operator', 'Inbound operator',
           'Receive stock and view network inventory'),
          ('quality_inspector', 'Quality inspector',
           'Record initial and repeat inspections'),
          ('outbound_operator', 'Outbound operator',
           'Create, allocate, ship, deliver and return outbound orders'),
          ('warehouse_supervisor', 'Warehouse supervisor',
           'Perform all standard inbound, quality and outbound operations')
      ) AS r(code, name, description)
    ON CONFLICT (tenant_id, code) DO UPDATE SET
        name = EXCLUDED.name,
        description = EXCLUDED.description,
        active = true,
        system_role = true;

    INSERT INTO role_permissions (tenant_id, role_id, permission_id)
    SELECT target_tenant_id, r.id, p.id
      FROM (VALUES
          ('inbound_operator', 'inventory.access'),
          ('inbound_operator', 'inventory.receipt.write'),
          ('quality_inspector', 'inventory.access'),
          ('quality_inspector', 'inventory.quality.write'),
          ('outbound_operator', 'inventory.access'),
          ('outbound_operator', 'inventory.order.write'),
          ('outbound_operator', 'inventory.allocation.write'),
          ('outbound_operator', 'inventory.shipment.write'),
          ('outbound_operator', 'inventory.delivery.write'),
          ('outbound_operator', 'inventory.return.write'),
          ('warehouse_supervisor', 'inventory.access'),
          ('warehouse_supervisor', 'inventory.receipt.write'),
          ('warehouse_supervisor', 'inventory.quality.write'),
          ('warehouse_supervisor', 'inventory.order.write'),
          ('warehouse_supervisor', 'inventory.allocation.write'),
          ('warehouse_supervisor', 'inventory.shipment.write'),
          ('warehouse_supervisor', 'inventory.delivery.write'),
          ('warehouse_supervisor', 'inventory.return.write')
      ) AS grant_map(role_code, permission_code)
      JOIN roles r
        ON r.tenant_id = target_tenant_id AND r.code = grant_map.role_code
      JOIN permissions p
        ON p.tenant_id = target_tenant_id AND p.code = grant_map.permission_code
    ON CONFLICT DO NOTHING;

    PERFORM set_config('app.tenant_id', COALESCE(previous_tenant, ''), true);
EXCEPTION WHEN OTHERS THEN
    PERFORM set_config('app.tenant_id', COALESCE(previous_tenant, ''), true);
    RAISE;
END
$$;

REVOKE ALL ON FUNCTION app.seed_tenant_identity_catalog(uuid) FROM PUBLIC;

-- FORCE RLS also applies to a non-superuser table owner. Temporarily restore
-- the normal owner bypass only for enumerating tenant ids, then seed every
-- catalog under that tenant's own RLS context.
ALTER TABLE tenants NO FORCE ROW LEVEL SECURITY;

DO $$
DECLARE
    existing_tenant_id uuid;
BEGIN
    FOR existing_tenant_id IN SELECT id FROM tenants ORDER BY id LOOP
        PERFORM app.seed_tenant_identity_catalog(existing_tenant_id);
    END LOOP;
END
$$;

ALTER TABLE tenants FORCE ROW LEVEL SECURITY;

CREATE INDEX users_tenant_updated_idx
    ON users (tenant_id, updated_at DESC, id);

CREATE INDEX memberships_tenant_status_idx
    ON memberships (tenant_id, status, updated_at DESC, id);

-- Future tenants receive the same catalog at provisioning time. The function
-- temporarily establishes the NEW tenant as RLS context, restores the caller's
-- context before returning, and is not directly executable by the runtime
-- role. Assigning the first membership to tenant_admin remains an explicit
-- platform-provisioning action; a tenant session can never bootstrap itself.
CREATE OR REPLACE FUNCTION app.seed_new_tenant_identity_catalog()
RETURNS trigger
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public, app
AS $$
BEGIN
    PERFORM app.seed_tenant_identity_catalog(NEW.id);
    RETURN NEW;
END
$$;

REVOKE ALL ON FUNCTION app.seed_new_tenant_identity_catalog() FROM PUBLIC;

CREATE TRIGGER tenants_seed_identity_catalog
AFTER INSERT ON tenants
FOR EACH ROW EXECUTE FUNCTION app.seed_new_tenant_identity_catalog();
