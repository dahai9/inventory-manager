PRAGMA foreign_keys = ON;

CREATE TABLE workspaces (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    timezone TEXT NOT NULL DEFAULT 'Asia/Shanghai',
    source_instance_id TEXT NOT NULL,
    read_only INTEGER NOT NULL DEFAULT 0 CHECK (read_only IN (0, 1)),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE business_parties (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    display_name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    UNIQUE (workspace_id, normalized_name)
) STRICT;

CREATE TABLE party_roles (
    workspace_id TEXT NOT NULL,
    party_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('goods_owner', 'upstream_receiver', 'supplier', 'carrier')),
    created_at TEXT NOT NULL,
    PRIMARY KEY (workspace_id, party_id, role),
    FOREIGN KEY (party_id) REFERENCES business_parties(id)
) STRICT;

CREATE TABLE skus (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    code TEXT NOT NULL,
    name TEXT NOT NULL,
    tracking_mode TEXT NOT NULL DEFAULT 'serial' CHECK (tracking_mode IN ('serial', 'quantity')),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    UNIQUE (workspace_id, code)
) STRICT;

CREATE TABLE warehouses (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    code TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    UNIQUE (workspace_id, code)
) STRICT;

CREATE TABLE locations (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    warehouse_id TEXT NOT NULL,
    code TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('receiving', 'storage', 'quarantine', 'shipping')),
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (warehouse_id) REFERENCES warehouses(id),
    UNIQUE (workspace_id, warehouse_id, code)
) STRICT;

CREATE TABLE inbound_receipts (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    receipt_no TEXT NOT NULL,
    owner_party_id TEXT NOT NULL,
    warehouse_id TEXT NOT NULL,
    source_reference TEXT,
    received_at TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('draft', 'posted', 'voided')),
    actor_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (owner_party_id) REFERENCES business_parties(id),
    FOREIGN KEY (warehouse_id) REFERENCES warehouses(id),
    UNIQUE (workspace_id, receipt_no),
    UNIQUE (workspace_id, idempotency_key)
) STRICT;

CREATE TABLE inbound_receipt_lines (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    receipt_id TEXT NOT NULL,
    sku_id TEXT NOT NULL,
    declared_quantity INTEGER NOT NULL CHECK (declared_quantity > 0),
    scanned_quantity INTEGER NOT NULL CHECK (scanned_quantity >= 0 AND scanned_quantity <= declared_quantity),
    notes TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (receipt_id) REFERENCES inbound_receipts(id),
    FOREIGN KEY (sku_id) REFERENCES skus(id)
) STRICT;

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
    FOREIGN KEY (location_id) REFERENCES locations(id),
    UNIQUE (workspace_id, barcode)
) STRICT;

CREATE TABLE quality_inspections (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    inspection_no TEXT NOT NULL,
    inspection_type TEXT NOT NULL CHECK (inspection_type IN ('initial', 'retest')),
    status TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'voided')),
    inspector_id TEXT NOT NULL,
    inspected_at TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    UNIQUE (workspace_id, inspection_no),
    UNIQUE (workspace_id, idempotency_key)
) STRICT;

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
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (inspection_id) REFERENCES quality_inspections(id),
    FOREIGN KEY (inventory_unit_id) REFERENCES inventory_units(id),
    UNIQUE (inspection_id, inventory_unit_id)
) STRICT;

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

CREATE TABLE outbound_orders (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    order_no TEXT NOT NULL,
    upstream_receiver_id TEXT NOT NULL,
    required_at TEXT,
    status TEXT NOT NULL CHECK (status IN ('draft', 'open', 'partially_allocated', 'allocated', 'partially_shipped', 'shipped', 'completed', 'voided')),
    actor_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (upstream_receiver_id) REFERENCES business_parties(id),
    UNIQUE (workspace_id, order_no),
    UNIQUE (workspace_id, idempotency_key)
) STRICT;

CREATE TABLE outbound_order_lines (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    outbound_order_id TEXT NOT NULL,
    sku_id TEXT NOT NULL,
    required_quantity INTEGER NOT NULL CHECK (required_quantity > 0),
    allocated_quantity INTEGER NOT NULL DEFAULT 0 CHECK (allocated_quantity >= 0),
    shipped_quantity INTEGER NOT NULL DEFAULT 0 CHECK (shipped_quantity >= 0),
    delivered_quantity INTEGER NOT NULL DEFAULT 0 CHECK (delivered_quantity >= 0),
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (outbound_order_id) REFERENCES outbound_orders(id),
    FOREIGN KEY (sku_id) REFERENCES skus(id),
    CHECK (delivered_quantity <= shipped_quantity AND shipped_quantity <= allocated_quantity AND allocated_quantity <= required_quantity)
) STRICT;

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

CREATE UNIQUE INDEX outbound_allocations_one_active_unit_idx
    ON outbound_allocations (workspace_id, inventory_unit_id)
    WHERE status IN ('active', 'shipped');

CREATE TABLE outbound_shipments (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    shipment_no TEXT NOT NULL,
    outbound_order_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('draft', 'posted', 'partially_delivered', 'delivered', 'voided')),
    shipped_at TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (outbound_order_id) REFERENCES outbound_orders(id),
    UNIQUE (workspace_id, shipment_no),
    UNIQUE (workspace_id, idempotency_key)
) STRICT;

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
    FOREIGN KEY (inventory_unit_id) REFERENCES inventory_units(id),
    UNIQUE (workspace_id, inventory_unit_id)
) STRICT;

CREATE TABLE delivery_confirmations (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    outbound_shipment_id TEXT NOT NULL,
    confirmation_code TEXT NOT NULL,
    confirmed_by TEXT NOT NULL,
    confirmed_at TEXT NOT NULL,
    notes TEXT,
    idempotency_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (outbound_shipment_id) REFERENCES outbound_shipments(id),
    UNIQUE (workspace_id, confirmation_code),
    UNIQUE (workspace_id, idempotency_key)
) STRICT;

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

CREATE TABLE outbound_return_batches (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    return_no TEXT NOT NULL,
    returned_at TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    UNIQUE (workspace_id, return_no),
    UNIQUE (workspace_id, idempotency_key)
) STRICT;

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

CREATE TABLE audit_logs (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    action TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    result TEXT NOT NULL CHECK (result IN ('success', 'rejected')),
    details_json TEXT NOT NULL DEFAULT '{}',
    occurred_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id)
) STRICT;

CREATE TABLE idempotency_records (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    response_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    UNIQUE (workspace_id, scope, idempotency_key)
) STRICT;

CREATE TABLE migration_packages (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    export_id TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('offline_to_network', 'network_to_offline')),
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    checksum TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('created', 'validated', 'imported', 'failed', 'archived')),
    created_at TEXT NOT NULL,
    imported_at TEXT,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    UNIQUE (export_id)
) STRICT;

CREATE INDEX inventory_units_owner_received_idx
    ON inventory_units (workspace_id, owner_party_id, received_at DESC);
CREATE INDEX inventory_units_sku_status_idx
    ON inventory_units (workspace_id, sku_id, inventory_status, quality_status);
CREATE INDEX inventory_units_receipt_line_idx
    ON inventory_units (workspace_id, inbound_receipt_line_id, id);
CREATE INDEX stock_movements_unit_time_idx
    ON stock_movements (workspace_id, inventory_unit_id, occurred_at DESC);
CREATE INDEX audit_logs_actor_time_idx
    ON audit_logs (workspace_id, actor_id, occurred_at DESC);
