ALTER TABLE quality_inspection_results
    ADD CONSTRAINT quality_inspection_results_label_snapshot_check CHECK (
        (quality_label_id IS NULL AND quality_label_snapshot IS NULL)
        OR (
            quality_label_id IS NOT NULL
            AND length(btrim(quality_label_snapshot)) > 0
        )
    );

CREATE TABLE quality_label_name_history (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    quality_label_id uuid NOT NULL,
    old_name text NOT NULL CHECK (length(btrim(old_name)) > 0 AND length(old_name) <= 40),
    new_name text NOT NULL CHECK (length(btrim(new_name)) > 0 AND length(new_name) <= 40),
    changed_by uuid NOT NULL,
    changed_by_snapshot text NOT NULL CHECK (
        length(btrim(changed_by_snapshot)) > 0 AND length(changed_by_snapshot) <= 100
    ),
    source_actor_id text,
    change_note text CHECK (change_note IS NULL OR length(change_note) <= 200),
    changed_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id),
    FOREIGN KEY (tenant_id, quality_label_id)
        REFERENCES quality_labels (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, changed_by)
        REFERENCES users (tenant_id, id) ON DELETE RESTRICT,
    CHECK (old_name <> new_name)
);

CREATE INDEX quality_label_name_history_label_time_idx
    ON quality_label_name_history (tenant_id, quality_label_id, changed_at DESC, id DESC);

ALTER TABLE quality_label_name_history ENABLE ROW LEVEL SECURITY;
ALTER TABLE quality_label_name_history FORCE ROW LEVEL SECURITY;
CREATE POLICY quality_label_name_history_current_context ON quality_label_name_history
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

CREATE FUNCTION app.reject_quality_label_name_history_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'quality label name history is append-only'
        USING ERRCODE = '55000';
END
$$;

CREATE TRIGGER quality_label_name_history_reject_update
BEFORE UPDATE ON quality_label_name_history
FOR EACH ROW EXECUTE FUNCTION app.reject_quality_label_name_history_mutation();

CREATE TRIGGER quality_label_name_history_reject_delete
BEFORE DELETE ON quality_label_name_history
FOR EACH ROW EXECUTE FUNCTION app.reject_quality_label_name_history_mutation();
