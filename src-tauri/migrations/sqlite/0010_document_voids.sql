CREATE TABLE operation_credentials (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    password_hash TEXT NOT NULL CHECK (length(trim(password_hash)) > 0),
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE document_voids (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    document_kind TEXT NOT NULL CHECK (document_kind IN ('inbound_receipt', 'outbound_order')),
    inbound_receipt_id TEXT,
    outbound_order_id TEXT,
    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
    actor_id TEXT NOT NULL,
    voided_at TEXT NOT NULL,
    request_id TEXT NOT NULL CHECK (length(trim(request_id)) > 0),
    idempotency_key TEXT NOT NULL CHECK (length(trim(idempotency_key)) > 0),
    created_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (inbound_receipt_id) REFERENCES inbound_receipts(id),
    FOREIGN KEY (outbound_order_id) REFERENCES outbound_orders(id),
    CHECK (
        (document_kind = 'inbound_receipt' AND inbound_receipt_id IS NOT NULL AND outbound_order_id IS NULL)
        OR
        (document_kind = 'outbound_order' AND outbound_order_id IS NOT NULL AND inbound_receipt_id IS NULL)
    ),
    UNIQUE (workspace_id, document_kind, idempotency_key)
) STRICT;

CREATE UNIQUE INDEX document_voids_receipt_once_idx
    ON document_voids (workspace_id, inbound_receipt_id)
    WHERE inbound_receipt_id IS NOT NULL;

CREATE UNIQUE INDEX document_voids_order_once_idx
    ON document_voids (workspace_id, outbound_order_id)
    WHERE outbound_order_id IS NOT NULL;

CREATE INDEX document_voids_workspace_time_idx
    ON document_voids (workspace_id, voided_at DESC, id);
