-- Optional supplier and customer warranty terms are immutable snapshots on
-- the business document that established them. Historical documents remain
-- valid with NULL terms.

ALTER TABLE inbound_receipts ADD COLUMN warranty_duration_days INTEGER
    CHECK (warranty_duration_days IS NULL OR warranty_duration_days BETWEEN 1 AND 36500);
ALTER TABLE inbound_receipts ADD COLUMN warranty_label_snapshot TEXT;
ALTER TABLE inbound_receipts ADD COLUMN warranty_started_at TEXT;
ALTER TABLE inbound_receipts ADD COLUMN warranty_expires_at TEXT;

ALTER TABLE outbound_shipments ADD COLUMN warranty_duration_days INTEGER
    CHECK (warranty_duration_days IS NULL OR warranty_duration_days BETWEEN 1 AND 36500);
ALTER TABLE outbound_shipments ADD COLUMN warranty_label_snapshot TEXT;
ALTER TABLE outbound_shipments ADD COLUMN warranty_started_at TEXT;
ALTER TABLE outbound_shipments ADD COLUMN warranty_expires_at TEXT;

CREATE INDEX inbound_receipts_warranty_expiry_idx
    ON inbound_receipts (workspace_id, warranty_expires_at)
    WHERE warranty_expires_at IS NOT NULL;
CREATE INDEX outbound_shipments_warranty_expiry_idx
    ON outbound_shipments (workspace_id, warranty_expires_at)
    WHERE warranty_expires_at IS NOT NULL;
