CREATE TABLE migration_result_reports (
    export_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    migration_id TEXT NOT NULL,
    target_workspace_id TEXT NOT NULL,
    checksum TEXT NOT NULL,
    import_status TEXT NOT NULL CHECK (import_status IN ('imported', 'already_imported')),
    entity_counts_json TEXT NOT NULL,
    server_imported_at TEXT,
    archived_at TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY (export_id) REFERENCES migration_packages(export_id)
) STRICT;
