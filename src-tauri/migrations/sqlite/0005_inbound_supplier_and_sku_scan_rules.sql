-- Preserve the upstream source for each new inbound receipt and make
-- product-specific scanner safeguards durable in the offline workspace.
-- supplier_party_id stays nullable so historical receipts retain their facts.

ALTER TABLE skus ADD COLUMN serial_prefix TEXT;
ALTER TABLE skus ADD COLUMN serial_forbidden_chars TEXT NOT NULL DEFAULT '';

ALTER TABLE inbound_receipts
    ADD COLUMN supplier_party_id TEXT REFERENCES business_parties(id);

CREATE INDEX inbound_receipts_supplier_idx
    ON inbound_receipts (workspace_id, supplier_party_id, received_at DESC);
