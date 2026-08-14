CREATE TABLE quality_labels (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    name TEXT NOT NULL CHECK (length(trim(name)) > 0 AND length(name) <= 40),
    normalized_name TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK (disposition IN ('available', 'quarantine')),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    UNIQUE (workspace_id, normalized_name)
) STRICT;

CREATE INDEX quality_labels_active_idx
    ON quality_labels (workspace_id, active, disposition, name);

ALTER TABLE quality_inspection_results
    ADD COLUMN quality_label_id TEXT;

ALTER TABLE quality_inspection_results
    ADD COLUMN quality_label_snapshot TEXT;

CREATE INDEX quality_inspection_results_label_idx
    ON quality_inspection_results (workspace_id, quality_label_id);
