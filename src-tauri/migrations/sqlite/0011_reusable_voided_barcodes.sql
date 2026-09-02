-- Keep voided inventory rows and all immutable history, while allowing the
-- same physical barcode to be received again as a new active unit. SQLite
-- cannot remove a table-level UNIQUE constraint in place, so rebuild the
-- inventory table and every descendant that references it in one migration
-- transaction. SQLx runs SQLite migrations transactionally.

CREATE TEMP TABLE inventory_units_rebuild_source AS
SELECT * FROM inventory_units;

CREATE TEMP TABLE quality_inspection_results_rebuild_source AS
SELECT * FROM quality_inspection_results;

CREATE TEMP TABLE quality_waivers_rebuild_source AS
SELECT * FROM quality_waivers;

CREATE TEMP TABLE outbound_allocations_rebuild_source AS
SELECT * FROM outbound_allocations;

CREATE TEMP TABLE outbound_shipment_lines_rebuild_source AS
SELECT * FROM outbound_shipment_lines;

CREATE TEMP TABLE delivery_confirmation_lines_rebuild_source AS
SELECT * FROM delivery_confirmation_lines;

CREATE TEMP TABLE outbound_return_lines_rebuild_source AS
SELECT * FROM outbound_return_lines;

CREATE TEMP TABLE stock_movements_rebuild_source AS
SELECT * FROM stock_movements;

CREATE TEMP TABLE legacy_import_rows_rebuild_source AS
SELECT * FROM legacy_import_rows;

-- Drop descendants before their referenced parent. The order also accounts
-- for legacy_import_rows referencing shipment and return history.
DROP TABLE legacy_import_rows;
DROP TABLE delivery_confirmation_lines;
DROP TABLE outbound_return_lines;
DROP TABLE outbound_shipment_lines;
DROP TABLE outbound_allocations;
DROP TABLE quality_inspection_results;
DROP TABLE quality_waivers;
DROP TABLE stock_movements;
DROP TABLE inventory_units;

CREATE TABLE inventory_units (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    barcode TEXT NOT NULL,
    inbound_receipt_line_id TEXT NOT NULL,
    owner_party_id TEXT NOT NULL,
    sku_id TEXT NOT NULL,
    location_id TEXT NOT NULL,
    inventory_status TEXT NOT NULL CHECK (inventory_status IN (
        'received', 'available', 'reserved', 'shipped', 'delivered',
        'quarantined', 'scrapped', 'returned_to_owner', 'voided'
    )),
    quality_status TEXT NOT NULL CHECK (quality_status IN ('untested', 'testing', 'passed', 'failed', 'waived')),
    version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
    received_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (inbound_receipt_line_id) REFERENCES inbound_receipt_lines(id),
    FOREIGN KEY (owner_party_id) REFERENCES business_parties(id),
    FOREIGN KEY (sku_id) REFERENCES skus(id),
    FOREIGN KEY (location_id) REFERENCES locations(id)
) STRICT;

INSERT INTO inventory_units
    (id, workspace_id, barcode, inbound_receipt_line_id, owner_party_id,
     sku_id, location_id, inventory_status, quality_status, version,
     received_at, updated_at)
SELECT id, workspace_id, barcode, inbound_receipt_line_id, owner_party_id,
       sku_id, location_id, inventory_status, quality_status, version,
       received_at, updated_at
  FROM inventory_units_rebuild_source;

CREATE UNIQUE INDEX inventory_units_active_barcode_idx
    ON inventory_units (workspace_id, barcode)
    WHERE inventory_status <> 'voided';
CREATE INDEX inventory_units_owner_received_idx
    ON inventory_units (workspace_id, owner_party_id, received_at DESC);
CREATE INDEX inventory_units_sku_status_idx
    ON inventory_units (workspace_id, sku_id, inventory_status, quality_status);
