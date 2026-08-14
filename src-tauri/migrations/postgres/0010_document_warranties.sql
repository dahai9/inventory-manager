-- Keep supplier and customer warranty terms on the immutable receipt or
-- shipment that established them. Existing rows intentionally remain NULL.

ALTER TABLE inbound_receipts
    ADD COLUMN warranty_duration_days integer,
    ADD COLUMN warranty_label_snapshot text,
    ADD COLUMN warranty_started_at timestamptz,
    ADD COLUMN warranty_expires_at timestamptz,
    ADD CONSTRAINT inbound_receipts_warranty_duration_check
        CHECK (warranty_duration_days IS NULL OR warranty_duration_days BETWEEN 1 AND 36500);

ALTER TABLE outbound_shipments
    ADD COLUMN warranty_duration_days integer,
    ADD COLUMN warranty_label_snapshot text,
    ADD COLUMN warranty_started_at timestamptz,
    ADD COLUMN warranty_expires_at timestamptz,
    ADD CONSTRAINT outbound_shipments_warranty_duration_check
        CHECK (warranty_duration_days IS NULL OR warranty_duration_days BETWEEN 1 AND 36500);

CREATE INDEX inbound_receipts_warranty_expiry_idx
    ON inbound_receipts (tenant_id, warranty_expires_at)
    WHERE warranty_expires_at IS NOT NULL;
CREATE INDEX outbound_shipments_warranty_expiry_idx
    ON outbound_shipments (tenant_id, warranty_expires_at)
    WHERE warranty_expires_at IS NOT NULL;
