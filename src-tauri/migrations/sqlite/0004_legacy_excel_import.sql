CREATE TABLE legacy_import_batches (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    source_file_name TEXT NOT NULL,
    source_file_sha256 TEXT NOT NULL CHECK (
        length(source_file_sha256) = 64
        AND source_file_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    source_file_bytes INTEGER NOT NULL CHECK (source_file_bytes >= 0),
    sheet_name TEXT NOT NULL,
    preview_id TEXT NOT NULL CHECK (
        length(preview_id) = 64
        AND preview_id NOT GLOB '*[^0-9a-f]*'
    ),
    mapping_json TEXT NOT NULL,
    selected_rows_json TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    response_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status = 'committed'),
    source_kind TEXT NOT NULL CHECK (source_kind = 'legacy_migration'),
    actor_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    imported_shipments INTEGER NOT NULL CHECK (imported_shipments >= 0),
    imported_returns INTEGER NOT NULL CHECK (imported_returns >= 0),
    skipped_rows INTEGER NOT NULL CHECK (skipped_rows >= 0),
    error_rows INTEGER NOT NULL CHECK (error_rows >= 0),
    created_at TEXT NOT NULL,
    committed_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    UNIQUE (workspace_id, idempotency_key)
) STRICT;

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

CREATE INDEX legacy_import_batches_source_idx
    ON legacy_import_batches (workspace_id, source_file_sha256, committed_at DESC);
CREATE INDEX legacy_import_rows_shipment_barcode_idx
    ON legacy_import_rows (workspace_id, shipment_barcode);
CREATE INDEX legacy_import_rows_return_barcode_idx
    ON legacy_import_rows (workspace_id, return_barcode);