CREATE INDEX inventory_units_receipt_line_idx
    ON inventory_units (workspace_id, inbound_receipt_line_id, id);

CREATE TABLE quality_inspection_results (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    inspection_id TEXT NOT NULL,
    inventory_unit_id TEXT NOT NULL,
    result TEXT NOT NULL CHECK (result IN ('passed', 'failed')),
    defect_code TEXT,
    measurements_json TEXT NOT NULL DEFAULT '{}',
    notes TEXT,
    created_at TEXT NOT NULL,
    quality_label_id TEXT,
    quality_label_snapshot TEXT,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (inspection_id) REFERENCES quality_inspections(id),
    FOREIGN KEY (inventory_unit_id) REFERENCES inventory_units(id),
    UNIQUE (inspection_id, inventory_unit_id)
) STRICT;

INSERT INTO quality_inspection_results
    (id, workspace_id, inspection_id, inventory_unit_id, result, defect_code,
     measurements_json, notes, created_at, quality_label_id,
     quality_label_snapshot)
SELECT id, workspace_id, inspection_id, inventory_unit_id, result, defect_code,
       measurements_json, notes, created_at, quality_label_id,
       quality_label_snapshot
  FROM quality_inspection_results_rebuild_source;

CREATE INDEX quality_inspection_results_label_idx
    ON quality_inspection_results (workspace_id, quality_label_id);

CREATE TRIGGER quality_inspection_results_label_insert_guard
BEFORE INSERT ON quality_inspection_results
FOR EACH ROW
WHEN (NEW.quality_label_id IS NULL) <> (NEW.quality_label_snapshot IS NULL)
  OR (
      NEW.quality_label_id IS NOT NULL
      AND NOT EXISTS (
          SELECT 1
            FROM quality_labels label
           WHERE label.id = NEW.quality_label_id
             AND label.workspace_id = NEW.workspace_id
      )
  )
BEGIN
    SELECT RAISE(ABORT, 'quality inspection label reference or snapshot is invalid');
END;

CREATE TRIGGER quality_inspection_results_label_update_guard
BEFORE UPDATE OF workspace_id, quality_label_id, quality_label_snapshot
ON quality_inspection_results
FOR EACH ROW
WHEN (NEW.quality_label_id IS NULL) <> (NEW.quality_label_snapshot IS NULL)
  OR (
      NEW.quality_label_id IS NOT NULL
      AND NOT EXISTS (
          SELECT 1
            FROM quality_labels label
           WHERE label.id = NEW.quality_label_id
             AND label.workspace_id = NEW.workspace_id
      )
  )
BEGIN
    SELECT RAISE(ABORT, 'quality inspection label reference or snapshot is invalid');
END;

CREATE TABLE quality_waivers (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    inventory_unit_id TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
    authorized_by TEXT NOT NULL,
    authorized_at TEXT NOT NULL,
    revoked_at TEXT,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (inventory_unit_id) REFERENCES inventory_units(id)
) STRICT;

INSERT INTO quality_waivers
    (id, workspace_id, inventory_unit_id, reason, authorized_by,
     authorized_at, revoked_at)
SELECT id, workspace_id, inventory_unit_id, reason, authorized_by,
       authorized_at, revoked_at
  FROM quality_waivers_rebuild_source;

CREATE TABLE outbound_allocations (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    outbound_order_line_id TEXT NOT NULL,
    inventory_unit_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'released', 'shipped', 'voided')),
    allocated_by TEXT NOT NULL,
    allocated_at TEXT NOT NULL,
    released_at TEXT,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (outbound_order_line_id) REFERENCES outbound_order_lines(id),
    FOREIGN KEY (inventory_unit_id) REFERENCES inventory_units(id)
) STRICT;

INSERT INTO outbound_allocations
    (id, workspace_id, outbound_order_line_id, inventory_unit_id, status,
     allocated_by, allocated_at, released_at)
