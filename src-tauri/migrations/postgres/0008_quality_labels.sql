CREATE TABLE quality_labels (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    name text NOT NULL CHECK (length(btrim(name)) > 0 AND length(name) <= 40),
    normalized_name text NOT NULL,
    disposition text NOT NULL CHECK (disposition IN ('available', 'quarantine')),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, normalized_name)
);

CREATE INDEX quality_labels_active_idx
    ON quality_labels (tenant_id, active, disposition, name);

ALTER TABLE quality_inspection_results
    ADD COLUMN quality_label_id uuid,
    ADD COLUMN quality_label_snapshot text,
    ADD CONSTRAINT quality_inspection_results_label_fk
        FOREIGN KEY (tenant_id, quality_label_id)
        REFERENCES quality_labels (tenant_id, id) ON DELETE RESTRICT;

CREATE INDEX quality_inspection_results_label_idx
    ON quality_inspection_results (tenant_id, quality_label_id);

ALTER TABLE quality_labels ENABLE ROW LEVEL SECURITY;
ALTER TABLE quality_labels FORCE ROW LEVEL SECURITY;
CREATE POLICY quality_labels_current_context ON quality_labels
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());
