CREATE UNIQUE INDEX quality_labels_workspace_id_idx
    ON quality_labels (workspace_id, id);

CREATE TRIGGER quality_inspection_results_label_insert_guard
BEFORE INSERT ON quality_inspection_results
FOR EACH ROW
WHEN (NEW.quality_label_id IS NULL) <> (NEW.quality_label_snapshot IS NULL)
  OR (
      NEW.quality_label_id IS NOT NULL
      AND NOT EXISTS (
          SELECT 1
            FROM quality_labels label
           WHERE label.id = NEW.quality_label_id
             AND label.workspace_id = NEW.workspace_id
      )
  )
BEGIN
    SELECT RAISE(ABORT, 'quality inspection label reference or snapshot is invalid');
END;

CREATE TRIGGER quality_inspection_results_label_update_guard
BEFORE UPDATE OF workspace_id, quality_label_id, quality_label_snapshot
ON quality_inspection_results
FOR EACH ROW
WHEN (NEW.quality_label_id IS NULL) <> (NEW.quality_label_snapshot IS NULL)
  OR (
      NEW.quality_label_id IS NOT NULL
      AND NOT EXISTS (
          SELECT 1
            FROM quality_labels label
           WHERE label.id = NEW.quality_label_id
             AND label.workspace_id = NEW.workspace_id
      )
  )
BEGIN
    SELECT RAISE(ABORT, 'quality inspection label reference or snapshot is invalid');
END;

CREATE TABLE quality_label_name_history (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    quality_label_id TEXT NOT NULL,
    old_name TEXT NOT NULL CHECK (length(trim(old_name)) > 0 AND length(old_name) <= 40),
    new_name TEXT NOT NULL CHECK (length(trim(new_name)) > 0 AND length(new_name) <= 40),
    changed_by TEXT NOT NULL CHECK (length(trim(changed_by)) > 0 AND length(changed_by) <= 100),
    changed_by_snapshot TEXT NOT NULL CHECK (
        length(trim(changed_by_snapshot)) > 0 AND length(changed_by_snapshot) <= 100
    ),
    change_note TEXT CHECK (change_note IS NULL OR length(change_note) <= 200),
    changed_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE RESTRICT,
    FOREIGN KEY (workspace_id, quality_label_id)
        REFERENCES quality_labels(workspace_id, id) ON DELETE RESTRICT,
    CHECK (old_name <> new_name)
) STRICT;

CREATE INDEX quality_label_name_history_label_time_idx
    ON quality_label_name_history (workspace_id, quality_label_id, changed_at DESC, id DESC);

CREATE TRIGGER quality_label_name_history_reject_update
BEFORE UPDATE ON quality_label_name_history
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'quality label name history is append-only');
END;

CREATE TRIGGER quality_label_name_history_reject_delete
BEFORE DELETE ON quality_label_name_history
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'quality label name history is append-only');
END;