SELECT id, workspace_id, outbound_order_line_id, inventory_unit_id, status,
       allocated_by, allocated_at, released_at
  FROM outbound_allocations_rebuild_source;

CREATE UNIQUE INDEX outbound_allocations_one_active_unit_idx
    ON outbound_allocations (workspace_id, inventory_unit_id)
    WHERE status IN ('active', 'shipped');

CREATE TABLE outbound_shipment_lines (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    outbound_shipment_id TEXT NOT NULL,
    outbound_allocation_id TEXT NOT NULL,
    inventory_unit_id TEXT NOT NULL,
    scanned_barcode_snapshot TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (outbound_shipment_id) REFERENCES outbound_shipments(id),
    FOREIGN KEY (outbound_allocation_id) REFERENCES outbound_allocations(id),
    FOREIGN KEY (inventory_unit_id) REFERENCES inventory_units(id)
) STRICT;

INSERT INTO outbound_shipment_lines
    (id, workspace_id, outbound_shipment_id, outbound_allocation_id,
     inventory_unit_id, scanned_barcode_snapshot, created_at)
SELECT id, workspace_id, outbound_shipment_id, outbound_allocation_id,
       inventory_unit_id, scanned_barcode_snapshot, created_at
  FROM outbound_shipment_lines_rebuild_source;

CREATE UNIQUE INDEX outbound_shipment_lines_allocation_idx
    ON outbound_shipment_lines (workspace_id, outbound_allocation_id);
CREATE INDEX outbound_shipment_lines_unit_history_idx
    ON outbound_shipment_lines (workspace_id, inventory_unit_id, created_at);

CREATE TABLE delivery_confirmation_lines (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    delivery_confirmation_id TEXT NOT NULL,
    outbound_shipment_line_id TEXT NOT NULL,
    result TEXT NOT NULL CHECK (result IN ('accepted', 'rejected')),
    exception_notes TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (delivery_confirmation_id) REFERENCES delivery_confirmations(id),
    FOREIGN KEY (outbound_shipment_line_id) REFERENCES outbound_shipment_lines(id),
    UNIQUE (outbound_shipment_line_id)
) STRICT;

INSERT INTO delivery_confirmation_lines
    (id, workspace_id, delivery_confirmation_id, outbound_shipment_line_id,
     result, exception_notes, created_at)
SELECT id, workspace_id, delivery_confirmation_id, outbound_shipment_line_id,
       result, exception_notes, created_at
  FROM delivery_confirmation_lines_rebuild_source;

CREATE TABLE outbound_return_lines (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    return_batch_id TEXT NOT NULL,
    outbound_shipment_line_id TEXT NOT NULL,
    inventory_unit_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK (disposition IN ('quarantine', 'returned_to_owner', 'scrapped')),
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (return_batch_id) REFERENCES outbound_return_batches(id),
    FOREIGN KEY (outbound_shipment_line_id) REFERENCES outbound_shipment_lines(id),
    FOREIGN KEY (inventory_unit_id) REFERENCES inventory_units(id),
    UNIQUE (outbound_shipment_line_id)
) STRICT;

INSERT INTO outbound_return_lines
    (id, workspace_id, return_batch_id, outbound_shipment_line_id,
     inventory_unit_id, reason, disposition, created_at)
SELECT id, workspace_id, return_batch_id, outbound_shipment_line_id,
       inventory_unit_id, reason, disposition, created_at
  FROM outbound_return_lines_rebuild_source;

CREATE TABLE stock_movements (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    inventory_unit_id TEXT NOT NULL,
    movement_type TEXT NOT NULL CHECK (movement_type IN (
        'received', 'moved', 'reserved', 'reservation_released', 'shipped',
        'delivered', 'returned', 'scrapped', 'returned_to_owner', 'voided', 'corrected'
    )),
    from_location_id TEXT,
    to_location_id TEXT,
    source_type TEXT NOT NULL,
    source_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (inventory_unit_id) REFERENCES inventory_units(id),
    FOREIGN KEY (from_location_id) REFERENCES locations(id),
    FOREIGN KEY (to_location_id) REFERENCES locations(id)
) STRICT;

