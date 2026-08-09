-- Preserve the upstream source for each new inbound receipt and make
-- product-specific scanner safeguards durable in the network catalog.
-- supplier_party_id stays nullable so historical receipts retain their facts.

ALTER TABLE skus
    ADD COLUMN serial_prefix text,
    ADD COLUMN serial_forbidden_chars text NOT NULL DEFAULT '';

ALTER TABLE skus
    ADD CONSTRAINT skus_serial_prefix_nonblank
    CHECK (serial_prefix IS NULL OR length(btrim(serial_prefix)) > 0);

ALTER TABLE inbound_receipts
    ADD COLUMN supplier_party_id uuid;

ALTER TABLE inbound_receipts
    ADD CONSTRAINT inbound_receipts_supplier_party_fk
    FOREIGN KEY (tenant_id, supplier_party_id)
    REFERENCES business_parties (tenant_id, id)
    ON DELETE RESTRICT;

CREATE INDEX inbound_receipts_supplier_idx
    ON inbound_receipts (tenant_id, supplier_party_id, received_at DESC);
