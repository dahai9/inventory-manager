-- A returned unit can pass retest and ship again. The original schema made
-- inventory_unit_id unique across all shipment history, which incorrectly
-- prevented that second lifecycle. Keep an ordinary history index instead;
-- the application transaction still requires an active allocation and the
-- unit's current available state before each shipment.

CREATE TEMP TABLE delivery_confirmation_lines_saved AS
SELECT * FROM delivery_confirmation_lines;

CREATE TEMP TABLE outbound_return_lines_saved AS
SELECT * FROM outbound_return_lines;

CREATE TEMP TABLE outbound_shipment_lines_rebuild_source AS
SELECT * FROM outbound_shipment_lines;

DROP TABLE delivery_confirmation_lines;
DROP TABLE outbound_return_lines;
DROP TABLE outbound_shipment_lines;

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

-- Recreate the parent history from the rows still referenced by the saved
-- children and by all other historical shipments.
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
SELECT * FROM delivery_confirmation_lines_saved;

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
SELECT * FROM outbound_return_lines_saved;

DROP TABLE delivery_confirmation_lines_saved;
DROP TABLE outbound_return_lines_saved;
DROP TABLE outbound_shipment_lines_rebuild_source;