INSERT INTO stock_movements
    (id, workspace_id, inventory_unit_id, movement_type, from_location_id,
     to_location_id, source_type, source_id, actor_id, occurred_at, created_at)
SELECT id, workspace_id, inventory_unit_id, movement_type, from_location_id,
       to_location_id, source_type, source_id, actor_id, occurred_at, created_at
  FROM stock_movements_rebuild_source;

CREATE INDEX stock_movements_unit_time_idx
    ON stock_movements (workspace_id, inventory_unit_id, occurred_at DESC);

CREATE TABLE legacy_import_rows (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    batch_id TEXT NOT NULL,
    source_row INTEGER NOT NULL CHECK (source_row >= 2),
    row_status TEXT NOT NULL CHECK (row_status IN ('imported', 'skipped', 'error')),
    raw_values_json TEXT NOT NULL,
    issues_json TEXT NOT NULL,
    shipment_barcode TEXT,
    return_barcode TEXT,
    counterparty_raw TEXT,
    shipment_time_raw TEXT,
    return_time_raw TEXT,
    shipment_time_normalized TEXT,
    return_time_normalized TEXT,
    shipment_time_fact TEXT NOT NULL CHECK (shipment_time_fact IN ('known', 'unknown', 'not_applicable')),
    return_time_fact TEXT NOT NULL CHECK (return_time_fact IN ('known', 'unknown', 'not_applicable')),
    source_kind TEXT NOT NULL CHECK (source_kind = 'legacy_migration'),
    received_at_fact TEXT NOT NULL CHECK (received_at_fact = 'unknown'),
    owner_fact TEXT NOT NULL CHECK (owner_fact = 'unknown'),
    sku_fact TEXT NOT NULL CHECK (sku_fact = 'unknown'),
    quality_fact TEXT NOT NULL CHECK (quality_fact = 'unknown'),
    quality_status_snapshot TEXT NOT NULL CHECK (quality_status_snapshot = 'untested'),
    counterparty_semantics TEXT NOT NULL CHECK (counterparty_semantics = 'unknown'),
    shipment_inventory_unit_id TEXT,
    outbound_shipment_line_id TEXT,
    returned_inventory_unit_id TEXT,
    outbound_return_line_id TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (batch_id) REFERENCES legacy_import_batches(id),
    FOREIGN KEY (shipment_inventory_unit_id) REFERENCES inventory_units(id),
    FOREIGN KEY (outbound_shipment_line_id) REFERENCES outbound_shipment_lines(id),
    FOREIGN KEY (returned_inventory_unit_id) REFERENCES inventory_units(id),
    FOREIGN KEY (outbound_return_line_id) REFERENCES outbound_return_lines(id),
    UNIQUE (batch_id, source_row)
) STRICT;

INSERT INTO legacy_import_rows
SELECT * FROM legacy_import_rows_rebuild_source;

CREATE INDEX legacy_import_rows_shipment_barcode_idx
    ON legacy_import_rows (workspace_id, shipment_barcode);
CREATE INDEX legacy_import_rows_return_barcode_idx
    ON legacy_import_rows (workspace_id, return_barcode);

DROP TABLE inventory_units_rebuild_source;
DROP TABLE quality_inspection_results_rebuild_source;
DROP TABLE quality_waivers_rebuild_source;
DROP TABLE outbound_allocations_rebuild_source;
DROP TABLE outbound_shipment_lines_rebuild_source;
DROP TABLE delivery_confirmation_lines_rebuild_source;
DROP TABLE outbound_return_lines_rebuild_source;
DROP TABLE stock_movements_rebuild_source;
DROP TABLE legacy_import_rows_rebuild_source;
