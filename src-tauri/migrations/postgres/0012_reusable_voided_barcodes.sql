-- A voided inbound unit remains immutable history, but its barcode may be
-- received again as a new inventory unit. Only current (non-voided) units
-- participate in barcode uniqueness.

ALTER TABLE inventory_units
    DROP CONSTRAINT inventory_units_tenant_id_barcode_key;

CREATE UNIQUE INDEX inventory_units_active_barcode_idx
    ON inventory_units (tenant_id, barcode)
    WHERE inventory_status <> 'voided';
