//! One-time, offline SQLite to network PostgreSQL upgrade packages.
//!
//! Version 3 uses an uncompressed directory whose name ends in `.invpack` and
//! preserves quality-label definitions and result snapshots in addition to the
//! version 2 supplier and scanner fields. Versions 1 and 2 remain readable,
//! while older network services reject version 3 data instead of silently
//! discarding the added fields.
//! Keeping the package inspectable avoids adding an archive dependency and makes
//! every file independently verifiable. A later archive representation must
//! increment `PACKAGE_FORMAT_VERSION` and retain the same safety properties.
//! Local activation data and network credentials, sessions and entitlements are
//! intentionally outside the exported table allowlist.

use super::auth::{authorize_session_in_transaction, AuthError};
use super::postgres::NetworkDatabase;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::types::Json as SqlxJson;
use sqlx::{Column, Postgres, Row, Sqlite, SqlitePool, Transaction, TypeInfo, ValueRef};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Display;
use std::fs::{self, File};
use std::future::Future;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub const PRODUCT_ID: &str = "inventory-manager";
pub const PACKAGE_FORMAT_VERSION: u32 = 3;
const MIN_SUPPORTED_PACKAGE_FORMAT_VERSION: u32 = 1;
pub const LOGICAL_SCHEMA_VERSION: i64 = 1;
const REQUIRED_SQLITE_MIGRATION_VERSION: i64 = 10;

const MANIFEST_FILE: &str = "manifest.json";
const CHECKSUMS_FILE: &str = "checksums.json";
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_JSONL_LINE_BYTES: usize = 16 * 1024 * 1024;
/// The first network upload implementation intentionally uses one bounded
/// JSON request.  Larger workspaces need a resumable streaming protocol rather
/// than silently allocating unbounded memory in the desktop and server.
pub const MAX_NETWORK_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_NETWORK_REQUEST_BYTES: usize = 80 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    #[error("invalid upgrade request: {0}")]
    InvalidRequest(String),
    #[error("unsafe package path: {0}")]
    UnsafePath(String),
    #[error("package already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("package I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("SQLite export failed: {0}")]
    Sqlite(#[from] sqlx::Error),
    #[error("package JSON failed at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("incompatible package: {0}")]
    Incompatible(String),
    #[error("package integrity check failed: {0}")]
    Integrity(String),
    #[error("package data validation failed: {0}")]
    Data(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationDirection {
    OfflineToNetwork,
}

#[derive(Clone, Debug)]
pub struct ExportRequest {
    pub export_id: String,
    pub workspace_id: String,
    /// UTC RFC 3339 timestamp supplied by the application layer.
    pub exported_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceIdentity {
    pub instance_id: String,
    pub workspace_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceMapping {
    pub source_workspace_id: String,
    pub target_workspace_id: Option<String>,
    pub requires_empty_target: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DataFileDigest {
    pub path: String,
    pub rows: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManifest {
    pub product: String,
    pub package_format_version: u32,
    pub logical_schema_version: i64,
    pub direction: MigrationDirection,
    pub export_id: String,
    pub migration_id: String,
    pub exported_at: String,
    pub source: SourceIdentity,
    pub workspace_mapping: WorkspaceMapping,
    pub files: Vec<DataFileDigest>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChecksumsDocument {
    pub package_format_version: u32,
    pub manifest_sha256: String,
    pub files: Vec<DataFileDigest>,
}

#[derive(Clone, Debug)]
pub struct ExportedPackage {
    pub path: PathBuf,
    pub manifest: PackageManifest,
    /// SHA-256 of the canonical `checksums.json`; suitable for the
    /// `migration_packages.checksum` conflict check.
    pub package_checksum: String,
}

#[derive(Clone, Debug)]
pub struct ValidatedPackage {
    root: PathBuf,
    pub manifest: PackageManifest,
    pub package_checksum: String,
    pub entity_counts: BTreeMap<String, u64>,
}

impl ValidatedPackage {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn data_file_path(&self, relative_path: &str) -> Result<PathBuf, UpgradeError> {
        safe_existing_package_file(&self.root, relative_path)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkUpgradeFile {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkUpgradeImportRequest {
    pub target_workspace_id: Uuid,
    pub manifest_json: String,
    pub checksums_json: String,
    pub files: Vec<NetworkUpgradeFile>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkUpgradeImportStatus {
    Imported,
    AlreadyImported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkUpgradeImportResponse {
    pub status: NetworkUpgradeImportStatus,
    pub export_id: String,
    pub migration_id: String,
    pub checksum: String,
    pub imported_at: Option<String>,
    pub entity_counts: BTreeMap<String, u64>,
}

/// Owns the server-side temporary directory for as long as the validated
/// package is being imported. Dropping it after success or error removes every
/// uploaded member.
pub struct StagedNetworkUpgrade {
    package: ValidatedPackage,
    _guard: StagingGuard,
}

impl StagedNetworkUpgrade {
    pub fn package(&self) -> &ValidatedPackage {
        &self.package
    }
}

/// Convert an already validated local directory into the bounded wire format.
/// Every member is re-read from the canonical package root so callers cannot
/// inject an unrelated path after local validation.
pub fn build_network_upgrade_request(
    package: &ValidatedPackage,
    target_workspace_id: Uuid,
) -> Result<NetworkUpgradeImportRequest, UpgradeError> {
    let manifest_path = package.data_file_path(MANIFEST_FILE)?;
    let checksums_path = package.data_file_path(CHECKSUMS_FILE)?;
    let manifest_json = read_utf8_package_member(&manifest_path, MAX_METADATA_BYTES)?;
    let checksums_json = read_utf8_package_member(&checksums_path, MAX_METADATA_BYTES)?;
    let mut total_bytes = checked_upload_size(0, manifest_json.len(), MANIFEST_FILE)?;
    total_bytes = checked_upload_size(total_bytes, checksums_json.len(), CHECKSUMS_FILE)?;
    let mut files = Vec::with_capacity(package.manifest.files.len());
    for digest in &package.manifest.files {
        let path = package.data_file_path(&digest.path)?;
        let content = read_utf8_package_member(&path, MAX_NETWORK_PACKAGE_BYTES)?;
        total_bytes = checked_upload_size(total_bytes, content.len(), &digest.path)?;
        files.push(NetworkUpgradeFile {
            path: digest.path.clone(),
            content,
        });
    }
    Ok(NetworkUpgradeImportRequest {
        target_workspace_id,
        manifest_json,
        checksums_json,
        files,
    })
}

/// Reconstruct and fully validate a network upload before opening a
/// PostgreSQL transaction. Only the exact format-v1 member allowlist is
/// accepted, and all files are written with `create_new` under a private
/// server-generated directory.
pub fn stage_network_upgrade_request(
    request: NetworkUpgradeImportRequest,
) -> Result<StagedNetworkUpgrade, UpgradeError> {
    let expected_paths: Vec<&str> = DATA_FILES.iter().map(|spec| spec.path).collect();
    let supplied_paths: Vec<&str> = request
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    if supplied_paths != expected_paths {
        return Err(UpgradeError::Integrity(format!(
            "network package files must be exactly {expected_paths:?} in canonical order"
        )));
    }
    let mut total_bytes = checked_upload_size(0, request.manifest_json.len(), MANIFEST_FILE)?;
    total_bytes = checked_upload_size(total_bytes, request.checksums_json.len(), CHECKSUMS_FILE)?;
    for file in &request.files {
        validate_relative_path(&file.path)?;
        total_bytes = checked_upload_size(total_bytes, file.content.len(), &file.path)?;
    }
    let root = std::env::temp_dir().join(format!(
        "inventory-network-upload-{}.invpack",
        Uuid::now_v7()
    ));
    fs::create_dir(&root).map_err(|source| UpgradeError::Io {
        path: root.clone(),
        source,
    })?;
    let guard = StagingGuard::new(root.clone());
    write_new_file(
        &safe_new_package_file(&root, MANIFEST_FILE)?,
        request.manifest_json.as_bytes(),
    )?;
    write_new_file(
        &safe_new_package_file(&root, CHECKSUMS_FILE)?,
        request.checksums_json.as_bytes(),
    )?;
    for file in request.files {
        write_new_file(
            &safe_new_package_file(&root, &file.path)?,
            file.content.as_bytes(),
        )?;
    }
    let package = validate_package(&root)?;
    Ok(StagedNetworkUpgrade {
        package,
        _guard: guard,
    })
}

fn read_utf8_package_member(path: &Path, max_bytes: u64) -> Result<String, UpgradeError> {
    String::from_utf8(read_limited(path, max_bytes)?)
        .map_err(|_| UpgradeError::Integrity(format!("{} is not valid UTF-8", path.display())))
}

fn checked_upload_size(
    current: u64,
    member_bytes: usize,
    member: &str,
) -> Result<u64, UpgradeError> {
    let member_bytes = u64::try_from(member_bytes)
        .map_err(|_| UpgradeError::Integrity(format!("{member} size cannot be represented")))?;
    let total = current
        .checked_add(member_bytes)
        .ok_or_else(|| UpgradeError::Integrity("network package size overflow".to_owned()))?;
    if total > MAX_NETWORK_PACKAGE_BYTES {
        return Err(UpgradeError::Integrity(format!(
            "network package exceeds the {} byte upload limit",
            MAX_NETWORK_PACKAGE_BYTES
        )));
    }
    Ok(total)
}

#[derive(Clone, Copy)]
struct TableSpec {
    name: &'static str,
    order_by: &'static str,
}

#[derive(Clone, Copy)]
struct DataFileSpec {
    path: &'static str,
    tables: &'static [TableSpec],
}

const MASTER_TABLES: &[TableSpec] = &[
    TableSpec {
        name: "workspaces",
        order_by: "id",
    },
    TableSpec {
        name: "business_parties",
        order_by: "id",
    },
    TableSpec {
        name: "party_roles",
        order_by: "party_id, role",
    },
    TableSpec {
        name: "skus",
        order_by: "id",
    },
    TableSpec {
        name: "warehouses",
        order_by: "id",
    },
    TableSpec {
        name: "locations",
        order_by: "id",
    },
];

const INBOUND_TABLES: &[TableSpec] = &[
    TableSpec {
        name: "inbound_receipts",
        order_by: "id",
    },
    TableSpec {
        name: "inbound_receipt_lines",
        order_by: "id",
    },
    TableSpec {
        name: "inventory_units",
        order_by: "id",
    },
];

const QUALITY_TABLES: &[TableSpec] = &[
    TableSpec {
        name: "quality_labels",
        order_by: "id",
    },
    TableSpec {
        name: "quality_label_name_history",
        order_by: "id",
    },
    TableSpec {
        name: "quality_inspections",
        order_by: "id",
    },
    TableSpec {
        name: "quality_inspection_results",
        order_by: "id",
    },
    TableSpec {
        name: "quality_waivers",
        order_by: "id",
    },
];

const OUTBOUND_TABLES: &[TableSpec] = &[
    TableSpec {
        name: "outbound_orders",
        order_by: "id",
    },
    TableSpec {
        name: "outbound_order_lines",
        order_by: "id",
    },
    TableSpec {
        name: "outbound_allocations",
        order_by: "id",
    },
    TableSpec {
        name: "outbound_shipments",
        order_by: "id",
    },
    TableSpec {
        name: "outbound_shipment_lines",
        order_by: "id",
    },
    TableSpec {
        name: "delivery_confirmations",
        order_by: "id",
    },
    TableSpec {
        name: "delivery_confirmation_lines",
        order_by: "id",
    },
    TableSpec {
        name: "outbound_return_batches",
        order_by: "id",
    },
    TableSpec {
        name: "outbound_return_lines",
        order_by: "id",
    },
    TableSpec {
        name: "stock_movements",
        order_by: "id",
    },
];

const AUDIT_TABLES: &[TableSpec] = &[
    TableSpec {
        name: "document_voids",
        order_by: "id",
    },
    TableSpec {
        name: "audit_logs",
        order_by: "id",
    },
    TableSpec {
        name: "idempotency_records",
        order_by: "id",
    },
];

const DATA_FILES: &[DataFileSpec] = &[
    DataFileSpec {
        path: "master-data.jsonl",
        tables: MASTER_TABLES,
    },
    DataFileSpec {
        path: "inbound.jsonl",
        tables: INBOUND_TABLES,
    },
    DataFileSpec {
        path: "quality.jsonl",
        tables: QUALITY_TABLES,
    },
    DataFileSpec {
        path: "outbound.jsonl",
        tables: OUTBOUND_TABLES,
    },
    DataFileSpec {
        path: "audit.jsonl",
        tables: AUDIT_TABLES,
    },
];

#[derive(Debug, Deserialize, Serialize)]
struct PackageRow {
    table: String,
    record: Map<String, Value>,
}

pub struct OfflineUpgradeExporter<'a> {
    pool: &'a SqlitePool,
}

impl<'a> OfflineUpgradeExporter<'a> {
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Exports a consistent SQLite snapshot to a newly-created `.invpack`
    /// directory. The final path appears only after the staged package passes
    /// the same validation used before network import.
    pub async fn export(
        &self,
        destination: &Path,
        request: ExportRequest,
    ) -> Result<ExportedPackage, UpgradeError> {
        validate_export_request(destination, &request)?;
        let (parent, final_path) = prepare_destination(destination)?;
        let staging_path = staging_path(&parent, &final_path, &request.export_id)?;
        fs::create_dir(&staging_path).map_err(|source| UpgradeError::Io {
            path: staging_path.clone(),
            source,
        })?;
        let mut guard = StagingGuard::new(staging_path.clone());

        let mut transaction = self.pool.begin().await?;
        let logical_schema_available: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version = ?1 AND success = 1)",
        )
        .bind(REQUIRED_SQLITE_MIGRATION_VERSION)
        .fetch_one(&mut *transaction)
        .await?;
        if !logical_schema_available {
            return Err(UpgradeError::Incompatible(format!(
                "SQLite does not contain required migration version {}",
                REQUIRED_SQLITE_MIGRATION_VERSION
            )));
        }

        let source_instance_id: String =
            sqlx::query_scalar("SELECT source_instance_id FROM workspaces WHERE id = ?1")
                .bind(&request.workspace_id)
                .fetch_one(&mut *transaction)
                .await?;

        let migration_id = deterministic_migration_id(
            &source_instance_id,
            &request.workspace_id,
            &request.export_id,
        );
        let mut files = Vec::with_capacity(DATA_FILES.len());
        for spec in DATA_FILES {
            let path = safe_new_package_file(&staging_path, spec.path)?;
            let rows =
                export_data_file(&mut transaction, &request.workspace_id, spec, &path).await?;
            let stats = hash_jsonl_file(&path)?;
            if rows != stats.rows {
                return Err(UpgradeError::Integrity(format!(
                    "{} row count changed while exporting",
                    spec.path
                )));
            }
            files.push(DataFileDigest {
                path: spec.path.to_owned(),
                rows,
                sha256: stats.sha256,
            });
        }

        let manifest = PackageManifest {
            product: PRODUCT_ID.to_owned(),
            package_format_version: PACKAGE_FORMAT_VERSION,
            logical_schema_version: LOGICAL_SCHEMA_VERSION,
            direction: MigrationDirection::OfflineToNetwork,
            export_id: request.export_id,
            migration_id,
            exported_at: request.exported_at,
            source: SourceIdentity {
                instance_id: source_instance_id,
                workspace_id: request.workspace_id.clone(),
            },
            workspace_mapping: WorkspaceMapping {
                source_workspace_id: request.workspace_id,
                target_workspace_id: None,
                requires_empty_target: true,
            },
            files,
        };

        let manifest_path = safe_new_package_file(&staging_path, MANIFEST_FILE)?;
        let manifest_bytes = canonical_json_bytes(&manifest, &manifest_path)?;
        write_new_file(&manifest_path, &manifest_bytes)?;
        let checksums = ChecksumsDocument {
            package_format_version: PACKAGE_FORMAT_VERSION,
            manifest_sha256: sha256_hex(&manifest_bytes),
            files: manifest.files.clone(),
        };
        let checksums_path = safe_new_package_file(&staging_path, CHECKSUMS_FILE)?;
        let checksums_bytes = canonical_json_bytes(&checksums, &checksums_path)?;
        write_new_file(&checksums_path, &checksums_bytes)?;

        transaction.commit().await?;
        let validated = validate_package_dir(&staging_path, false)?;
        fs::rename(&staging_path, &final_path).map_err(|source| UpgradeError::Io {
            path: final_path.clone(),
            source,
        })?;
        guard.disarm();

        Ok(ExportedPackage {
            path: final_path,
            manifest: validated.manifest,
            package_checksum: validated.package_checksum,
        })
    }
}

/// Performs all local, read-only validation before any PostgreSQL transaction
/// is opened. Database-specific uniqueness, reference and target-workspace
/// checks are repeated against PostgreSQL staging by `import_to_postgres`.
pub fn validate_package(path: &Path) -> Result<ValidatedPackage, UpgradeError> {
    validate_package_dir(path, true)
}

pub fn deterministic_migration_id(
    source_instance_id: &str,
    source_workspace_id: &str,
    export_id: &str,
) -> String {
    let identity = format!(
        "{PRODUCT_ID}\0offline_to_network\0{source_instance_id}\0{source_workspace_id}\0{export_id}"
    );
    format!("invpack_{}", sha256_hex(identity.as_bytes()))
}

async fn export_data_file(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    spec: &DataFileSpec,
    path: &Path,
) -> Result<u64, UpgradeError> {
    let file = File::options()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|source| UpgradeError::Io {
            path: path.to_owned(),
            source,
        })?;
    let mut writer = BufWriter::new(file);
    let mut count = 0_u64;

    for table in spec.tables {
        let sql = format!(
            "SELECT * FROM {} WHERE {} = ?1 ORDER BY {}",
            table.name,
            if table.name == "workspaces" {
                "id"
            } else {
                "workspace_id"
            },
            table.order_by
        );
        let rows = sqlx::query(&sql)
            .bind(workspace_id)
            .fetch_all(&mut **transaction)
            .await?;
        for row in rows {
            let mut record = sqlite_row_to_json(&row)?;
            redact_sensitive_json_columns(table.name, &mut record)?;
            let package_row = PackageRow {
                table: table.name.to_owned(),
                record,
            };
            serde_json::to_writer(&mut writer, &package_row).map_err(|source| {
                UpgradeError::Json {
                    path: path.to_owned(),
                    source,
                }
            })?;
            writer.write_all(b"\n").map_err(|source| UpgradeError::Io {
                path: path.to_owned(),
                source,
            })?;
            count += 1;
        }
    }

    writer.flush().map_err(|source| UpgradeError::Io {
        path: path.to_owned(),
        source,
    })?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|source| UpgradeError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(count)
}

fn sqlite_row_to_json(row: &sqlx::sqlite::SqliteRow) -> Result<Map<String, Value>, UpgradeError> {
    let mut record = Map::new();
    for (index, column) in row.columns().iter().enumerate() {
        let raw = row.try_get_raw(index)?;
        let value = if raw.is_null() {
            Value::Null
        } else {
            match raw.type_info().name() {
                "TEXT" => Value::String(row.try_get::<String, _>(index)?),
                "INTEGER" => Value::Number(row.try_get::<i64, _>(index)?.into()),
                "REAL" => {
                    let number = serde_json::Number::from_f64(row.try_get::<f64, _>(index)?)
                        .ok_or_else(|| {
                            UpgradeError::Data(format!(
                                "{} contains a non-finite REAL value",
                                column.name()
                            ))
                        })?;
                    Value::Number(number)
                }
                "BLOB" => {
                    let bytes = row.try_get::<Vec<u8>, _>(index)?;
                    let mut encoded = Map::new();
                    encoded.insert(
                        "$binary_base64".to_owned(),
                        Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
                    );
                    Value::Object(encoded)
                }
                other => {
                    return Err(UpgradeError::Data(format!(
                        "unsupported SQLite value type {other} in {}",
                        column.name()
                    )))
                }
            }
        };
        record.insert(column.name().to_owned(), value);
    }
    Ok(record)
}

fn redact_sensitive_json_columns(
    table: &str,
    record: &mut Map<String, Value>,
) -> Result<(), UpgradeError> {
    let field = match table {
        "audit_logs" => "details_json",
        "idempotency_records" => "response_json",
        _ => return Ok(()),
    };
    let Some(Value::String(encoded)) = record.get_mut(field) else {
        return Err(UpgradeError::Data(format!(
            "{table}.{field} must be a JSON string"
        )));
    };
    let mut value: Value = serde_json::from_str(encoded).map_err(|source| UpgradeError::Json {
        path: PathBuf::from(format!("{table}.{field}")),
        source,
    })?;
    redact_sensitive_values(&mut value);
    *encoded = serde_json::to_string(&value).map_err(|source| UpgradeError::Json {
        path: PathBuf::from(format!("{table}.{field}")),
        source,
    })?;
    Ok(())
}

fn redact_sensitive_values(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(redact_sensitive_values),
        Value::Object(fields) => {
            for (key, value) in fields {
                let normalized = key.to_ascii_lowercase();
                if normalized == "password"
                    || normalized.ends_with("_password")
                    || normalized.contains("password_hash")
                    || normalized == "token"
                    || normalized.ends_with("_token")
                    || normalized.contains("refresh_token")
                    || normalized == "secret"
                    || normalized.ends_with("_secret")
                    || normalized.contains("credential")
                    || normalized.contains("cookie")
                    || normalized.contains("activation_code")
                    || normalized.contains("license_key")
                    || normalized.contains("entitlement")
                {
                    *value = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_sensitive_values(value);
                }
            }
        }
        _ => {}
    }
}

fn validate_export_request(
    destination: &Path,
    request: &ExportRequest,
) -> Result<(), UpgradeError> {
    if destination.extension().and_then(|value| value.to_str()) != Some("invpack") {
        return Err(UpgradeError::InvalidRequest(
            "destination must end in .invpack".to_owned(),
        ));
    }
    for (field, value) in [
        ("export_id", request.export_id.as_str()),
        ("workspace_id", request.workspace_id.as_str()),
    ] {
        Uuid::parse_str(value).map_err(|_| {
            UpgradeError::InvalidRequest(format!("{field} must be a UUID: {value}"))
        })?;
    }
    if request.exported_at.trim().is_empty() {
        return Err(UpgradeError::InvalidRequest(
            "exported_at is empty".to_owned(),
        ));
    }
    validate_utc_rfc3339(&request.exported_at).map_err(UpgradeError::InvalidRequest)?;
    Ok(())
}

fn validate_utc_rfc3339(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.last() != Some(&b'Z')
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return Err(format!(
            "timestamp must be UTC RFC 3339 with Z suffix: {value}"
        ));
    }
    for range in [0..4, 5..7, 8..10, 11..13, 14..16, 17..19] {
        if !bytes[range].iter().all(u8::is_ascii_digit) {
            return Err(format!("timestamp contains invalid digits: {value}"));
        }
    }
    if bytes.len() > 20
        && (bytes.get(19) != Some(&b'.')
            || !bytes[20..bytes.len() - 1].iter().all(u8::is_ascii_digit)
            || bytes.len() == 21)
    {
        return Err(format!(
            "timestamp has an invalid fractional second: {value}"
        ));
    }
    let number = |range: std::ops::Range<usize>| -> u32 {
        std::str::from_utf8(&bytes[range])
            .expect("ASCII digits are UTF-8")
            .parse()
            .expect("digit range is a number")
    };
    let year = number(0..4);
    let month = number(5..7);
    let day = number(8..10);
    let hour = number(11..13);
    let minute = number(14..16);
    let second = number(17..19);
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => 0,
    };
    if day == 0 || day > days_in_month || hour > 23 || minute > 59 || second > 59 {
        return Err(format!(
            "timestamp is outside the valid UTC calendar: {value}"
        ));
    }
    Ok(())
}

fn prepare_destination(destination: &Path) -> Result<(PathBuf, PathBuf), UpgradeError> {
    if destination.file_name().is_none() {
        return Err(UpgradeError::UnsafePath(
            "destination has no file name".to_owned(),
        ));
    }
    match fs::symlink_metadata(destination) {
        Ok(_) => return Err(UpgradeError::DestinationExists(destination.to_owned())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(UpgradeError::Io {
                path: destination.to_owned(),
                source,
            })
        }
    }

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|source| UpgradeError::Io {
        path: parent.to_owned(),
        source,
    })?;
    let final_path = parent.join(destination.file_name().expect("checked above"));
    Ok((parent, final_path))
}

fn staging_path(
    parent: &Path,
    destination: &Path,
    export_id: &str,
) -> Result<PathBuf, UpgradeError> {
    let file_name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| UpgradeError::UnsafePath("destination is not valid UTF-8".to_owned()))?;
    let token = &sha256_hex(export_id.as_bytes())[..16];
    let candidate = parent.join(format!(".{file_name}.{token}.staging"));
    match fs::symlink_metadata(&candidate) {
        Ok(_) => Err(UpgradeError::DestinationExists(candidate)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(candidate),
        Err(source) => Err(UpgradeError::Io {
            path: candidate,
            source,
        }),
    }
}

fn validate_relative_path(relative_path: &str) -> Result<(), UpgradeError> {
    if relative_path.is_empty() || relative_path.contains('\0') {
        return Err(UpgradeError::UnsafePath(
            "package path is empty or contains NUL".to_owned(),
        ));
    }
    let path = Path::new(relative_path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(UpgradeError::UnsafePath(relative_path.to_owned()));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(UpgradeError::UnsafePath(relative_path.to_owned()));
    }
    Ok(())
}

fn safe_new_package_file(root: &Path, relative_path: &str) -> Result<PathBuf, UpgradeError> {
    validate_relative_path(relative_path)?;
    let root_metadata = fs::symlink_metadata(root).map_err(|source| UpgradeError::Io {
        path: root.to_owned(),
        source,
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(UpgradeError::UnsafePath(format!(
            "package root is not a real directory: {}",
            root.display()
        )));
    }
    let path = root.join(relative_path);
    if path.parent() != Some(root) {
        return Err(UpgradeError::UnsafePath(relative_path.to_owned()));
    }
    match fs::symlink_metadata(&path) {
        Ok(_) => Err(UpgradeError::DestinationExists(path)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(source) => Err(UpgradeError::Io { path, source }),
    }
}

fn safe_existing_package_file(root: &Path, relative_path: &str) -> Result<PathBuf, UpgradeError> {
    validate_relative_path(relative_path)?;
    let path = root.join(relative_path);
    let metadata = fs::symlink_metadata(&path).map_err(|source| UpgradeError::Io {
        path: path.clone(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UpgradeError::UnsafePath(format!(
            "package member is not a regular file: {}",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(&path).map_err(|source| UpgradeError::Io {
        path: path.clone(),
        source,
    })?;
    if !canonical.starts_with(root) {
        return Err(UpgradeError::UnsafePath(relative_path.to_owned()));
    }
    Ok(canonical)
}

fn canonical_json_bytes<T: Serialize>(value: &T, path: &Path) -> Result<Vec<u8>, UpgradeError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| UpgradeError::Json {
        path: path.to_owned(),
        source,
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), UpgradeError> {
    let mut file = File::options()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|source| UpgradeError::Io {
            path: path.to_owned(),
            source,
        })?;
    file.write_all(bytes).map_err(|source| UpgradeError::Io {
        path: path.to_owned(),
        source,
    })?;
    file.sync_all().map_err(|source| UpgradeError::Io {
        path: path.to_owned(),
        source,
    })
}

fn read_limited(path: &Path, max_bytes: u64) -> Result<Vec<u8>, UpgradeError> {
    let metadata = fs::metadata(path).map_err(|source| UpgradeError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.len() > max_bytes {
        return Err(UpgradeError::Integrity(format!(
            "{} exceeds the {} byte metadata limit",
            path.display(),
            max_bytes
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|source| UpgradeError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(bytes)
}

#[derive(Debug)]
struct FileStats {
    rows: u64,
    sha256: String,
}

fn hash_jsonl_file(path: &Path) -> Result<FileStats, UpgradeError> {
    let file = File::open(path).map_err(|source| UpgradeError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut rows = 0_u64;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = reader
            .read_until(b'\n', &mut buffer)
            .map_err(|source| UpgradeError::Io {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            break;
        }
        if read > MAX_JSONL_LINE_BYTES {
            return Err(UpgradeError::Integrity(format!(
                "{} contains a JSONL line larger than {} bytes",
                path.display(),
                MAX_JSONL_LINE_BYTES
            )));
        }
        if buffer.last() != Some(&b'\n') {
            return Err(UpgradeError::Integrity(format!(
                "{} must end every JSONL record with a newline",
                path.display()
            )));
        }
        hasher.update(&buffer);
        rows += 1;
    }
    Ok(FileStats {
        rows,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_package_dir(
    path: &Path,
    require_invpack_extension: bool,
) -> Result<ValidatedPackage, UpgradeError> {
    if require_invpack_extension
        && path.extension().and_then(|value| value.to_str()) != Some("invpack")
    {
        return Err(UpgradeError::UnsafePath(
            "package directory must end in .invpack".to_owned(),
        ));
    }
    let root_metadata = fs::symlink_metadata(path).map_err(|source| UpgradeError::Io {
        path: path.to_owned(),
        source,
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(UpgradeError::UnsafePath(format!(
            "package root is not a real directory: {}",
            path.display()
        )));
    }
    let root = fs::canonicalize(path).map_err(|source| UpgradeError::Io {
        path: path.to_owned(),
        source,
    })?;

    let expected_members: BTreeSet<&str> = DATA_FILES
        .iter()
        .map(|spec| spec.path)
        .chain([MANIFEST_FILE, CHECKSUMS_FILE])
        .collect();
    let mut actual_members = BTreeSet::new();
    for entry in fs::read_dir(&root).map_err(|source| UpgradeError::Io {
        path: root.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| UpgradeError::Io {
            path: root.clone(),
            source,
        })?;
        let name = entry.file_name().into_string().map_err(|_| {
            UpgradeError::UnsafePath("package contains a non-UTF-8 member".to_owned())
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|source| UpgradeError::Io {
            path: entry.path(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(UpgradeError::UnsafePath(format!(
                "unexpected non-file package member: {name}"
            )));
        }
        actual_members.insert(name);
    }
    let expected_owned: BTreeSet<String> = expected_members
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    if actual_members != expected_owned {
        return Err(UpgradeError::Integrity(format!(
            "package members differ: expected {expected_owned:?}, got {actual_members:?}"
        )));
    }

    let manifest_path = safe_existing_package_file(&root, MANIFEST_FILE)?;
    let manifest_bytes = read_limited(&manifest_path, MAX_METADATA_BYTES)?;
    let manifest: PackageManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|source| UpgradeError::Json {
            path: manifest_path.clone(),
            source,
        })?;
    if canonical_json_bytes(&manifest, &manifest_path)? != manifest_bytes {
        return Err(UpgradeError::Integrity(
            "manifest.json is not in canonical package encoding".to_owned(),
        ));
    }

    let checksums_path = safe_existing_package_file(&root, CHECKSUMS_FILE)?;
    let checksums_bytes = read_limited(&checksums_path, MAX_METADATA_BYTES)?;
    let checksums: ChecksumsDocument =
        serde_json::from_slice(&checksums_bytes).map_err(|source| UpgradeError::Json {
            path: checksums_path.clone(),
            source,
        })?;
    if canonical_json_bytes(&checksums, &checksums_path)? != checksums_bytes {
        return Err(UpgradeError::Integrity(
            "checksums.json is not in canonical package encoding".to_owned(),
        ));
    }

    validate_metadata(&manifest, &checksums, &manifest_bytes)?;
    let mut all_records: BTreeMap<String, Vec<Map<String, Value>>> = BTreeMap::new();
    let mut entity_counts = BTreeMap::new();
    for (spec, expected) in DATA_FILES.iter().zip(&manifest.files) {
        let data_path = safe_existing_package_file(&root, spec.path)?;
        let actual = hash_jsonl_file(&data_path)?;
        if actual.rows != expected.rows || actual.sha256 != expected.sha256 {
            return Err(UpgradeError::Integrity(format!(
                "{} expected {} rows/{} but found {} rows/{}",
                spec.path, expected.rows, expected.sha256, actual.rows, actual.sha256
            )));
        }
        read_and_validate_jsonl(&data_path, spec, &mut all_records, &mut entity_counts)?;
    }
    validate_relational_data(&manifest, &all_records)?;

    Ok(ValidatedPackage {
        root,
        manifest,
        package_checksum: sha256_hex(&checksums_bytes),
        entity_counts,
    })
}

fn validate_metadata(
    manifest: &PackageManifest,
    checksums: &ChecksumsDocument,
    manifest_bytes: &[u8],
) -> Result<(), UpgradeError> {
    if manifest.product != PRODUCT_ID {
        return Err(UpgradeError::Incompatible(format!(
            "product {} is not {PRODUCT_ID}",
            manifest.product
        )));
    }
    if manifest.package_format_version != checksums.package_format_version {
        return Err(UpgradeError::Integrity(
            "manifest and checksums package format versions differ".to_owned(),
        ));
    }
    if !(MIN_SUPPORTED_PACKAGE_FORMAT_VERSION..=PACKAGE_FORMAT_VERSION)
        .contains(&manifest.package_format_version)
    {
        return Err(UpgradeError::Incompatible(format!(
            "package format {} is not supported; expected {} through {}",
            manifest.package_format_version,
            MIN_SUPPORTED_PACKAGE_FORMAT_VERSION,
            PACKAGE_FORMAT_VERSION,
        )));
    }
    if manifest.logical_schema_version != LOGICAL_SCHEMA_VERSION {
        return Err(UpgradeError::Incompatible(format!(
            "logical schema {} is not supported; expected {}",
            manifest.logical_schema_version, LOGICAL_SCHEMA_VERSION
        )));
    }
    if manifest.direction != MigrationDirection::OfflineToNetwork {
        return Err(UpgradeError::Incompatible(
            "only offline_to_network is accepted".to_owned(),
        ));
    }
    for (field, value) in [
        ("export_id", manifest.export_id.as_str()),
        ("source.instance_id", manifest.source.instance_id.as_str()),
        ("source.workspace_id", manifest.source.workspace_id.as_str()),
    ] {
        Uuid::parse_str(value)
            .map_err(|_| UpgradeError::Data(format!("{field} is not a UUID: {value}")))?;
    }
    if manifest.exported_at.trim().is_empty() {
        return Err(UpgradeError::Data("exported_at is empty".to_owned()));
    }
    validate_utc_rfc3339(&manifest.exported_at).map_err(UpgradeError::Data)?;
    if manifest.workspace_mapping.source_workspace_id != manifest.source.workspace_id
        || manifest.workspace_mapping.target_workspace_id.is_some()
        || !manifest.workspace_mapping.requires_empty_target
    {
        return Err(UpgradeError::Data(
            "this package format requires the source workspace and an empty, unassigned target workspace"
                .to_owned(),
        ));
    }
    let expected_migration_id = deterministic_migration_id(
        &manifest.source.instance_id,
        &manifest.source.workspace_id,
        &manifest.export_id,
    );
    if manifest.migration_id != expected_migration_id {
        return Err(UpgradeError::Integrity(format!(
            "migration_id does not match its source identity: expected {expected_migration_id}"
        )));
    }
    let expected_paths: Vec<&str> = DATA_FILES.iter().map(|spec| spec.path).collect();
    let manifest_paths: Vec<&str> = manifest
        .files
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    if manifest_paths != expected_paths {
        return Err(UpgradeError::Integrity(format!(
            "manifest data files must be exactly {expected_paths:?}"
        )));
    }
    if checksums.manifest_sha256 != sha256_hex(manifest_bytes) {
        return Err(UpgradeError::Integrity(
            "manifest.json SHA-256 does not match checksums.json".to_owned(),
        ));
    }
    if checksums.files != manifest.files {
        return Err(UpgradeError::Integrity(
            "manifest and checksums data-file entries differ".to_owned(),
        ));
    }
    for entry in &manifest.files {
        validate_relative_path(&entry.path)?;
        if entry.sha256.len() != 64 || !entry.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(UpgradeError::Integrity(format!(
                "{} has an invalid SHA-256",
                entry.path
            )));
        }
    }
    Ok(())
}

fn read_and_validate_jsonl(
    path: &Path,
    spec: &DataFileSpec,
    all_records: &mut BTreeMap<String, Vec<Map<String, Value>>>,
    entity_counts: &mut BTreeMap<String, u64>,
) -> Result<(), UpgradeError> {
    let allowed: HashSet<&str> = spec.tables.iter().map(|table| table.name).collect();
    let order: HashMap<&str, usize> = spec
        .tables
        .iter()
        .enumerate()
        .map(|(index, table)| (table.name, index))
        .collect();
    let mut last_table_index = 0_usize;
    let mut seen_any = false;
    let reader = BufReader::new(File::open(path).map_err(|source| UpgradeError::Io {
        path: path.to_owned(),
        source,
    })?);
    for (line_index, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| UpgradeError::Io {
            path: path.to_owned(),
            source,
        })?;
        if line.is_empty() {
            return Err(UpgradeError::Data(format!(
                "{} line {} is empty",
                path.display(),
                line_index + 1
            )));
        }
        let row: PackageRow = serde_json::from_str(&line).map_err(|source| UpgradeError::Json {
            path: path.to_owned(),
            source,
        })?;
        if !allowed.contains(row.table.as_str()) {
            return Err(UpgradeError::Data(format!(
                "{} contains table {} in the wrong data file",
                path.display(),
                row.table
            )));
        }
        let table_index = order[&row.table.as_str()];
        if seen_any && table_index < last_table_index {
            return Err(UpgradeError::Data(format!(
                "{} table records are not in canonical order",
                path.display()
            )));
        }
        seen_any = true;
        last_table_index = table_index;
        *entity_counts.entry(row.table.clone()).or_default() += 1;
        all_records.entry(row.table).or_default().push(row.record);
    }
    Ok(())
}

fn validate_relational_data(
    manifest: &PackageManifest,
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<(), UpgradeError> {
    let workspaces = table_records(records, "workspaces");
    if workspaces.len() != 1 {
        return Err(UpgradeError::Data(format!(
            "package must contain exactly one workspace, found {}",
            workspaces.len()
        )));
    }
    let workspace = &workspaces[0];
    if required_string(workspace, "workspaces", "id")? != manifest.source.workspace_id
        || required_string(workspace, "workspaces", "source_instance_id")?
            != manifest.source.instance_id
    {
        return Err(UpgradeError::Data(
            "workspace identity differs from manifest source".to_owned(),
        ));
    }

    let mut ids: BTreeMap<&str, HashSet<String>> = BTreeMap::new();
    for table in all_table_names() {
        if table == "party_roles" {
            continue;
        }
        let mut table_ids = HashSet::new();
        for record in table_records(records, table) {
            let id = required_string(record, table, "id")?.to_owned();
            Uuid::parse_str(&id)
                .map_err(|_| UpgradeError::Data(format!("{table}.id is not a UUID: {id}")))?;
            if !table_ids.insert(id.clone()) {
                return Err(UpgradeError::Data(format!(
                    "duplicate primary key {id} in {table}"
                )));
            }
            if table != "workspaces" {
                let workspace_id = required_string(record, table, "workspace_id")?;
                if workspace_id != manifest.source.workspace_id {
                    return Err(UpgradeError::Data(format!(
                        "{table}.{id} belongs to workspace {workspace_id}, not {}",
                        manifest.source.workspace_id
                    )));
                }
            }
        }
        ids.insert(table, table_ids);
    }
    for record in table_records(records, "party_roles") {
        if required_string(record, "party_roles", "workspace_id")? != manifest.source.workspace_id {
            return Err(UpgradeError::Data(
                "party_roles contains another workspace".to_owned(),
            ));
        }
    }

    for (table, fields) in [
        ("party_roles", &["party_id", "role"][..]),
        ("business_parties", &["workspace_id", "normalized_name"]),
        ("skus", &["workspace_id", "code"]),
        ("warehouses", &["workspace_id", "code"]),
        ("locations", &["workspace_id", "warehouse_id", "code"]),
        ("inbound_receipts", &["workspace_id", "receipt_no"]),
        ("inbound_receipts", &["workspace_id", "idempotency_key"]),
        ("inventory_units", &["workspace_id", "barcode"]),
        ("quality_labels", &["workspace_id", "normalized_name"]),
        ("quality_inspections", &["workspace_id", "inspection_no"]),
        ("quality_inspections", &["workspace_id", "idempotency_key"]),
        (
            "quality_inspection_results",
            &["inspection_id", "inventory_unit_id"],
        ),
        ("outbound_orders", &["workspace_id", "order_no"]),
        ("outbound_orders", &["workspace_id", "idempotency_key"]),
        ("outbound_shipments", &["workspace_id", "shipment_no"]),
        ("outbound_shipments", &["workspace_id", "idempotency_key"]),
        (
            "delivery_confirmations",
            &["workspace_id", "confirmation_code"],
        ),
        (
            "delivery_confirmations",
            &["workspace_id", "idempotency_key"],
        ),
        (
            "delivery_confirmation_lines",
            &["outbound_shipment_line_id"],
        ),
        ("outbound_return_batches", &["workspace_id", "return_no"]),
        (
            "outbound_return_batches",
            &["workspace_id", "idempotency_key"],
        ),
        ("outbound_return_lines", &["outbound_shipment_line_id"]),
        (
            "idempotency_records",
            &["workspace_id", "scope", "idempotency_key"],
        ),
    ] {
        validate_unique_fields(records, table, fields)?;
    }
    validate_active_allocation_uniqueness(records)?;
    validate_active_shipment_line_uniqueness(records)?;

    for (table, field, target) in [
        ("party_roles", "party_id", "business_parties"),
        ("locations", "warehouse_id", "warehouses"),
        ("inbound_receipts", "owner_party_id", "business_parties"),
        ("inbound_receipts", "warehouse_id", "warehouses"),
        ("inbound_receipts", "supplier_party_id", "business_parties"),
        ("inbound_receipt_lines", "receipt_id", "inbound_receipts"),
        ("inbound_receipt_lines", "sku_id", "skus"),
        (
            "inventory_units",
            "inbound_receipt_line_id",
            "inbound_receipt_lines",
        ),
        ("inventory_units", "owner_party_id", "business_parties"),
        ("inventory_units", "sku_id", "skus"),
        ("inventory_units", "location_id", "locations"),
        (
            "quality_inspection_results",
            "inspection_id",
            "quality_inspections",
        ),
        (
            "quality_inspection_results",
            "inventory_unit_id",
            "inventory_units",
        ),
        (
            "quality_inspection_results",
            "quality_label_id",
            "quality_labels",
        ),
        (
            "quality_label_name_history",
            "quality_label_id",
            "quality_labels",
        ),
        ("quality_waivers", "inventory_unit_id", "inventory_units"),
        (
            "outbound_orders",
            "upstream_receiver_id",
            "business_parties",
        ),
        (
            "outbound_order_lines",
            "outbound_order_id",
            "outbound_orders",
        ),
        ("outbound_order_lines", "sku_id", "skus"),
        (
            "outbound_allocations",
            "outbound_order_line_id",
            "outbound_order_lines",
        ),
        (
            "outbound_allocations",
            "inventory_unit_id",
            "inventory_units",
        ),
        ("outbound_shipments", "outbound_order_id", "outbound_orders"),
        (
            "outbound_shipment_lines",
            "outbound_shipment_id",
            "outbound_shipments",
        ),
        (
            "outbound_shipment_lines",
            "outbound_allocation_id",
            "outbound_allocations",
        ),
        (
            "outbound_shipment_lines",
            "inventory_unit_id",
            "inventory_units",
        ),
        (
            "delivery_confirmations",
            "outbound_shipment_id",
            "outbound_shipments",
        ),
        (
            "delivery_confirmation_lines",
            "delivery_confirmation_id",
            "delivery_confirmations",
        ),
        (
            "delivery_confirmation_lines",
            "outbound_shipment_line_id",
            "outbound_shipment_lines",
        ),
        (
            "outbound_return_lines",
            "return_batch_id",
            "outbound_return_batches",
        ),
        (
            "outbound_return_lines",
            "outbound_shipment_line_id",
            "outbound_shipment_lines",
        ),
        (
            "outbound_return_lines",
            "inventory_unit_id",
            "inventory_units",
        ),
        ("stock_movements", "inventory_unit_id", "inventory_units"),
        ("document_voids", "inbound_receipt_id", "inbound_receipts"),
        ("document_voids", "outbound_order_id", "outbound_orders"),
    ] {
        validate_foreign_key(
            records,
            &ids,
            table,
            field,
            target,
            (table == "inbound_receipts" && field == "supplier_party_id")
                || (table == "quality_inspection_results" && field == "quality_label_id")
                || table == "document_voids",
        )?;
    }
    for field in ["from_location_id", "to_location_id"] {
        validate_foreign_key(records, &ids, "stock_movements", field, "locations", true)?;
    }

    validate_quality_label_snapshots(records)?;
    validate_document_voids(records)?;
    validate_quality_label_name_history(records)?;
    validate_inbound_quantities(records)?;
    validate_outbound_quantities(records)?;
    validate_inventory_projections(records)?;
    Ok(())
}

fn validate_document_voids(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<(), UpgradeError> {
    let mut receipts = HashSet::new();
    let mut orders = HashSet::new();
    for record in table_records(records, "document_voids") {
        let kind = required_string(record, "document_voids", "document_kind")?;
        let receipt_id = record.get("inbound_receipt_id").and_then(Value::as_str);
        let order_id = record.get("outbound_order_id").and_then(Value::as_str);
        match (kind, receipt_id, order_id) {
            ("inbound_receipt", Some(receipt_id), None) if receipts.insert(receipt_id) => {}
            ("outbound_order", None, Some(order_id)) if orders.insert(order_id) => {}
            _ => {
                return Err(UpgradeError::Data(
                    "document_voids must identify one unique document matching document_kind"
                        .to_owned(),
                ));
            }
        }
        if required_string(record, "document_voids", "reason")?
            .trim()
            .is_empty()
        {
            return Err(UpgradeError::Data(
                "document_voids.reason must not be empty".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_quality_label_snapshots(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<(), UpgradeError> {
    let labels: HashMap<&str, &str> = table_records(records, "quality_labels")
        .iter()
        .map(|record| {
            Ok((
                required_string(record, "quality_labels", "id")?,
                required_string(record, "quality_labels", "disposition")?,
            ))
        })
        .collect::<Result<_, UpgradeError>>()?;
    for record in table_records(records, "quality_inspection_results") {
        let label_id = record.get("quality_label_id").and_then(Value::as_str);
        let snapshot = record.get("quality_label_snapshot").and_then(Value::as_str);
        match (label_id, snapshot) {
            (None, None) => continue,
            (Some(label_id), Some(snapshot)) if !snapshot.trim().is_empty() => {
                let disposition = labels.get(label_id).ok_or_else(|| {
                    UpgradeError::Data(format!(
                        "quality_inspection_results.quality_label_id references missing quality_labels.{label_id}"
                    ))
                })?;
                let result = required_string(record, "quality_inspection_results", "result")?;
                let expected = match *disposition {
                    "available" => "passed",
                    "quarantine" => "failed",
                    other => {
                        return Err(UpgradeError::Data(format!(
                            "quality_labels.{label_id} has unknown disposition {other}"
                        )))
                    }
                };
                if result != expected {
                    return Err(UpgradeError::Data(format!(
                        "quality inspection label {label_id} disposition {disposition} does not match result {result}"
                    )));
                }
            }
            _ => {
                return Err(UpgradeError::Data(
                    "quality inspection label id and non-empty snapshot must appear together"
                        .to_owned(),
                ))
            }
        }
    }
    Ok(())
}

fn validate_quality_label_name_history(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<(), UpgradeError> {
    for record in table_records(records, "quality_label_name_history") {
        let old_name = required_string(record, "quality_label_name_history", "old_name")?;
        let new_name = required_string(record, "quality_label_name_history", "new_name")?;
        if old_name == new_name {
            return Err(UpgradeError::Data(
                "quality_label_name_history old_name and new_name must differ".to_owned(),
            ));
        }
        if old_name.chars().count() > 40 || new_name.chars().count() > 40 {
            return Err(UpgradeError::Data(
                "quality_label_name_history names must be at most 40 characters".to_owned(),
            ));
        }
        let actor = required_string(record, "quality_label_name_history", "changed_by_snapshot")?;
        if actor.chars().count() > 100 {
            return Err(UpgradeError::Data(
                "quality_label_name_history changed_by_snapshot must be at most 100 characters"
                    .to_owned(),
            ));
        }
        let note = record
            .get("change_note")
            .filter(|value| !value.is_null())
            .map(|value| {
                value.as_str().ok_or_else(|| {
                    UpgradeError::Data(
                        "quality_label_name_history.change_note must be a string or null"
                            .to_owned(),
                    )
                })
            })
            .transpose()?;
        if let Some(note) = note {
            if note.chars().count() > 200 {
                return Err(UpgradeError::Data(
                    "quality_label_name_history change_note must be at most 200 characters"
                        .to_owned(),
                ));
            }
        }
        validate_utc_rfc3339(required_string(
            record,
            "quality_label_name_history",
            "changed_at",
        )?)
        .map_err(UpgradeError::Data)?;
    }
    Ok(())
}

fn all_table_names() -> impl Iterator<Item = &'static str> {
    DATA_FILES
        .iter()
        .flat_map(|file| file.tables.iter().map(|table| table.name))
}

fn table_records<'a>(
    records: &'a BTreeMap<String, Vec<Map<String, Value>>>,
    table: &str,
) -> &'a [Map<String, Value>] {
    records.get(table).map(Vec::as_slice).unwrap_or(&[])
}

fn required_value<'a>(
    record: &'a Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<&'a Value, UpgradeError> {
    record
        .get(field)
        .filter(|value| !value.is_null())
        .ok_or_else(|| UpgradeError::Data(format!("{table}.{field} is missing or null")))
}

fn required_string<'a>(
    record: &'a Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<&'a str, UpgradeError> {
    required_value(record, table, field)?
        .as_str()
        .ok_or_else(|| UpgradeError::Data(format!("{table}.{field} must be a string")))
}

fn required_i64(
    record: &Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<i64, UpgradeError> {
    required_value(record, table, field)?
        .as_i64()
        .ok_or_else(|| UpgradeError::Data(format!("{table}.{field} must be an integer")))
}

fn validate_unique_fields(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
    table: &str,
    fields: &[&str],
) -> Result<(), UpgradeError> {
    let mut keys = HashSet::new();
    for record in table_records(records, table) {
        let mut key = String::new();
        for field in fields {
            let value = required_value(record, table, field)?;
            key.push_str(&serde_json::to_string(value).expect("JSON values always serialize"));
            key.push('\0');
        }
        if !keys.insert(key) {
            return Err(UpgradeError::Data(format!(
                "{table} has duplicate unique fields {}",
                fields.join(", ")
            )));
        }
    }
    Ok(())
}

fn validate_active_allocation_uniqueness(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<(), UpgradeError> {
    let mut inventory_units = HashSet::new();
    for record in table_records(records, "outbound_allocations") {
        let status = required_string(record, "outbound_allocations", "status")?;
        if matches!(status, "active" | "shipped") {
            let unit = required_string(record, "outbound_allocations", "inventory_unit_id")?;
            if !inventory_units.insert(unit) {
                return Err(UpgradeError::Data(format!(
                    "inventory unit {unit} has more than one active allocation"
                )));
            }
        }
    }
    Ok(())
}

fn validate_active_shipment_line_uniqueness(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<(), UpgradeError> {
    let returned_line_ids: HashSet<&str> = table_records(records, "outbound_return_lines")
        .iter()
        .map(|record| required_string(record, "outbound_return_lines", "outbound_shipment_line_id"))
        .collect::<Result<_, UpgradeError>>()?;
    let mut active_units = HashSet::new();
    for record in table_records(records, "outbound_shipment_lines") {
        let line_id = required_string(record, "outbound_shipment_lines", "id")?;
        if returned_line_ids.contains(line_id) {
            continue;
        }
        let unit = required_string(record, "outbound_shipment_lines", "inventory_unit_id")?;
        if !active_units.insert(unit) {
            return Err(UpgradeError::Data(format!(
                "inventory unit {unit} has more than one active shipment line"
            )));
        }
    }
    Ok(())
}

fn validate_foreign_key(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
    ids: &BTreeMap<&str, HashSet<String>>,
    table: &str,
    field: &str,
    target: &str,
    nullable: bool,
) -> Result<(), UpgradeError> {
    let target_ids = ids
        .get(target)
        .expect("foreign-key target table is indexed");
    for record in table_records(records, table) {
        let Some(value) = record.get(field) else {
            if nullable {
                continue;
            }
            return Err(UpgradeError::Data(format!("{table}.{field} is missing")));
        };
        if nullable && value.is_null() {
            continue;
        }
        let foreign_id = value.as_str().ok_or_else(|| {
            UpgradeError::Data(format!("{table}.{field} must be a string or null"))
        })?;
        if !target_ids.contains(foreign_id) {
            return Err(UpgradeError::Data(format!(
                "{table}.{field} references missing {target}.{foreign_id}"
            )));
        }
    }
    Ok(())
}

fn validate_inbound_quantities(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<(), UpgradeError> {
    let mut units_by_line: HashMap<&str, i64> = HashMap::new();
    for unit in table_records(records, "inventory_units") {
        *units_by_line
            .entry(required_string(
                unit,
                "inventory_units",
                "inbound_receipt_line_id",
            )?)
            .or_default() += 1;
    }
    for line in table_records(records, "inbound_receipt_lines") {
        let id = required_string(line, "inbound_receipt_lines", "id")?;
        let declared = required_i64(line, "inbound_receipt_lines", "declared_quantity")?;
        let scanned = required_i64(line, "inbound_receipt_lines", "scanned_quantity")?;
        if declared <= 0 || scanned < 0 || scanned > declared {
            return Err(UpgradeError::Data(format!(
                "inbound line {id} has invalid declared/scanned quantities {declared}/{scanned}"
            )));
        }
        let unit_count = units_by_line.get(id).copied().unwrap_or_default();
        if scanned != unit_count {
            return Err(UpgradeError::Data(format!(
                "inbound line {id} says {scanned} scanned but contains {unit_count} inventory units"
            )));
        }
    }
    Ok(())
}

fn validate_outbound_quantities(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<(), UpgradeError> {
    for line in table_records(records, "outbound_order_lines") {
        let id = required_string(line, "outbound_order_lines", "id")?;
        let required = required_i64(line, "outbound_order_lines", "required_quantity")?;
        let allocated = required_i64(line, "outbound_order_lines", "allocated_quantity")?;
        let shipped = required_i64(line, "outbound_order_lines", "shipped_quantity")?;
        let delivered = required_i64(line, "outbound_order_lines", "delivered_quantity")?;
        if required <= 0
            || allocated < 0
            || delivered > shipped
            || shipped > allocated
            || allocated > required
        {
            return Err(UpgradeError::Data(format!(
                "outbound line {id} has invalid required/allocated/shipped/delivered quantities"
            )));
        }
    }
    Ok(())
}

fn validate_inventory_projections(
    records: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<(), UpgradeError> {
    let receipt_by_id: HashMap<&str, &Map<String, Value>> =
        table_records(records, "inbound_receipts")
            .iter()
            .map(|record| Ok((required_string(record, "inbound_receipts", "id")?, record)))
            .collect::<Result<_, UpgradeError>>()?;
    let line_by_id: HashMap<&str, &Map<String, Value>> =
        table_records(records, "inbound_receipt_lines")
            .iter()
            .map(|record| {
                Ok((
                    required_string(record, "inbound_receipt_lines", "id")?,
                    record,
                ))
            })
            .collect::<Result<_, UpgradeError>>()?;
    for unit in table_records(records, "inventory_units") {
        let unit_id = required_string(unit, "inventory_units", "id")?;
        let line_id = required_string(unit, "inventory_units", "inbound_receipt_line_id")?;
        let line = line_by_id[line_id];
        let receipt_id = required_string(line, "inbound_receipt_lines", "receipt_id")?;
        let receipt = receipt_by_id[receipt_id];
        if required_string(unit, "inventory_units", "sku_id")?
            != required_string(line, "inbound_receipt_lines", "sku_id")?
            || required_string(unit, "inventory_units", "owner_party_id")?
                != required_string(receipt, "inbound_receipts", "owner_party_id")?
        {
            return Err(UpgradeError::Data(format!(
                "inventory unit {unit_id} does not match its inbound line SKU/owner"
            )));
        }
        if required_i64(unit, "inventory_units", "version")? <= 0 {
            return Err(UpgradeError::Data(format!(
                "inventory unit {unit_id} has a non-positive version"
            )));
        }
    }
    Ok(())
}

struct StagingGuard {
    path: PathBuf,
    armed: bool,
}

impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetworkUpgradeTarget {
    pub tenant_id: String,
    pub workspace_id: String,
    pub actor_id: String,
}

#[derive(Clone, Debug)]
pub struct MigrationClaim {
    pub migration_id: String,
    pub export_id: String,
    pub source_instance_id: String,
    pub source_workspace_id: String,
    pub package_checksum: String,
    pub logical_schema_version: i64,
    pub target: NetworkUpgradeTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationClaimState {
    New,
    AlreadyImported {
        package_checksum: String,
        imported_at: String,
    },
}

#[derive(Clone, Debug)]
pub struct StagedDataFile {
    pub path: PathBuf,
    pub digest: DataFileDigest,
}

#[derive(Clone, Debug)]
pub struct PostgresStagingReport {
    pub file_rows: BTreeMap<String, u64>,
    pub target_workspace_is_empty: bool,
    pub uniqueness_validated: bool,
    pub references_validated: bool,
    pub quantities_validated: bool,
}

/// A PostgreSQL transaction dedicated to one upgrade attempt.
///
/// Implementations must use one real database transaction for every method,
/// must not commit inside individual methods, and must roll back when dropped.
/// `claim_migration` must serialize concurrent claims for both `migration_id`
/// and `export_id`. `stage_file` writes only to transaction-local staging;
/// `apply_staged_package` is the sole step that copies staged rows into live
/// tenant tables.
pub trait PostgresUpgradeTransaction {
    type Error;

    fn claim_migration(
        &mut self,
        claim: MigrationClaim,
    ) -> impl Future<Output = Result<MigrationClaimState, Self::Error>> + Send;

    fn stage_file(
        &mut self,
        file: StagedDataFile,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn validate_staging(
        &mut self,
        manifest: PackageManifest,
        target: NetworkUpgradeTarget,
    ) -> impl Future<Output = Result<PostgresStagingReport, Self::Error>> + Send;

    fn apply_staged_package(
        &mut self,
        manifest: PackageManifest,
        target: NetworkUpgradeTarget,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn record_imported(
        &mut self,
        claim: MigrationClaim,
    ) -> impl Future<Output = Result<String, Self::Error>> + Send;

    fn commit(self) -> impl Future<Output = Result<(), Self::Error>> + Send
    where
        Self: Sized;
}

/// PostgreSQL repository boundary for the network edition. The transaction
/// type makes it impossible for the orchestrator to switch connections between
/// staging, validation, live-table writes and migration bookkeeping.
pub trait PostgresUpgradeAdapter {
    type Error;
    type Transaction<'a>: PostgresUpgradeTransaction<Error = Self::Error> + Send + 'a
    where
        Self: 'a;

    fn begin_upgrade(
        &self,
    ) -> impl Future<Output = Result<Self::Transaction<'_>, Self::Error>> + Send;
}

#[derive(Debug)]
pub enum PostgresImportError<E> {
    Adapter(E),
    IdempotencyConflict {
        migration_id: String,
        expected_checksum: String,
        imported_checksum: String,
    },
    StagingRejected(String),
}

impl<E: Display> Display for PostgresImportError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Adapter(error) => write!(formatter, "PostgreSQL upgrade adapter failed: {error}"),
            Self::IdempotencyConflict {
                migration_id,
                expected_checksum,
                imported_checksum,
            } => write!(
                formatter,
                "migration {migration_id} was already used for checksum {imported_checksum}, not {expected_checksum}"
            ),
            Self::StagingRejected(reason) => {
                write!(formatter, "PostgreSQL staging validation rejected the package: {reason}")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for PostgresImportError<E> {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportOutcome {
    Imported {
        migration_id: String,
        imported_at: String,
    },
    AlreadyImported {
        migration_id: String,
        imported_at: String,
    },
}

/// Imports a previously locally validated package. Every adapter call after
/// `begin_upgrade` uses the same transaction object. Any error drops that
/// object without calling `commit`, which the adapter contract defines as a
/// complete rollback of staging and live-table changes.
pub async fn import_to_postgres<A: PostgresUpgradeAdapter>(
    adapter: &A,
    package: &ValidatedPackage,
    target: NetworkUpgradeTarget,
) -> Result<ImportOutcome, PostgresImportError<A::Error>> {
    let claim = MigrationClaim {
        migration_id: package.manifest.migration_id.clone(),
        export_id: package.manifest.export_id.clone(),
        source_instance_id: package.manifest.source.instance_id.clone(),
        source_workspace_id: package.manifest.source.workspace_id.clone(),
        package_checksum: package.package_checksum.clone(),
        logical_schema_version: package.manifest.logical_schema_version,
        target: target.clone(),
    };
    let mut transaction = adapter
        .begin_upgrade()
        .await
        .map_err(PostgresImportError::Adapter)?;
    match transaction
        .claim_migration(claim.clone())
        .await
        .map_err(PostgresImportError::Adapter)?
    {
        MigrationClaimState::AlreadyImported {
            package_checksum,
            imported_at,
        } => {
            if package_checksum != package.package_checksum {
                return Err(PostgresImportError::IdempotencyConflict {
                    migration_id: package.manifest.migration_id.clone(),
                    expected_checksum: package.package_checksum.clone(),
                    imported_checksum: package_checksum,
                });
            }
            transaction
                .commit()
                .await
                .map_err(PostgresImportError::Adapter)?;
            return Ok(ImportOutcome::AlreadyImported {
                migration_id: package.manifest.migration_id.clone(),
                imported_at,
            });
        }
        MigrationClaimState::New => {}
    }

    for digest in &package.manifest.files {
        let path = package
            .data_file_path(&digest.path)
            .map_err(|error| PostgresImportError::StagingRejected(error.to_string()))?;
        transaction
            .stage_file(StagedDataFile {
                path,
                digest: digest.clone(),
            })
            .await
            .map_err(PostgresImportError::Adapter)?;
    }
    let report = transaction
        .validate_staging(package.manifest.clone(), target.clone())
        .await
        .map_err(PostgresImportError::Adapter)?;
    let expected_rows: BTreeMap<String, u64> = package
        .manifest
        .files
        .iter()
        .map(|file| (file.path.clone(), file.rows))
        .collect();
    if report.file_rows != expected_rows
        || !report.target_workspace_is_empty
        || !report.uniqueness_validated
        || !report.references_validated
        || !report.quantities_validated
    {
        return Err(PostgresImportError::StagingRejected(format!(
            "expected rows {expected_rows:?}, got {:?}; empty={}, unique={}, references={}, quantities={}",
            report.file_rows,
            report.target_workspace_is_empty,
            report.uniqueness_validated,
            report.references_validated,
            report.quantities_validated
        )));
    }

    transaction
        .apply_staged_package(package.manifest.clone(), target)
        .await
        .map_err(PostgresImportError::Adapter)?;
    let imported_at = transaction
        .record_imported(claim)
        .await
        .map_err(PostgresImportError::Adapter)?;
    transaction
        .commit()
        .await
        .map_err(PostgresImportError::Adapter)?;
    Ok(ImportOutcome::Imported {
        migration_id: package.manifest.migration_id.clone(),
        imported_at,
    })
}

/// Errors produced by the concrete PostgreSQL upgrade adapter.  Package
/// syntax and the source-side relational checks are deliberately represented
/// by [`UpgradeError`]; this type is limited to failures that happen while a
/// network transaction is open.
#[derive(Debug, thiserror::Error)]
pub enum PgUpgradeError {
    #[error("PostgreSQL operation failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("network authorization failed: {0}")]
    Auth(#[from] AuthError),
    #[error("invalid PostgreSQL upgrade data: {0}")]
    Data(String),
    #[error("upgrade target is not empty: {0}")]
    TargetOccupied(String),
    #[error(
        "migration idempotency conflict for migration {migration_id} or export {export_id}: {detail}"
    )]
    IdempotencyConflict {
        migration_id: String,
        export_id: String,
        detail: String,
    },
}

/// A PostgreSQL adapter that authenticates the caller before opening the
/// upgrade transaction.  It is intentionally constructed with a network
/// session token rather than an actor id supplied by the request body.
/// `import_to_postgres` still receives a target object for the generic trait,
/// but the concrete transaction rejects a tenant or actor that differs from
/// this verified principal.
pub struct PgUpgradeAdapter {
    pool: PgPool,
    tenant_id: Uuid,
    session_token: String,
    required_permission: String,
}

impl PgUpgradeAdapter {
    pub fn new(
        database: &NetworkDatabase,
        tenant_id: Uuid,
        session_token: impl Into<String>,
        required_permission: impl Into<String>,
    ) -> Self {
        Self {
            pool: database.pool().clone(),
            tenant_id,
            session_token: session_token.into(),
            required_permission: required_permission.into(),
        }
    }

    pub fn from_pool(
        pool: &PgPool,
        tenant_id: Uuid,
        session_token: impl Into<String>,
        required_permission: impl Into<String>,
    ) -> Self {
        Self {
            pool: pool.clone(),
            tenant_id,
            session_token: session_token.into(),
            required_permission: required_permission.into(),
        }
    }
}

pub struct PgUpgradeTransaction<'a> {
    transaction: Transaction<'a, Postgres>,
    tenant_id: Uuid,
    actor_id: Uuid,
}

impl PostgresUpgradeAdapter for PgUpgradeAdapter {
    type Error = PgUpgradeError;
    type Transaction<'tx>
        = PgUpgradeTransaction<'tx>
    where
        Self: 'tx;

    fn begin_upgrade(
        &self,
    ) -> impl Future<Output = Result<Self::Transaction<'_>, Self::Error>> + Send {
        async move {
            let mut transaction = self.pool.begin().await?;
            let session = authorize_session_in_transaction(
                &mut transaction,
                self.tenant_id,
                &self.session_token,
                &self.required_permission,
            )
            .await?;
            sqlx::query(
                r#"
                CREATE TEMP TABLE invpack_upgrade_stage (
                    file_path text NOT NULL,
                    row_number bigint NOT NULL CHECK (row_number > 0),
                    table_name text NOT NULL,
                    record jsonb NOT NULL,
                    PRIMARY KEY (file_path, row_number)
                ) ON COMMIT DROP
                "#,
            )
            .execute(&mut *transaction)
            .await?;
            Ok(PgUpgradeTransaction {
                transaction,
                tenant_id: self.tenant_id,
                actor_id: session.identity.user_id,
            })
        }
    }
}

impl<'a> PgUpgradeTransaction<'a> {
    async fn claim_migration_impl(
        &mut self,
        claim: MigrationClaim,
    ) -> Result<MigrationClaimState, PgUpgradeError> {
        let tenant_id = parse_target_uuid("tenant_id", &claim.target.tenant_id)?;
        let workspace_id = parse_target_uuid("workspace_id", &claim.target.workspace_id)?;
        let actor_id = parse_target_uuid("actor_id", &claim.target.actor_id)?;
        if tenant_id != self.tenant_id {
            return Err(PgUpgradeError::Data(
                "upgrade target tenant does not match the authenticated tenant".to_owned(),
            ));
        }
        if actor_id != self.actor_id {
            return Err(PgUpgradeError::Data(
                "upgrade target actor does not match the authenticated session".to_owned(),
            ));
        }
        if claim.migration_id.trim().is_empty() || claim.export_id.trim().is_empty() {
            return Err(PgUpgradeError::Data(
                "migration_id and export_id must not be empty".to_owned(),
            ));
        }
        if claim.source_instance_id.parse::<Uuid>().is_err()
            || claim.source_workspace_id.parse::<Uuid>().is_err()
        {
            return Err(PgUpgradeError::Data(
                "source identity must contain UUIDs".to_owned(),
            ));
        }

        // A transaction advisory lock closes the race where two requests both
        // observe no migration_packages row and then try to import the same
        // package.  Lock keys are acquired in stable order to avoid deadlocks
        // when a request reuses either identifier.
        let mut lock_keys = [
            format!(
                "inventory-upgrade:migration:{tenant_id}:{}",
                claim.migration_id
            ),
            format!("inventory-upgrade:export:{tenant_id}:{}", claim.export_id),
        ];
        lock_keys.sort_unstable();
        for key in lock_keys {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
                .bind(key)
                .execute(&mut *self.transaction)
                .await?;
        }

        let rows = sqlx::query(
            r#"
            SELECT migration_id, export_id, checksum, status,
                   imported_at::text AS imported_at
              FROM migration_packages
             WHERE tenant_id = $1
               AND (migration_id = $2 OR export_id = $3)
             FOR UPDATE
            "#,
        )
        .bind(tenant_id)
        .bind(&claim.migration_id)
        .bind(&claim.export_id)
        .fetch_all(&mut *self.transaction)
        .await?;
        if let Some(row) = rows.into_iter().next() {
            let migration_id: Option<String> = row.try_get("migration_id")?;
            let export_id: String = row.try_get("export_id")?;
            let checksum: String = row.try_get("checksum")?;
            let status: String = row.try_get("status")?;
            let imported_at: Option<String> = row.try_get("imported_at")?;
            let same_identity = migration_id.as_deref() == Some(claim.migration_id.as_str())
                || export_id == claim.export_id;
            if same_identity && status == "imported" && checksum == claim.package_checksum {
                return Ok(MigrationClaimState::AlreadyImported {
                    package_checksum: checksum,
                    imported_at: imported_at.unwrap_or_default(),
                });
            }
            return Err(PgUpgradeError::IdempotencyConflict {
                migration_id: claim.migration_id.clone(),
                export_id: claim.export_id.clone(),
                detail: format!(
                    "existing migration={migration_id:?}, export={export_id}, status={status}, checksum differs or is incomplete"
                ),
            });
        }

        // Keep the parsed values alive in this method so a future implementation
        // can attach target metadata to the transaction without accepting an
        // unvalidated string from the network request.
        let _ = workspace_id;
        Ok(MigrationClaimState::New)
    }

    async fn stage_file_impl(&mut self, file: StagedDataFile) -> Result<(), PgUpgradeError> {
        let spec = DATA_FILES
            .iter()
            .find(|spec| spec.path == file.digest.path)
            .ok_or_else(|| {
                PgUpgradeError::Data(format!("unsupported data file {}", file.digest.path))
            })?;
        let actual =
            hash_jsonl_file(&file.path).map_err(|error| PgUpgradeError::Data(error.to_string()))?;
        if actual.rows != file.digest.rows || actual.sha256 != file.digest.sha256 {
            return Err(PgUpgradeError::Data(format!(
                "{} digest changed while staging",
                file.digest.path
            )));
        }
        let reader = BufReader::new(
            File::open(&file.path).map_err(|error| PgUpgradeError::Data(error.to_string()))?,
        );
        let allowed: HashSet<&str> = spec.tables.iter().map(|table| table.name).collect();
        for (index, line) in reader.lines().enumerate() {
            let line = line.map_err(|error| PgUpgradeError::Data(error.to_string()))?;
            let row: PackageRow = serde_json::from_str(&line)
                .map_err(|error| PgUpgradeError::Data(error.to_string()))?;
            if !allowed.contains(row.table.as_str()) {
                return Err(PgUpgradeError::Data(format!(
                    "{} contains table {} in the wrong file",
                    file.digest.path, row.table
                )));
            }
            let record = SqlxJson(Value::Object(row.record));
            sqlx::query(
                "INSERT INTO invpack_upgrade_stage (file_path, row_number, table_name, record) VALUES ($1, $2, $3, $4)",
            )
            .bind(&file.digest.path)
            .bind(i64::try_from(index + 1).map_err(|_| {
                PgUpgradeError::Data("staging row number exceeds PostgreSQL bigint".to_owned())
            })?)
            .bind(&row.table)
            .bind(record)
            .execute(&mut *self.transaction)
            .await?;
        }
        Ok(())
    }

    async fn validate_staging_impl(
        &mut self,
        manifest: PackageManifest,
        target: NetworkUpgradeTarget,
    ) -> Result<PostgresStagingReport, PgUpgradeError> {
        let tenant_id = parse_target_uuid("tenant_id", &target.tenant_id)?;
        let workspace_id = parse_target_uuid("workspace_id", &target.workspace_id)?;
        let actor_id = parse_target_uuid("actor_id", &target.actor_id)?;
        if tenant_id != self.tenant_id || actor_id != self.actor_id {
            return Err(PgUpgradeError::Data(
                "upgrade target does not match authenticated context".to_owned(),
            ));
        }
        let other_workspace_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM workspaces WHERE tenant_id = $1 AND id <> $2")
                .bind(tenant_id)
                .bind(workspace_id)
                .fetch_one(&mut *self.transaction)
                .await?;
        if other_workspace_count != 0 {
            return Err(PgUpgradeError::TargetOccupied(
                "tenant already owns a different workspace".to_owned(),
            ));
        }

        // A workspace row itself is not business data.  Every other table must
        // be empty before a one-time upgrade can establish the new source of
        // truth.  Counting in this transaction also protects an existing empty
        // workspace from a concurrent write.
        for table in NETWORK_BUSINESS_TABLES {
            let query = format!("SELECT count(*) FROM {table} WHERE tenant_id = $1");
            let count: i64 = sqlx::query_scalar(&query)
                .bind(tenant_id)
                .fetch_one(&mut *self.transaction)
                .await?;
            if count != 0 {
                return Err(PgUpgradeError::TargetOccupied(format!(
                    "{table} contains {count} rows"
                )));
            }
        }

        let records = self.load_staged_records().await?;
        validate_relational_data(&manifest, &records)
            .map_err(|error| PgUpgradeError::Data(error.to_string()))?;
        let mut file_rows = self.staged_file_rows().await?;
        // SQL `GROUP BY` omits empty files; retain explicit zeroes so the
        // staging report can be compared with the manifest one-to-one.
        for file in &manifest.files {
            file_rows.entry(file.path.clone()).or_insert(0);
        }
        let expected_rows: BTreeMap<String, u64> = manifest
            .files
            .iter()
            .map(|file| (file.path.clone(), file.rows))
            .collect();
        if file_rows != expected_rows {
            return Err(PgUpgradeError::Data(format!(
                "staged row counts differ: expected {expected_rows:?}, got {file_rows:?}"
            )));
        }
        Ok(PostgresStagingReport {
            file_rows,
            target_workspace_is_empty: true,
            uniqueness_validated: true,
            references_validated: true,
            quantities_validated: true,
        })
    }

    async fn apply_staged_package_impl(
        &mut self,
        _manifest: PackageManifest,
        target: NetworkUpgradeTarget,
    ) -> Result<(), PgUpgradeError> {
        let tenant_id = parse_target_uuid("tenant_id", &target.tenant_id)?;
        let workspace_id = parse_target_uuid("workspace_id", &target.workspace_id)?;
        let actor_id = parse_target_uuid("actor_id", &target.actor_id)?;
        if tenant_id != self.tenant_id || actor_id != self.actor_id {
            return Err(PgUpgradeError::Data(
                "upgrade target does not match authenticated context".to_owned(),
            ));
        }
        let records = self.load_staged_records().await?;
        self.apply_master_data(tenant_id, workspace_id, &records)
            .await?;
        self.apply_inbound(tenant_id, actor_id, &records).await?;
        self.apply_quality(tenant_id, actor_id, &records).await?;
        self.apply_outbound(tenant_id, actor_id, &records).await?;
        self.apply_audit(tenant_id, actor_id, &records).await?;
        self.verify_live_counts(tenant_id, &records).await?;
        Ok(())
    }

    async fn verify_live_counts(
        &mut self,
        tenant_id: Uuid,
        records: &BTreeMap<String, Vec<Map<String, Value>>>,
    ) -> Result<(), PgUpgradeError> {
        for table in all_table_names() {
            let expected = i64::try_from(records_for(records, table).len()).map_err(|_| {
                PgUpgradeError::Data(format!("{table} row count exceeds PostgreSQL bigint"))
            })?;
            let query = format!("SELECT count(*) FROM {table} WHERE tenant_id = $1");
            let actual: i64 = sqlx::query_scalar(&query)
                .bind(tenant_id)
                .fetch_one(&mut *self.transaction)
                .await?;
            if actual != expected {
                return Err(PgUpgradeError::Data(format!(
                    "post-apply count mismatch for {table}: expected {expected}, got {actual}"
                )));
            }
        }
        Ok(())
    }

    async fn record_imported_impl(
        &mut self,
        claim: MigrationClaim,
    ) -> Result<String, PgUpgradeError> {
        let tenant_id = parse_target_uuid("tenant_id", &claim.target.tenant_id)?;
        let workspace_id = parse_target_uuid("workspace_id", &claim.target.workspace_id)?;
        let actor_id = parse_target_uuid("actor_id", &claim.target.actor_id)?;
        let source_instance_id =
            parse_target_uuid("source_instance_id", &claim.source_instance_id)?;
        let source_workspace_id =
            parse_target_uuid("source_workspace_id", &claim.source_workspace_id)?;
        if tenant_id != self.tenant_id || actor_id != self.actor_id {
            return Err(PgUpgradeError::Data(
                "migration record target does not match authenticated context".to_owned(),
            ));
        }
        let imported_at: String = sqlx::query_scalar(
            r#"
            INSERT INTO migration_packages
                (tenant_id, id, workspace_id, export_id, direction,
                 schema_version, checksum, status, created_at, imported_at,
                 migration_id, source_instance_id, source_workspace_id, actor_id)
            VALUES ($1, $2, $3, $4, 'offline_to_network', $5, $6, 'imported',
                    CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, $7, $8, $9, $10)
            RETURNING imported_at::text
            "#,
        )
        .bind(tenant_id)
        .bind(Uuid::now_v7())
        .bind(workspace_id)
        .bind(&claim.export_id)
        .bind(i32::try_from(claim.logical_schema_version).map_err(|_| {
            PgUpgradeError::Data(
                "logical schema version does not fit PostgreSQL integer".to_owned(),
            )
        })?)
        .bind(&claim.package_checksum)
        .bind(&claim.migration_id)
        .bind(source_instance_id)
        .bind(source_workspace_id)
        .bind(actor_id)
        .fetch_one(&mut *self.transaction)
        .await?;
        Ok(imported_at)
    }

    async fn staged_file_rows(&mut self) -> Result<BTreeMap<String, u64>, PgUpgradeError> {
        let rows = sqlx::query(
            "SELECT file_path, count(*)::bigint AS rows FROM invpack_upgrade_stage GROUP BY file_path",
        )
        .fetch_all(&mut *self.transaction)
        .await?;
        let mut result = BTreeMap::new();
        for row in rows {
            let path: String = row.try_get("file_path")?;
            let count: i64 = row.try_get("rows")?;
            result.insert(
                path,
                u64::try_from(count)
                    .map_err(|_| PgUpgradeError::Data("negative staging row count".to_owned()))?,
            );
        }
        Ok(result)
    }

    async fn load_staged_records(
        &mut self,
    ) -> Result<BTreeMap<String, Vec<Map<String, Value>>>, PgUpgradeError> {
        let rows: Vec<PgRow> = sqlx::query(
            "SELECT table_name, record FROM invpack_upgrade_stage ORDER BY file_path, row_number",
        )
        .fetch_all(&mut *self.transaction)
        .await?;
        let mut records: BTreeMap<String, Vec<Map<String, Value>>> = BTreeMap::new();
        for row in rows {
            let table: String = row.try_get("table_name")?;
            let SqlxJson(value): SqlxJson<Value> = row.try_get("record")?;
            let Value::Object(record) = value else {
                return Err(PgUpgradeError::Data(format!(
                    "staged {table} record is not a JSON object"
                )));
            };
            records.entry(table).or_default().push(record);
        }
        Ok(records)
    }

    async fn apply_master_data(
        &mut self,
        tenant_id: Uuid,
        workspace_id: Uuid,
        records: &BTreeMap<String, Vec<Map<String, Value>>>,
    ) -> Result<(), PgUpgradeError> {
        let workspace = one_record(records, "workspaces")?;
        sqlx::query(
            r#"
            INSERT INTO workspaces
                (tenant_id, id, name, timezone, source_instance_id, created_at)
            VALUES ($1, $2, $3, $4, $5, $6::timestamptz)
            ON CONFLICT (tenant_id, id) DO UPDATE SET
                name = EXCLUDED.name,
                timezone = EXCLUDED.timezone,
                source_instance_id = EXCLUDED.source_instance_id
            "#,
        )
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(string_field(workspace, "workspaces", "name")?)
        .bind(string_field(workspace, "workspaces", "timezone")?)
        .bind(uuid_field(workspace, "workspaces", "source_instance_id")?)
        .bind(string_field(workspace, "workspaces", "created_at")?)
        .execute(&mut *self.transaction)
        .await?;

        for record in records_for(records, "business_parties") {
            sqlx::query(
                r#"
                INSERT INTO business_parties
                    (tenant_id, id, normalized_name, display_name, contact_name,
                     phone, wechat, email, address, notes, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "business_parties", "id")?)
            .bind(string_field(record, "business_parties", "normalized_name")?)
            .bind(string_field(record, "business_parties", "display_name")?)
            .bind(optional_string_field(
                record,
                "business_parties",
                "contact_name",
            )?)
            .bind(optional_string_field(record, "business_parties", "phone")?)
            .bind(optional_string_field(record, "business_parties", "wechat")?)
            .bind(optional_string_field(record, "business_parties", "email")?)
            .bind(optional_string_field(
                record,
                "business_parties",
                "address",
            )?)
            .bind(optional_string_field(record, "business_parties", "notes")?)
            .bind(string_field(record, "business_parties", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "party_roles") {
            sqlx::query(
                "INSERT INTO party_roles (tenant_id, party_id, role, created_at) VALUES ($1, $2, $3, $4::timestamptz)",
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "party_roles", "party_id")?)
            .bind(string_field(record, "party_roles", "role")?)
            .bind(string_field(record, "party_roles", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "skus") {
            sqlx::query(
                r#"INSERT INTO skus
                    (tenant_id, id, code, name, tracking_mode, active,
                     serial_prefix, serial_forbidden_chars, created_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::timestamptz)"#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "skus", "id")?)
            .bind(string_field(record, "skus", "code")?)
            .bind(string_field(record, "skus", "name")?)
            .bind(string_field(record, "skus", "tracking_mode")?)
            .bind(bool_field(record, "skus", "active")?)
            .bind(optional_string_field(record, "skus", "serial_prefix")?)
            .bind(
                optional_string_field(record, "skus", "serial_forbidden_chars")?
                    .unwrap_or_default(),
            )
            .bind(string_field(record, "skus", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "warehouses") {
            sqlx::query(
                r#"INSERT INTO warehouses
                    (tenant_id, id, code, name, created_at)
                   VALUES ($1, $2, $3, $4, $5::timestamptz)"#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "warehouses", "id")?)
            .bind(string_field(record, "warehouses", "code")?)
            .bind(string_field(record, "warehouses", "name")?)
            .bind(string_field(record, "warehouses", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "locations") {
            sqlx::query(
                r#"INSERT INTO locations
                    (tenant_id, id, warehouse_id, code, name, kind, created_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz)"#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "locations", "id")?)
            .bind(uuid_field(record, "locations", "warehouse_id")?)
            .bind(string_field(record, "locations", "code")?)
            .bind(string_field(record, "locations", "name")?)
            .bind(string_field(record, "locations", "kind")?)
            .bind(string_field(record, "locations", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        Ok(())
    }

    async fn apply_inbound(
        &mut self,
        tenant_id: Uuid,
        actor_id: Uuid,
        records: &BTreeMap<String, Vec<Map<String, Value>>>,
    ) -> Result<(), PgUpgradeError> {
        for record in records_for(records, "inbound_receipts") {
            sqlx::query(
                r#"
                INSERT INTO inbound_receipts
                    (tenant_id, id, receipt_no, owner_party_id, warehouse_id,
                     supplier_party_id, source_reference, received_at, status,
                     actor_id, source_actor_id, idempotency_key, request_id,
                     created_at, warranty_duration_days, warranty_label_snapshot,
                     warranty_started_at, warranty_expires_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9, $10,
                        $11, $12, $13, $14::timestamptz, $15, $16,
                        $17::timestamptz, $18::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "inbound_receipts", "id")?)
            .bind(string_field(record, "inbound_receipts", "receipt_no")?)
            .bind(uuid_field(record, "inbound_receipts", "owner_party_id")?)
            .bind(uuid_field(record, "inbound_receipts", "warehouse_id")?)
            .bind(optional_uuid_field(
                record,
                "inbound_receipts",
                "supplier_party_id",
            )?)
            .bind(optional_string_field(
                record,
                "inbound_receipts",
                "source_reference",
            )?)
            .bind(string_field(record, "inbound_receipts", "received_at")?)
            .bind(string_field(record, "inbound_receipts", "status")?)
            .bind(actor_id)
            .bind(string_field(record, "inbound_receipts", "actor_id")?)
            .bind(string_field(record, "inbound_receipts", "idempotency_key")?)
            .bind(string_field(record, "inbound_receipts", "request_id")?)
            .bind(string_field(record, "inbound_receipts", "created_at")?)
            .bind(optional_i32_field(
                record,
                "inbound_receipts",
                "warranty_duration_days",
            )?)
            .bind(optional_string_field(
                record,
                "inbound_receipts",
                "warranty_label_snapshot",
            )?)
            .bind(optional_string_field(
                record,
                "inbound_receipts",
                "warranty_started_at",
            )?)
            .bind(optional_string_field(
                record,
                "inbound_receipts",
                "warranty_expires_at",
            )?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "inbound_receipt_lines") {
            sqlx::query(
                r#"
                INSERT INTO inbound_receipt_lines
                    (tenant_id, id, receipt_id, sku_id, declared_quantity,
                     scanned_quantity, notes, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "inbound_receipt_lines", "id")?)
            .bind(uuid_field(record, "inbound_receipt_lines", "receipt_id")?)
            .bind(uuid_field(record, "inbound_receipt_lines", "sku_id")?)
            .bind(i32_field(
                record,
                "inbound_receipt_lines",
                "declared_quantity",
            )?)
            .bind(i32_field(
                record,
                "inbound_receipt_lines",
                "scanned_quantity",
            )?)
            .bind(optional_string_field(
                record,
                "inbound_receipt_lines",
                "notes",
            )?)
            .bind(string_field(record, "inbound_receipt_lines", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "inventory_units") {
            sqlx::query(
                r#"
                INSERT INTO inventory_units
                    (tenant_id, id, barcode, inbound_receipt_line_id,
                     owner_party_id, sku_id, location_id, inventory_status,
                     quality_status, version, received_at, updated_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::timestamptz,
                        $12::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "inventory_units", "id")?)
            .bind(string_field(record, "inventory_units", "barcode")?)
            .bind(uuid_field(
                record,
                "inventory_units",
                "inbound_receipt_line_id",
            )?)
            .bind(uuid_field(record, "inventory_units", "owner_party_id")?)
            .bind(uuid_field(record, "inventory_units", "sku_id")?)
            .bind(uuid_field(record, "inventory_units", "location_id")?)
            .bind(string_field(record, "inventory_units", "inventory_status")?)
            .bind(string_field(record, "inventory_units", "quality_status")?)
            .bind(i64_field(record, "inventory_units", "version")?)
            .bind(string_field(record, "inventory_units", "received_at")?)
            .bind(string_field(record, "inventory_units", "updated_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        Ok(())
    }

    async fn apply_quality(
        &mut self,
        tenant_id: Uuid,
        actor_id: Uuid,
        records: &BTreeMap<String, Vec<Map<String, Value>>>,
    ) -> Result<(), PgUpgradeError> {
        for record in records_for(records, "quality_labels") {
            sqlx::query(
                r#"
                INSERT INTO quality_labels
                    (tenant_id, id, name, normalized_name, disposition, active,
                     created_at, updated_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz,
                        $8::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "quality_labels", "id")?)
            .bind(string_field(record, "quality_labels", "name")?)
            .bind(string_field(record, "quality_labels", "normalized_name")?)
            .bind(string_field(record, "quality_labels", "disposition")?)
            .bind(bool_field(record, "quality_labels", "active")?)
            .bind(string_field(record, "quality_labels", "created_at")?)
            .bind(string_field(record, "quality_labels", "updated_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "quality_label_name_history") {
            sqlx::query(
                r#"
                INSERT INTO quality_label_name_history
                    (tenant_id, id, quality_label_id, old_name, new_name,
                     changed_by, changed_by_snapshot, source_actor_id,
                     change_note, changed_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                        $10::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "quality_label_name_history", "id")?)
            .bind(uuid_field(
                record,
                "quality_label_name_history",
                "quality_label_id",
            )?)
            .bind(string_field(
                record,
                "quality_label_name_history",
                "old_name",
            )?)
            .bind(string_field(
                record,
                "quality_label_name_history",
                "new_name",
            )?)
            .bind(actor_id)
            .bind(string_field(
                record,
                "quality_label_name_history",
                "changed_by_snapshot",
            )?)
            .bind(string_field(
                record,
                "quality_label_name_history",
                "changed_by",
            )?)
            .bind(optional_string_field(
                record,
                "quality_label_name_history",
                "change_note",
            )?)
            .bind(string_field(
                record,
                "quality_label_name_history",
                "changed_at",
            )?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "quality_inspections") {
            sqlx::query(
                r#"
                INSERT INTO quality_inspections
                    (tenant_id, id, inspection_no, inspection_type, status,
                     inspector_id, source_actor_id, inspected_at,
                     idempotency_key, request_id, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9,
                        $10, $11::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "quality_inspections", "id")?)
            .bind(string_field(
                record,
                "quality_inspections",
                "inspection_no",
            )?)
            .bind(string_field(
                record,
                "quality_inspections",
                "inspection_type",
            )?)
            .bind(string_field(record, "quality_inspections", "status")?)
            .bind(actor_id)
            .bind(string_field(record, "quality_inspections", "inspector_id")?)
            .bind(string_field(record, "quality_inspections", "inspected_at")?)
            .bind(string_field(
                record,
                "quality_inspections",
                "idempotency_key",
            )?)
            .bind(string_field(record, "quality_inspections", "request_id")?)
            .bind(string_field(record, "quality_inspections", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "quality_inspection_results") {
            sqlx::query(
                r#"
                INSERT INTO quality_inspection_results
                    (tenant_id, id, inspection_id, inventory_unit_id, result,
                     quality_label_id, quality_label_snapshot, defect_code,
                     measurements_json, notes, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                        $11::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "quality_inspection_results", "id")?)
            .bind(uuid_field(
                record,
                "quality_inspection_results",
                "inspection_id",
            )?)
            .bind(uuid_field(
                record,
                "quality_inspection_results",
                "inventory_unit_id",
            )?)
            .bind(string_field(
                record,
                "quality_inspection_results",
                "result",
            )?)
            .bind(optional_uuid_field(
                record,
                "quality_inspection_results",
                "quality_label_id",
            )?)
            .bind(optional_string_field(
                record,
                "quality_inspection_results",
                "quality_label_snapshot",
            )?)
            .bind(optional_string_field(
                record,
                "quality_inspection_results",
                "defect_code",
            )?)
            .bind(SqlxJson(json_field(
                record,
                "quality_inspection_results",
                "measurements_json",
            )?))
            .bind(optional_string_field(
                record,
                "quality_inspection_results",
                "notes",
            )?)
            .bind(string_field(
                record,
                "quality_inspection_results",
                "created_at",
            )?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "quality_waivers") {
            sqlx::query(
                r#"
                INSERT INTO quality_waivers
                    (tenant_id, id, inventory_unit_id, reason, authorized_by,
                     source_actor_id, authorized_at, revoked_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz,
                        $8::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "quality_waivers", "id")?)
            .bind(uuid_field(record, "quality_waivers", "inventory_unit_id")?)
            .bind(string_field(record, "quality_waivers", "reason")?)
            .bind(actor_id)
            .bind(string_field(record, "quality_waivers", "authorized_by")?)
            .bind(string_field(record, "quality_waivers", "authorized_at")?)
            .bind(optional_string_field(
                record,
                "quality_waivers",
                "revoked_at",
            )?)
            .execute(&mut *self.transaction)
            .await?;
        }
        Ok(())
    }

    async fn apply_outbound(
        &mut self,
        tenant_id: Uuid,
        actor_id: Uuid,
        records: &BTreeMap<String, Vec<Map<String, Value>>>,
    ) -> Result<(), PgUpgradeError> {
        for record in records_for(records, "outbound_orders") {
            sqlx::query(
                r#"
                INSERT INTO outbound_orders
                    (tenant_id, id, order_no, upstream_receiver_id, required_at,
                     status, actor_id, source_actor_id, idempotency_key,
                     request_id, created_at)
                VALUES ($1, $2, $3, $4, $5::timestamptz, $6, $7, $8, $9,
                        $10, $11::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "outbound_orders", "id")?)
            .bind(string_field(record, "outbound_orders", "order_no")?)
            .bind(uuid_field(
                record,
                "outbound_orders",
                "upstream_receiver_id",
            )?)
            .bind(optional_string_field(
                record,
                "outbound_orders",
                "required_at",
            )?)
            .bind(string_field(record, "outbound_orders", "status")?)
            .bind(actor_id)
            .bind(string_field(record, "outbound_orders", "actor_id")?)
            .bind(string_field(record, "outbound_orders", "idempotency_key")?)
            .bind(string_field(record, "outbound_orders", "request_id")?)
            .bind(string_field(record, "outbound_orders", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "outbound_order_lines") {
            sqlx::query(
                r#"
                INSERT INTO outbound_order_lines
                    (tenant_id, id, outbound_order_id, sku_id, required_quantity,
                     allocated_quantity, shipped_quantity, delivered_quantity,
                     created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "outbound_order_lines", "id")?)
            .bind(uuid_field(
                record,
                "outbound_order_lines",
                "outbound_order_id",
            )?)
            .bind(uuid_field(record, "outbound_order_lines", "sku_id")?)
            .bind(i32_field(
                record,
                "outbound_order_lines",
                "required_quantity",
            )?)
            .bind(i32_field(
                record,
                "outbound_order_lines",
                "allocated_quantity",
            )?)
            .bind(i32_field(
                record,
                "outbound_order_lines",
                "shipped_quantity",
            )?)
            .bind(i32_field(
                record,
                "outbound_order_lines",
                "delivered_quantity",
            )?)
            .bind(string_field(record, "outbound_order_lines", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }

        let line_skus: HashMap<String, Uuid> = records_for(records, "outbound_order_lines")
            .iter()
            .map(|record| {
                Ok((
                    string_field(record, "outbound_order_lines", "id")?.to_owned(),
                    uuid_field(record, "outbound_order_lines", "sku_id")?,
                ))
            })
            .collect::<Result<_, PgUpgradeError>>()?;
        let unit_skus: HashMap<String, Uuid> = records_for(records, "inventory_units")
            .iter()
            .map(|record| {
                Ok((
                    string_field(record, "inventory_units", "id")?.to_owned(),
                    uuid_field(record, "inventory_units", "sku_id")?,
                ))
            })
            .collect::<Result<_, PgUpgradeError>>()?;
        for record in records_for(records, "outbound_allocations") {
            let line_id = string_field(record, "outbound_allocations", "outbound_order_line_id")?;
            let unit_id = string_field(record, "outbound_allocations", "inventory_unit_id")?;
            let line_sku = line_skus.get(line_id).ok_or_else(|| {
                PgUpgradeError::Data(format!(
                    "allocation references missing order line {line_id}"
                ))
            })?;
            let unit_sku = unit_skus.get(unit_id).ok_or_else(|| {
                PgUpgradeError::Data(format!("allocation references missing unit {unit_id}"))
            })?;
            if line_sku != unit_sku {
                return Err(PgUpgradeError::Data(format!(
                    "allocation {line_id}/{unit_id} has inconsistent SKU"
                )));
            }
            sqlx::query(
                r#"
                INSERT INTO outbound_allocations
                    (tenant_id, id, outbound_order_line_id, inventory_unit_id,
                     sku_id, status, allocated_by, source_actor_id,
                     allocated_at, released_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::timestamptz,
                        $10::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "outbound_allocations", "id")?)
            .bind(uuid_field(
                record,
                "outbound_allocations",
                "outbound_order_line_id",
            )?)
            .bind(uuid_field(
                record,
                "outbound_allocations",
                "inventory_unit_id",
            )?)
            .bind(*line_sku)
            .bind(string_field(record, "outbound_allocations", "status")?)
            .bind(actor_id)
            .bind(string_field(
                record,
                "outbound_allocations",
                "allocated_by",
            )?)
            .bind(string_field(
                record,
                "outbound_allocations",
                "allocated_at",
            )?)
            .bind(optional_string_field(
                record,
                "outbound_allocations",
                "released_at",
            )?)
            .execute(&mut *self.transaction)
            .await?;
        }

        for record in records_for(records, "outbound_shipments") {
            sqlx::query(
                r#"
                INSERT INTO outbound_shipments
                    (tenant_id, id, shipment_no, outbound_order_id, status,
                     shipped_at, actor_id, source_actor_id, idempotency_key,
                     request_id, created_at, warranty_duration_days,
                     warranty_label_snapshot, warranty_started_at, warranty_expires_at)
                VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7, $8, $9,
                        $10, $11::timestamptz, $12, $13, $14::timestamptz,
                        $15::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "outbound_shipments", "id")?)
            .bind(string_field(record, "outbound_shipments", "shipment_no")?)
            .bind(uuid_field(
                record,
                "outbound_shipments",
                "outbound_order_id",
            )?)
            .bind(string_field(record, "outbound_shipments", "status")?)
            .bind(string_field(record, "outbound_shipments", "shipped_at")?)
            .bind(actor_id)
            .bind(string_field(record, "outbound_shipments", "actor_id")?)
            .bind(string_field(
                record,
                "outbound_shipments",
                "idempotency_key",
            )?)
            .bind(string_field(record, "outbound_shipments", "request_id")?)
            .bind(string_field(record, "outbound_shipments", "created_at")?)
            .bind(optional_i32_field(
                record,
                "outbound_shipments",
                "warranty_duration_days",
            )?)
            .bind(optional_string_field(
                record,
                "outbound_shipments",
                "warranty_label_snapshot",
            )?)
            .bind(optional_string_field(
                record,
                "outbound_shipments",
                "warranty_started_at",
            )?)
            .bind(optional_string_field(
                record,
                "outbound_shipments",
                "warranty_expires_at",
            )?)
            .execute(&mut *self.transaction)
            .await?;
        }

        for record in records_for(records, "delivery_confirmations") {
            sqlx::query(
                r#"
                INSERT INTO delivery_confirmations
                    (tenant_id, id, outbound_shipment_id, confirmation_code,
                     confirmed_by, source_actor_id, confirmed_at, notes,
                     idempotency_key, request_id, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz, $8, $9,
                        $10, $11::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "delivery_confirmations", "id")?)
            .bind(uuid_field(
                record,
                "delivery_confirmations",
                "outbound_shipment_id",
            )?)
            .bind(string_field(
                record,
                "delivery_confirmations",
                "confirmation_code",
            )?)
            .bind(actor_id)
            .bind(string_field(
                record,
                "delivery_confirmations",
                "confirmed_by",
            )?)
            .bind(string_field(
                record,
                "delivery_confirmations",
                "confirmed_at",
            )?)
            .bind(optional_string_field(
                record,
                "delivery_confirmations",
                "notes",
            )?)
            .bind(string_field(
                record,
                "delivery_confirmations",
                "idempotency_key",
            )?)
            .bind(string_field(
                record,
                "delivery_confirmations",
                "request_id",
            )?)
            .bind(string_field(
                record,
                "delivery_confirmations",
                "created_at",
            )?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "outbound_return_batches") {
            sqlx::query(
                r#"
                INSERT INTO outbound_return_batches
                    (tenant_id, id, return_no, returned_at, actor_id,
                     source_actor_id, idempotency_key, request_id, created_at)
                VALUES ($1, $2, $3, $4::timestamptz, $5, $6, $7, $8,
                        $9::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "outbound_return_batches", "id")?)
            .bind(string_field(
                record,
                "outbound_return_batches",
                "return_no",
            )?)
            .bind(string_field(
                record,
                "outbound_return_batches",
                "returned_at",
            )?)
            .bind(actor_id)
            .bind(string_field(record, "outbound_return_batches", "actor_id")?)
            .bind(string_field(
                record,
                "outbound_return_batches",
                "idempotency_key",
            )?)
            .bind(string_field(
                record,
                "outbound_return_batches",
                "request_id",
            )?)
            .bind(string_field(
                record,
                "outbound_return_batches",
                "created_at",
            )?)
            .execute(&mut *self.transaction)
            .await?;
        }

        let delivery_lines_by_shipment: HashMap<&str, &Map<String, Value>> =
            records_for(records, "delivery_confirmation_lines")
                .iter()
                .map(|record| {
                    Ok((
                        string_field(
                            record,
                            "delivery_confirmation_lines",
                            "outbound_shipment_line_id",
                        )?,
                        record,
                    ))
                })
                .collect::<Result<_, PgUpgradeError>>()?;
        let return_lines_by_shipment: HashMap<&str, &Map<String, Value>> =
            records_for(records, "outbound_return_lines")
                .iter()
                .map(|record| {
                    Ok((
                        string_field(record, "outbound_return_lines", "outbound_shipment_line_id")?,
                        record,
                    ))
                })
                .collect::<Result<_, PgUpgradeError>>()?;
        let mut shipment_lines = records_for(records, "outbound_shipment_lines")
            .iter()
            .map(|record| {
                Ok((
                    string_field(record, "outbound_shipment_lines", "created_at")?.to_owned(),
                    string_field(record, "outbound_shipment_lines", "id")?.to_owned(),
                    record,
                ))
            })
            .collect::<Result<Vec<_>, PgUpgradeError>>()?;
        shipment_lines
            .sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

        // Replaying each unit's history in time order is required for a unit
        // that was returned, retested and shipped again. PostgreSQL permits
        // only one currently shipped/delivered line per unit, so the earlier
        // line must reach `returned` before the later line is inserted.
        for (_, shipment_line_id, record) in shipment_lines {
            sqlx::query(
                r#"
                INSERT INTO outbound_shipment_lines
                    (tenant_id, id, outbound_shipment_id, outbound_allocation_id,
                     inventory_unit_id, scanned_barcode_snapshot, created_at, status)
                VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz, 'shipped')
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "outbound_shipment_lines", "id")?)
            .bind(uuid_field(
                record,
                "outbound_shipment_lines",
                "outbound_shipment_id",
            )?)
            .bind(uuid_field(
                record,
                "outbound_shipment_lines",
                "outbound_allocation_id",
            )?)
            .bind(uuid_field(
                record,
                "outbound_shipment_lines",
                "inventory_unit_id",
            )?)
            .bind(string_field(
                record,
                "outbound_shipment_lines",
                "scanned_barcode_snapshot",
            )?)
            .bind(string_field(
                record,
                "outbound_shipment_lines",
                "created_at",
            )?)
            .execute(&mut *self.transaction)
            .await?;

            if let Some(delivery) = delivery_lines_by_shipment.get(shipment_line_id.as_str()) {
                sqlx::query(
                    r#"
                    INSERT INTO delivery_confirmation_lines
                        (tenant_id, id, delivery_confirmation_id,
                         outbound_shipment_line_id, result, exception_notes,
                         created_at)
                    VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz)
                    "#,
                )
                .bind(tenant_id)
                .bind(uuid_field(delivery, "delivery_confirmation_lines", "id")?)
                .bind(uuid_field(
                    delivery,
                    "delivery_confirmation_lines",
                    "delivery_confirmation_id",
                )?)
                .bind(uuid_field(
                    delivery,
                    "delivery_confirmation_lines",
                    "outbound_shipment_line_id",
                )?)
                .bind(string_field(
                    delivery,
                    "delivery_confirmation_lines",
                    "result",
                )?)
                .bind(optional_string_field(
                    delivery,
                    "delivery_confirmation_lines",
                    "exception_notes",
                )?)
                .bind(string_field(
                    delivery,
                    "delivery_confirmation_lines",
                    "created_at",
                )?)
                .execute(&mut *self.transaction)
                .await?;
            }

            if let Some(returned) = return_lines_by_shipment.get(shipment_line_id.as_str()) {
                sqlx::query(
                    r#"
                    INSERT INTO outbound_return_lines
                        (tenant_id, id, return_batch_id,
                         outbound_shipment_line_id, inventory_unit_id, reason,
                         disposition, created_at)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz)
                    "#,
                )
                .bind(tenant_id)
                .bind(uuid_field(returned, "outbound_return_lines", "id")?)
                .bind(uuid_field(
                    returned,
                    "outbound_return_lines",
                    "return_batch_id",
                )?)
                .bind(uuid_field(
                    returned,
                    "outbound_return_lines",
                    "outbound_shipment_line_id",
                )?)
                .bind(uuid_field(
                    returned,
                    "outbound_return_lines",
                    "inventory_unit_id",
                )?)
                .bind(string_field(returned, "outbound_return_lines", "reason")?)
                .bind(string_field(
                    returned,
                    "outbound_return_lines",
                    "disposition",
                )?)
                .bind(string_field(
                    returned,
                    "outbound_return_lines",
                    "created_at",
                )?)
                .execute(&mut *self.transaction)
                .await?;
            }
        }
        for record in records_for(records, "stock_movements") {
            sqlx::query(
                r#"
                INSERT INTO stock_movements
                    (tenant_id, id, inventory_unit_id, movement_type,
                     from_location_id, to_location_id, source_type, source_id,
                     actor_id, source_actor_id, occurred_at, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                        $11::timestamptz, $12::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "stock_movements", "id")?)
            .bind(uuid_field(record, "stock_movements", "inventory_unit_id")?)
            .bind(string_field(record, "stock_movements", "movement_type")?)
            .bind(optional_uuid_field(
                record,
                "stock_movements",
                "from_location_id",
            )?)
            .bind(optional_uuid_field(
                record,
                "stock_movements",
                "to_location_id",
            )?)
            .bind(string_field(record, "stock_movements", "source_type")?)
            .bind(uuid_field(record, "stock_movements", "source_id")?)
            .bind(actor_id)
            .bind(string_field(record, "stock_movements", "actor_id")?)
            .bind(string_field(record, "stock_movements", "occurred_at")?)
            .bind(string_field(record, "stock_movements", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        Ok(())
    }

    async fn apply_audit(
        &mut self,
        tenant_id: Uuid,
        actor_id: Uuid,
        records: &BTreeMap<String, Vec<Map<String, Value>>>,
    ) -> Result<(), PgUpgradeError> {
        for record in records_for(records, "document_voids") {
            sqlx::query(
                r#"
                INSERT INTO document_voids
                    (tenant_id, id, document_kind, inbound_receipt_id,
                     outbound_order_id, reason, actor_id, source_actor_id,
                     voided_at, request_id, idempotency_key, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                        $9::timestamptz, $10, $11, $12::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "document_voids", "id")?)
            .bind(string_field(record, "document_voids", "document_kind")?)
            .bind(optional_uuid_field(
                record,
                "document_voids",
                "inbound_receipt_id",
            )?)
            .bind(optional_uuid_field(
                record,
                "document_voids",
                "outbound_order_id",
            )?)
            .bind(string_field(record, "document_voids", "reason")?)
            .bind(actor_id)
            .bind(string_field(record, "document_voids", "actor_id")?)
            .bind(string_field(record, "document_voids", "voided_at")?)
            .bind(string_field(record, "document_voids", "request_id")?)
            .bind(string_field(record, "document_voids", "idempotency_key")?)
            .bind(string_field(record, "document_voids", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "audit_logs") {
            sqlx::query(
                r#"
                INSERT INTO audit_logs
                    (tenant_id, id, actor_id, source_actor_id, action,
                     entity_type, entity_id, request_id, result, details_json,
                     occurred_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                        $11::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "audit_logs", "id")?)
            .bind(actor_id)
            .bind(string_field(record, "audit_logs", "actor_id")?)
            .bind(string_field(record, "audit_logs", "action")?)
            .bind(string_field(record, "audit_logs", "entity_type")?)
            .bind(uuid_field(record, "audit_logs", "entity_id")?)
            .bind(string_field(record, "audit_logs", "request_id")?)
            .bind(string_field(record, "audit_logs", "result")?)
            .bind(SqlxJson(json_field(record, "audit_logs", "details_json")?))
            .bind(string_field(record, "audit_logs", "occurred_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "idempotency_records") {
            sqlx::query(
                r#"
                INSERT INTO idempotency_records
                    (tenant_id, id, scope, idempotency_key, request_hash,
                     response_json, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "idempotency_records", "id")?)
            .bind(string_field(record, "idempotency_records", "scope")?)
            .bind(string_field(
                record,
                "idempotency_records",
                "idempotency_key",
            )?)
            .bind(string_field(record, "idempotency_records", "request_hash")?)
            .bind(SqlxJson(json_field(
                record,
                "idempotency_records",
                "response_json",
            )?))
            .bind(string_field(record, "idempotency_records", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        Ok(())
    }
}

impl<'a> PostgresUpgradeTransaction for PgUpgradeTransaction<'a> {
    type Error = PgUpgradeError;

    fn claim_migration(
        &mut self,
        claim: MigrationClaim,
    ) -> impl Future<Output = Result<MigrationClaimState, Self::Error>> + Send {
        async move { self.claim_migration_impl(claim).await }
    }

    fn stage_file(
        &mut self,
        file: StagedDataFile,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async move { self.stage_file_impl(file).await }
    }

    fn validate_staging(
        &mut self,
        manifest: PackageManifest,
        target: NetworkUpgradeTarget,
    ) -> impl Future<Output = Result<PostgresStagingReport, Self::Error>> + Send {
        async move { self.validate_staging_impl(manifest, target).await }
    }

    fn apply_staged_package(
        &mut self,
        manifest: PackageManifest,
        target: NetworkUpgradeTarget,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async move { self.apply_staged_package_impl(manifest, target).await }
    }

    fn record_imported(
        &mut self,
        claim: MigrationClaim,
    ) -> impl Future<Output = Result<String, Self::Error>> + Send {
        async move { self.record_imported_impl(claim).await }
    }

    fn commit(self) -> impl Future<Output = Result<(), Self::Error>> + Send
    where
        Self: Sized,
    {
        async move {
            self.transaction
                .commit()
                .await
                .map_err(PgUpgradeError::from)
        }
    }
}

const NETWORK_BUSINESS_TABLES: &[&str] = &[
    "business_parties",
    "party_roles",
    "skus",
    "warehouses",
    "locations",
    "inbound_receipts",
    "inbound_receipt_lines",
    "inventory_units",
    "quality_labels",
    "quality_label_name_history",
    "quality_inspections",
    "quality_inspection_results",
    "quality_waivers",
    "outbound_orders",
    "outbound_order_lines",
    "outbound_allocations",
    "outbound_shipments",
    "outbound_shipment_lines",
    "delivery_confirmations",
    "delivery_confirmation_lines",
    "outbound_return_batches",
    "outbound_return_lines",
    "stock_movements",
    "document_voids",
    "audit_logs",
    "idempotency_records",
    "migration_packages",
];

fn parse_target_uuid(field: &str, value: &str) -> Result<Uuid, PgUpgradeError> {
    Uuid::parse_str(value)
        .map_err(|_| PgUpgradeError::Data(format!("{field} must be a UUID: {value}")))
}

fn records_for<'a>(
    records: &'a BTreeMap<String, Vec<Map<String, Value>>>,
    table: &str,
) -> &'a [Map<String, Value>] {
    records.get(table).map(Vec::as_slice).unwrap_or(&[])
}

fn one_record<'a>(
    records: &'a BTreeMap<String, Vec<Map<String, Value>>>,
    table: &str,
) -> Result<&'a Map<String, Value>, PgUpgradeError> {
    let values = records_for(records, table);
    if values.len() != 1 {
        return Err(PgUpgradeError::Data(format!(
            "{table} must contain exactly one record, found {}",
            values.len()
        )));
    }
    Ok(&values[0])
}

fn required_record_value<'a>(
    record: &'a Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<&'a Value, PgUpgradeError> {
    record
        .get(field)
        .filter(|value| !value.is_null())
        .ok_or_else(|| PgUpgradeError::Data(format!("{table}.{field} is missing or null")))
}

fn string_field<'a>(
    record: &'a Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<&'a str, PgUpgradeError> {
    required_record_value(record, table, field)?
        .as_str()
        .ok_or_else(|| PgUpgradeError::Data(format!("{table}.{field} must be a string")))
}

fn optional_string_field(
    record: &Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<Option<String>, PgUpgradeError> {
    let Some(value) = record.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(ToOwned::to_owned)
        .map(Some)
        .ok_or_else(|| PgUpgradeError::Data(format!("{table}.{field} must be a string or null")))
}

fn i64_field(record: &Map<String, Value>, table: &str, field: &str) -> Result<i64, PgUpgradeError> {
    required_record_value(record, table, field)?
        .as_i64()
        .ok_or_else(|| PgUpgradeError::Data(format!("{table}.{field} must be an integer")))
}

fn i32_field(record: &Map<String, Value>, table: &str, field: &str) -> Result<i32, PgUpgradeError> {
    i32::try_from(i64_field(record, table, field)?).map_err(|_| {
        PgUpgradeError::Data(format!("{table}.{field} does not fit PostgreSQL integer"))
    })
}

fn optional_i32_field(
    record: &Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<Option<i32>, PgUpgradeError> {
    let Some(value) = record.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let integer = value.as_i64().ok_or_else(|| {
        PgUpgradeError::Data(format!("{table}.{field} must be an integer or null"))
    })?;
    i32::try_from(integer).map(Some).map_err(|_| {
        PgUpgradeError::Data(format!("{table}.{field} does not fit PostgreSQL integer"))
    })
}

fn bool_field(
    record: &Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<bool, PgUpgradeError> {
    let value = required_record_value(record, table, field)?;
    if let Some(value) = value.as_bool() {
        return Ok(value);
    }
    match value.as_i64() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(PgUpgradeError::Data(format!(
            "{table}.{field} must be a boolean or 0/1"
        ))),
    }
}

fn uuid_field(
    record: &Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<Uuid, PgUpgradeError> {
    let value = string_field(record, table, field)?;
    parse_target_uuid(&format!("{table}.{field}"), value)
}

fn optional_uuid_field(
    record: &Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<Option<Uuid>, PgUpgradeError> {
    let Some(value) = record.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let value = value
        .as_str()
        .ok_or_else(|| PgUpgradeError::Data(format!("{table}.{field} must be a UUID or null")))?;
    parse_target_uuid(&format!("{table}.{field}"), value).map(Some)
}

fn json_field(
    record: &Map<String, Value>,
    table: &str,
    field: &str,
) -> Result<Value, PgUpgradeError> {
    let value = required_record_value(record, table, field)?;
    match value {
        Value::String(encoded) => serde_json::from_str(encoded).map_err(|error| {
            PgUpgradeError::Data(format!("{table}.{field} contains invalid JSON: {error}"))
        }),
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::application::{
        CatalogPartyRole, CompleteInspectionRequest, CreateCatalogPartyRequest,
        CreateCatalogProductRequest, InspectionResultInput, PostReceiptRequest,
    };
    use crate::v2::auth::{hash_token, TokenKind};
    use crate::v2::domain::{InspectionKind, QualityOutcome};
    use crate::v2::network::{NetworkService, PERMISSION_NETWORK_ACCESS};
    use crate::v2::outbound::{
        AllocateOutboundRequest, ConfirmOutboundDeliveryRequest, CreateOutboundOrderRequest,
        ReturnOutboundShipmentRequest, ShipOutboundRequest,
    };
    use crate::v2::postgres::NetworkDatabaseConfig;
    use crate::v2::voiding::VoidDocumentRequest;
    use crate::v2::OfflineDatabase;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::Executor;
    use std::sync::{Arc, Mutex};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("inventory-upgrade-test-{}", Uuid::now_v7()));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn test_pool() -> (SqlitePool, String, String) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open SQLite");
        pool.execute(include_str!(
            "../../migrations/sqlite/0001_inventory_v2_core.sql"
        ))
        .await
        .expect("create V2 schema");
        pool.execute(include_str!(
            "../../migrations/sqlite/0002_upgrade_result_reports.sql"
        ))
        .await
        .expect("create upgrade reports schema");
        pool.execute(include_str!(
            "../../migrations/sqlite/0003_repeat_outbound_after_return.sql"
        ))
        .await
        .expect("migrate repeat outbound schema");
        pool.execute(include_str!(
            "../../migrations/sqlite/0004_legacy_excel_import.sql"
        ))
        .await
        .expect("create legacy import schema");
        pool.execute(include_str!(
            "../../migrations/sqlite/0005_inbound_supplier_and_sku_scan_rules.sql"
        ))
        .await
        .expect("create supplier and scanner schema");
        pool.execute(include_str!(
            "../../migrations/sqlite/0006_business_party_contact_details.sql"
        ))
        .await
        .expect("create business party contact schema");
        pool.execute(include_str!(
            "../../migrations/sqlite/0007_quality_labels.sql"
        ))
        .await
        .expect("create quality label schema");
        pool.execute(include_str!(
            "../../migrations/sqlite/0008_quality_label_name_history.sql"
        ))
        .await
        .expect("create quality label history schema");
        pool.execute(include_str!(
            "../../migrations/sqlite/0009_document_warranties.sql"
        ))
        .await
        .expect("create warranty schema");
        pool.execute(include_str!(
            "../../migrations/sqlite/0010_document_voids.sql"
        ))
        .await
        .expect("create document void schema");
        pool.execute(
            "CREATE TABLE _sqlx_migrations (version BIGINT NOT NULL, success BOOLEAN NOT NULL)",
        )
        .await
        .expect("create migration metadata");
        for version in 1..=REQUIRED_SQLITE_MIGRATION_VERSION {
            sqlx::query("INSERT INTO _sqlx_migrations (version, success) VALUES (?1, 1)")
                .bind(version)
                .execute(&pool)
                .await
                .expect("record migration");
        }

        let workspace_id = Uuid::now_v7().to_string();
        let source_instance_id = Uuid::now_v7().to_string();
        sqlx::query(
            r#"
            INSERT INTO workspaces (id, name, timezone, source_instance_id, created_at)
            VALUES (?1, '测试工作区', 'Asia/Shanghai', ?2, '2026-07-31T00:00:00Z')
            "#,
        )
        .bind(&workspace_id)
        .bind(&source_instance_id)
        .execute(&pool)
        .await
        .expect("seed workspace");
        (pool, workspace_id, source_instance_id)
    }

    fn export_request(workspace_id: String) -> ExportRequest {
        ExportRequest {
            export_id: "01983a61-9c00-7000-8000-000000000001".to_owned(),
            workspace_id,
            exported_at: "2026-07-31T00:00:00Z".to_owned(),
        }
    }

    #[tokio::test]
    async fn export_is_deterministic_and_validates_before_import() {
        let (pool, workspace_id, source_instance_id) = test_pool().await;
        let quality_label_id = Uuid::now_v7().to_string();
        sqlx::query(
            r#"
            INSERT INTO quality_labels
                (id, workspace_id, name, normalized_name, disposition, active,
                 created_at, updated_at)
            VALUES (?1, ?2, '外观完好', '外观完好', 'available', 1,
                    '2026-07-31T00:00:00Z', '2026-07-31T00:00:00Z')
            "#,
        )
        .bind(&quality_label_id)
        .bind(&workspace_id)
        .execute(&pool)
        .await
        .expect("seed quality label");
        sqlx::query(
            r#"
            INSERT INTO quality_label_name_history
                (id, workspace_id, quality_label_id, old_name, new_name,
                 changed_by, changed_by_snapshot, change_note, changed_at)
            VALUES (?1, ?2, ?3, '外观正常', '外观完好', 'local', '本机操作',
                    '统一命名', '2026-07-31T00:00:01Z')
            "#,
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&workspace_id)
        .bind(&quality_label_id)
        .execute(&pool)
        .await
        .expect("seed quality label history");
        sqlx::query(
            r#"
            INSERT INTO audit_logs
                (id, workspace_id, actor_id, action, entity_type, entity_id,
                 request_id, result, details_json, occurred_at)
            VALUES (?1, ?2, 'operator', 'upgrade_test', 'workspace', ?2,
                    'request-1', 'success',
                    '{"visible":"kept","nested":{"refresh_token":"must-not-leak"}}',
                    '2026-07-31T00:00:00Z')
            "#,
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&workspace_id)
        .execute(&pool)
        .await
        .expect("seed redacted audit data");
        let directory = TestDirectory::new();
        let first_path = directory.path().join("first.invpack");
        let second_path = directory.path().join("second.invpack");
        let exporter = OfflineUpgradeExporter::new(&pool);
        let first = exporter
            .export(&first_path, export_request(workspace_id.clone()))
            .await
            .expect("export first package");
        let second = exporter
            .export(&second_path, export_request(workspace_id.clone()))
            .await
            .expect("export identical package");

        for member in DATA_FILES
            .iter()
            .map(|file| file.path)
            .chain([MANIFEST_FILE, CHECKSUMS_FILE])
        {
            assert_eq!(
                fs::read(first.path.join(member)).expect("read first member"),
                fs::read(second.path.join(member)).expect("read second member"),
                "{member} must be deterministic"
            );
        }
        assert_eq!(first.package_checksum, second.package_checksum);
        assert_eq!(
            first.manifest.migration_id,
            deterministic_migration_id(
                &source_instance_id,
                &workspace_id,
                &first.manifest.export_id
            )
        );

        let validated = validate_package(&first.path).expect("validate exported package");
        assert_eq!(validated.manifest, first.manifest);
        assert_eq!(validated.entity_counts.get("workspaces"), Some(&1));
        assert_eq!(validated.entity_counts.get("quality_labels"), Some(&1));
        assert_eq!(
            validated.entity_counts.get("quality_label_name_history"),
            Some(&1)
        );
        assert_eq!(validated.entity_counts.get("audit_logs"), Some(&1));
        assert_eq!(validated.package_checksum, first.package_checksum);
        let audit = String::from_utf8(fs::read(first.path.join("audit.jsonl")).unwrap())
            .expect("audit is UTF-8");
        assert!(!audit.contains("must-not-leak"));
        assert!(audit.contains("[REDACTED]"));
        assert!(audit.contains("kept"));
    }

    #[tokio::test]
    async fn export_keeps_document_voids_and_excludes_operation_credentials() {
        let directory = TestDirectory::new();
        let database = OfflineDatabase::open(&directory.path().join("source.sqlite3"))
            .await
            .expect("open offline database");
        database
            .create_catalog_product(CreateCatalogProductRequest {
                code: "RAM-VOID-EXPORT".to_owned(),
                name: "作废导出测试内存".to_owned(),
                serial_prefix: None,
                serial_forbidden_chars: String::new(),
            })
            .await
            .expect("create product");
        for (display_name, role) in [
            ("升级测试货主", CatalogPartyRole::GoodsOwner),
            ("升级测试供应商", CatalogPartyRole::Supplier),
        ] {
            database
                .create_catalog_party(CreateCatalogPartyRequest {
                    display_name: display_name.to_owned(),
                    role,
                })
                .await
                .expect("create party");
        }
        let receipt = database
            .post_receipt(PostReceiptRequest {
                request_id: "receipt-request-void-export".to_owned(),
                idempotency_key: "receipt-key-void-export".to_owned(),
                receipt_no: "RK-VOID-EXPORT".to_owned(),
                owner_name: "升级测试货主".to_owned(),
                supplier_name: "升级测试供应商".to_owned(),
                sku_code: "RAM-VOID-EXPORT".to_owned(),
                sku_name: "作废导出测试内存".to_owned(),
                source_reference: None,
                received_at: "2026-08-15T00:00:00Z".to_owned(),
                actor_id: "upgrade-operator".to_owned(),
                barcodes: vec!["VOID-EXPORT-001".to_owned()],
                notes: None,
                warranty: None,
            })
            .await
            .expect("post receipt");
        database
            .void_receipt_document(VoidDocumentRequest {
                document_id: receipt.receipt_id,
                reason: "验证升级包保留作废事实".to_owned(),
                password: "admin".to_owned(),
                actor_id: Some("upgrade-operator".to_owned()),
                request_id: "void-request-export".to_owned(),
                idempotency_key: "void-key-export".to_owned(),
            })
            .await
            .expect("void receipt");
        let operation_hash: String =
            sqlx::query_scalar("SELECT password_hash FROM operation_credentials WHERE id=1")
                .fetch_one(database.pool())
                .await
                .expect("read operation password hash");

        let package = OfflineUpgradeExporter::new(database.pool())
            .export(
                &directory.path().join("void-export.invpack"),
                export_request(database.workspace_id().to_owned()),
            )
            .await
            .expect("export package");
        let validated = validate_package(&package.path).expect("validate package");
        assert_eq!(validated.entity_counts.get("document_voids"), Some(&1));
        let audit = fs::read_to_string(package.path.join("audit.jsonl"))
            .expect("read audit package member");
        assert!(audit.contains("\"table\":\"document_voids\""));
        assert!(audit.contains("验证升级包保留作废事实"));
        for digest in &package.manifest.files {
            let member = fs::read_to_string(package.path.join(&digest.path))
                .expect("read package data member");
            assert!(!member.contains("operation_credentials"));
            assert!(!member.contains(&operation_hash));
        }
        database.pool().close().await;
    }

    #[tokio::test]
    async fn changed_data_file_is_rejected_before_database_import() {
        let (pool, workspace_id, _) = test_pool().await;
        let directory = TestDirectory::new();
        let package_path = directory.path().join("tampered.invpack");
        OfflineUpgradeExporter::new(&pool)
            .export(&package_path, export_request(workspace_id))
            .await
            .expect("export package");

        let outbound = package_path.join("outbound.jsonl");
        File::options()
            .append(true)
            .open(&outbound)
            .expect("open outbound data")
            .write_all(b"{}\n")
            .expect("tamper data");
        let error = validate_package(&package_path).expect_err("tampering must fail");
        assert!(matches!(error, UpgradeError::Integrity(_)));
    }

    #[tokio::test]
    async fn network_upload_round_trips_only_canonical_members_and_cleans_staging() {
        let (pool, workspace_id, _) = test_pool().await;
        sqlx::query("INSERT INTO _sqlx_migrations (version, success) VALUES (6, 1)")
            .execute(&pool)
            .await
            .expect("record newer physical migration");
        let directory = TestDirectory::new();
        let package_path = directory.path().join("network-upload.invpack");
        OfflineUpgradeExporter::new(&pool)
            .export(&package_path, export_request(workspace_id))
            .await
            .expect("export package");
        let package = validate_package(&package_path).expect("validate package");
        let target_workspace_id = Uuid::now_v7();
        let request = build_network_upgrade_request(&package, target_workspace_id)
            .expect("build upload request");
        assert_eq!(request.target_workspace_id, target_workspace_id);
        assert_eq!(
            request
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            DATA_FILES.iter().map(|file| file.path).collect::<Vec<_>>()
        );
        let staged = stage_network_upgrade_request(request.clone()).expect("stage upload");
        assert_eq!(staged.package().manifest, package.manifest);
        assert_eq!(staged.package().package_checksum, package.package_checksum);
        let staged_root = staged.package().root().to_owned();
        assert!(staged_root.exists());
        drop(staged);
        assert!(!staged_root.exists());

        let mut unsafe_request = request;
        unsafe_request.files[0].path = "../master-data.jsonl".to_owned();
        assert!(matches!(
            stage_network_upgrade_request(unsafe_request),
            Err(UpgradeError::Integrity(_)) | Err(UpgradeError::UnsafePath(_))
        ));
    }

    #[test]
    fn traversal_and_non_invpack_paths_are_rejected() {
        assert!(matches!(
            validate_relative_path("../outside.jsonl"),
            Err(UpgradeError::UnsafePath(_))
        ));
        assert!(matches!(
            validate_relative_path("./master-data.jsonl"),
            Err(UpgradeError::UnsafePath(_))
        ));
        let request = export_request(Uuid::now_v7().to_string());
        assert!(matches!(
            validate_export_request(Path::new("export.zip"), &request),
            Err(UpgradeError::InvalidRequest(_))
        ));
    }

    #[test]
    fn migration_identity_is_stable_and_source_scoped() {
        let source = Uuid::now_v7().to_string();
        let workspace = Uuid::now_v7().to_string();
        let export = Uuid::now_v7().to_string();
        let first = deterministic_migration_id(&source, &workspace, &export);
        assert_eq!(
            first,
            deterministic_migration_id(&source, &workspace, &export)
        );
        assert_ne!(
            first,
            deterministic_migration_id(&Uuid::now_v7().to_string(), &workspace, &export)
        );
    }

    #[test]
    fn package_validation_allows_a_returned_unit_to_ship_again() {
        let inventory_unit_id = Uuid::now_v7().to_string();
        let first_line_id = Uuid::now_v7().to_string();
        let second_line_id = Uuid::now_v7().to_string();
        let shipment_line = |id: &str| {
            serde_json::json!({
                "id": id,
                "inventory_unit_id": inventory_unit_id,
            })
            .as_object()
            .expect("shipment line object")
            .clone()
        };
        let mut records = BTreeMap::from([
            (
                "outbound_shipment_lines".to_owned(),
                vec![
                    shipment_line(&first_line_id),
                    shipment_line(&second_line_id),
                ],
            ),
            (
                "outbound_return_lines".to_owned(),
                vec![serde_json::json!({
                    "outbound_shipment_line_id": first_line_id,
                })
                .as_object()
                .expect("return line object")
                .clone()],
            ),
        ]);

        validate_active_shipment_line_uniqueness(&records)
            .expect("returned shipment history is not active");
        records
            .get_mut("outbound_return_lines")
            .expect("return lines")
            .clear();
        let error = validate_active_shipment_line_uniqueness(&records)
            .expect_err("two unreturned lines for one unit must fail");
        assert!(error
            .to_string()
            .contains("more than one active shipment line"));
    }

    #[derive(Debug)]
    struct FakeAdapterError;

    impl Display for FakeAdapterError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("fake adapter error")
        }
    }

    impl std::error::Error for FakeAdapterError {}

    struct FakeAdapter {
        events: Arc<Mutex<Vec<String>>>,
    }

    struct FakeTransaction {
        events: Arc<Mutex<Vec<String>>>,
        staged_rows: BTreeMap<String, u64>,
        committed: bool,
    }

    impl Drop for FakeTransaction {
        fn drop(&mut self) {
            if !self.committed {
                self.events
                    .lock()
                    .expect("event lock")
                    .push("rollback".into());
            }
        }
    }

    impl PostgresUpgradeTransaction for FakeTransaction {
        type Error = FakeAdapterError;

        fn claim_migration(
            &mut self,
            _claim: MigrationClaim,
        ) -> impl Future<Output = Result<MigrationClaimState, Self::Error>> + Send {
            let events = Arc::clone(&self.events);
            async move {
                events.lock().expect("event lock").push("claim".into());
                Ok(MigrationClaimState::New)
            }
        }

        fn stage_file(
            &mut self,
            file: StagedDataFile,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.staged_rows
                .insert(file.digest.path.clone(), file.digest.rows);
            let events = Arc::clone(&self.events);
            async move {
                events
                    .lock()
                    .expect("event lock")
                    .push(format!("stage:{}", file.digest.path));
                Ok(())
            }
        }

        fn validate_staging(
            &mut self,
            _manifest: PackageManifest,
            _target: NetworkUpgradeTarget,
        ) -> impl Future<Output = Result<PostgresStagingReport, Self::Error>> + Send {
            let events = Arc::clone(&self.events);
            let file_rows = self.staged_rows.clone();
            async move {
                events.lock().expect("event lock").push("validate".into());
                Ok(PostgresStagingReport {
                    file_rows,
                    target_workspace_is_empty: true,
                    uniqueness_validated: true,
                    references_validated: true,
                    quantities_validated: true,
                })
            }
        }

        fn apply_staged_package(
            &mut self,
            _manifest: PackageManifest,
            _target: NetworkUpgradeTarget,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            let events = Arc::clone(&self.events);
            async move {
                events.lock().expect("event lock").push("apply".into());
                Ok(())
            }
        }

        fn record_imported(
            &mut self,
            _claim: MigrationClaim,
        ) -> impl Future<Output = Result<String, Self::Error>> + Send {
            let events = Arc::clone(&self.events);
            async move {
                events.lock().expect("event lock").push("record".into());
                Ok("2026-08-04 00:00:00+00".to_owned())
            }
        }

        fn commit(mut self) -> impl Future<Output = Result<(), Self::Error>> + Send
        where
            Self: Sized,
        {
            self.committed = true;
            let events = Arc::clone(&self.events);
            async move {
                events.lock().expect("event lock").push("commit".into());
                Ok(())
            }
        }
    }

    impl PostgresUpgradeAdapter for FakeAdapter {
        type Error = FakeAdapterError;
        type Transaction<'a> = FakeTransaction;

        fn begin_upgrade(
            &self,
        ) -> impl Future<Output = Result<Self::Transaction<'_>, Self::Error>> + Send {
            let events = Arc::clone(&self.events);
            async move {
                events.lock().expect("event lock").push("begin".into());
                Ok(FakeTransaction {
                    events,
                    staged_rows: BTreeMap::new(),
                    committed: false,
                })
            }
        }
    }

    #[tokio::test]
    async fn postgres_contract_uses_one_transaction_for_the_whole_import() {
        let (pool, workspace_id, _) = test_pool().await;
        let directory = TestDirectory::new();
        let package_path = directory.path().join("transaction.invpack");
        OfflineUpgradeExporter::new(&pool)
            .export(&package_path, export_request(workspace_id))
            .await
            .expect("export package");
        let package = validate_package(&package_path).expect("validate package");
        let events = Arc::new(Mutex::new(Vec::new()));
        let adapter = FakeAdapter {
            events: Arc::clone(&events),
        };

        let outcome = import_to_postgres(
            &adapter,
            &package,
            NetworkUpgradeTarget {
                tenant_id: Uuid::now_v7().to_string(),
                workspace_id: Uuid::now_v7().to_string(),
                actor_id: Uuid::now_v7().to_string(),
            },
        )
        .await
        .expect("import through transaction contract");

        assert_eq!(
            outcome,
            ImportOutcome::Imported {
                migration_id: package.manifest.migration_id.clone(),
                imported_at: "2026-08-04 00:00:00+00".to_owned(),
            }
        );
        assert_eq!(
            *events.lock().expect("event lock"),
            vec![
                "begin",
                "claim",
                "stage:master-data.jsonl",
                "stage:inbound.jsonl",
                "stage:quality.jsonl",
                "stage:outbound.jsonl",
                "stage:audit.jsonl",
                "validate",
                "apply",
                "record",
                "commit",
            ]
        );
    }

    /// Exercise the concrete adapter against a real restricted PostgreSQL
    /// role.  The test intentionally remains ignored in normal unit runs;
    /// CI or a local POC must provide both URLs and a database migrated with
    /// the runtime role grants.
    #[tokio::test]
    #[ignore = "requires INVENTORY_NETWORK_TEST_ADMIN_URL and INVENTORY_NETWORK_TEST_RUNTIME_URL"]
    async fn postgres_adapter_imports_idempotently_and_keeps_failed_apply_atomic() {
        let admin_url =
            std::env::var("INVENTORY_NETWORK_TEST_ADMIN_URL").expect("network test admin URL");
        let runtime_url =
            std::env::var("INVENTORY_NETWORK_TEST_RUNTIME_URL").expect("network test runtime URL");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&admin_url)
            .await
            .expect("connect admin database");

        let tenant_id = Uuid::now_v7();
        let user_id = Uuid::now_v7();
        let membership_id = Uuid::now_v7();
        let role_id = Uuid::now_v7();
        let access_permission_id = Uuid::now_v7();
        let upgrade_permission_id = Uuid::now_v7();
        let device_id = Uuid::now_v7();
        let source_instance_id = Uuid::now_v7();
        let target_workspace_id = Uuid::now_v7();
        let session_token = format!("upgrade-session-{tenant_id}");
        let session_id = Uuid::now_v7();
        let password = "upgrade-test-password";
        let password_hash = crate::v2::auth::PasswordService::recommended()
            .expect("password service")
            .hash_password(password)
            .expect("hash password");

        let mut setup = admin.begin().await.expect("begin setup");
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *setup)
            .await
            .expect("set tenant context");
        sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, 'Upgrade Test Tenant')")
            .bind(tenant_id)
            .bind(format!("upgrade-{}", tenant_id.simple()))
            .execute(&mut *setup)
            .await
            .expect("insert tenant");
        sqlx::query("INSERT INTO users (tenant_id, id, login, normalized_login, display_name) VALUES ($1, $2, 'upgrade', 'upgrade', 'Upgrade User')")
            .bind(tenant_id)
            .bind(user_id)
            .execute(&mut *setup)
            .await
            .expect("insert user");
        sqlx::query(
            "INSERT INTO credentials (tenant_id, user_id, password_hash) VALUES ($1, $2, $3)",
        )
        .bind(tenant_id)
        .bind(user_id)
        .bind(password_hash)
        .execute(&mut *setup)
        .await
        .expect("insert credential");
        sqlx::query("INSERT INTO memberships (tenant_id, id, user_id) VALUES ($1, $2, $3)")
            .bind(tenant_id)
            .bind(membership_id)
            .bind(user_id)
            .execute(&mut *setup)
            .await
            .expect("insert membership");
        sqlx::query(
            "INSERT INTO roles (tenant_id, id, code, name) VALUES ($1, $2, 'upgrade', 'Upgrade')",
        )
        .bind(tenant_id)
        .bind(role_id)
        .execute(&mut *setup)
        .await
        .expect("insert role");
        for (proposed_id, code) in [
            (access_permission_id, PERMISSION_NETWORK_ACCESS),
            (upgrade_permission_id, "inventory.upgrade.import"),
        ] {
            let permission_id: Uuid = sqlx::query_scalar("INSERT INTO permissions (tenant_id, id, code, description) VALUES ($1, $2, $3, $4) ON CONFLICT (tenant_id, code) DO UPDATE SET description = EXCLUDED.description RETURNING id")
                .bind(tenant_id)
                .bind(proposed_id)
                .bind(code)
                .bind(code)
                .fetch_one(&mut *setup)
                .await
                .expect("insert permission");
            sqlx::query("INSERT INTO role_permissions (tenant_id, role_id, permission_id) VALUES ($1, $2, $3)")
                .bind(tenant_id)
                .bind(role_id)
                .bind(permission_id)
                .execute(&mut *setup)
                .await
                .expect("assign permission");
        }
        sqlx::query(
            "INSERT INTO membership_roles (tenant_id, membership_id, role_id) VALUES ($1, $2, $3)",
        )
        .bind(tenant_id)
        .bind(membership_id)
        .bind(role_id)
        .execute(&mut *setup)
        .await
        .expect("assign role");
        sqlx::query("INSERT INTO devices (tenant_id, id, membership_id, user_id, device_fingerprint, display_name) VALUES ($1, $2, $3, $4, $5, 'Upgrade Device')")
            .bind(tenant_id)
            .bind(device_id)
            .bind(membership_id)
            .bind(user_id)
            .bind(format!("upgrade-device-{device_id}"))
            .execute(&mut *setup)
            .await
            .expect("insert device");
        sqlx::query("INSERT INTO license_entitlements (tenant_id, id, license_id, edition, status, seat_limit, starts_at, expires_at, issuer, signature, key_id, claims_hash, verified_at) VALUES ($1, $2, $3, 'network', 'active', 5, CURRENT_TIMESTAMP - INTERVAL '1 hour', CURRENT_TIMESTAMP + INTERVAL '1 day', 'integration-test', 'test-signature', 'test-key', $4, CURRENT_TIMESTAMP)")
            .bind(tenant_id)
            .bind(Uuid::now_v7())
            .bind(format!("UPGRADE-{tenant_id}"))
            .bind("b".repeat(64))
            .execute(&mut *setup)
            .await
            .expect("insert entitlement");
        sqlx::query("INSERT INTO sessions (tenant_id, id, membership_id, user_id, device_id, token_hash, issued_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP + INTERVAL '1 hour')")
            .bind(tenant_id)
            .bind(session_id)
            .bind(membership_id)
            .bind(user_id)
            .bind(device_id)
            .bind(hash_token(TokenKind::Session, &session_token).as_slice())
            .execute(&mut *setup)
            .await
            .expect("insert session");
        setup.commit().await.expect("commit setup");

        let directory = TestDirectory::new();
        let source_path = directory.path().join("source.sqlite");
        let source = OfflineDatabase::open(&source_path)
            .await
            .expect("open source offline database");
        sqlx::query("UPDATE workspaces SET source_instance_id = ?1 WHERE id = ?2")
            .bind(source_instance_id.to_string())
            .bind(source.workspace_id())
            .execute(source.pool())
            .await
            .expect("set source instance");
        for owner in ["Upgrade Owner A", "Upgrade Owner B"] {
            source
                .create_catalog_party(CreateCatalogPartyRequest {
                    display_name: owner.to_owned(),
                    role: CatalogPartyRole::GoodsOwner,
                })
                .await
                .expect("create upgrade source owner");
        }
        for supplier in ["Upgrade Supplier A", "Upgrade Supplier B"] {
            source
                .create_catalog_party(CreateCatalogPartyRequest {
                    display_name: supplier.to_owned(),
                    role: CatalogPartyRole::Supplier,
                })
                .await
                .expect("create upgrade source supplier");
        }
        source
            .create_catalog_product(CreateCatalogProductRequest {
                code: "UPGRADE-SKU".to_owned(),
                name: "Upgrade Model".to_owned(),
                serial_prefix: Some("UPGRADE-".to_owned()),
                serial_forbidden_chars: "#, ".to_owned(),
            })
            .await
            .expect("create upgrade source product");

        let source_barcode_a = format!("UPGRADE-A-{tenant_id}");
        let source_barcode_b = format!("UPGRADE-B-{tenant_id}");
        let receipt_a = source
            .post_receipt(PostReceiptRequest {
                request_id: "source-receipt-a".to_owned(),
                idempotency_key: "source-receipt-a-key".to_owned(),
                receipt_no: format!("UPGRADE-RA-{}", tenant_id.simple()),
                owner_name: "Upgrade Owner A".to_owned(),
                supplier_name: "Upgrade Supplier A".to_owned(),
                sku_code: "UPGRADE-SKU".to_owned(),
                sku_name: "Upgrade Model".to_owned(),
                source_reference: Some("offline-batch-a".to_owned()),
                received_at: "2026-08-03T00:00:00Z".to_owned(),
                actor_id: "offline-receiver-a".to_owned(),
                barcodes: vec![source_barcode_a.clone()],
                notes: None,
                warranty: None,
            })
            .await
            .expect("post source receipt A");
        let receipt_b = source
            .post_receipt(PostReceiptRequest {
                request_id: "source-receipt-b".to_owned(),
                idempotency_key: "source-receipt-b-key".to_owned(),
                receipt_no: format!("UPGRADE-RB-{}", tenant_id.simple()),
                owner_name: "Upgrade Owner B".to_owned(),
                supplier_name: "Upgrade Supplier B".to_owned(),
                sku_code: "UPGRADE-SKU".to_owned(),
                sku_name: "Upgrade Model".to_owned(),
                source_reference: Some("offline-batch-b".to_owned()),
                received_at: "2026-08-03T00:01:00Z".to_owned(),
                actor_id: "offline-receiver-b".to_owned(),
                barcodes: vec![source_barcode_b.clone()],
                notes: None,
                warranty: None,
            })
            .await
            .expect("post source receipt B");

        let inspection_a = source
            .complete_inspection(CompleteInspectionRequest {
                request_id: "source-inspection-a".to_owned(),
                idempotency_key: "source-inspection-a-key".to_owned(),
                inspection_no: format!("UPGRADE-QA-{}", tenant_id.simple()),
                inspection_kind: InspectionKind::Initial,
                inspector_id: "offline-inspector-a".to_owned(),
                inspected_at: "2026-08-03T00:10:00Z".to_owned(),
                results: vec![InspectionResultInput {
                    barcode: source_barcode_a.clone(),
                    outcome: QualityOutcome::Passed,
                    quality_label_id: None,
                    defect_code: None,
                    measurements: serde_json::json!({"voltage": 3.3}),
                    notes: None,
                }],
            })
            .await
            .expect("pass source unit A");
        let inspection_b = source
            .complete_inspection(CompleteInspectionRequest {
                request_id: "source-inspection-b".to_owned(),
                idempotency_key: "source-inspection-b-key".to_owned(),
                inspection_no: format!("UPGRADE-QB-{}", tenant_id.simple()),
                inspection_kind: InspectionKind::Initial,
                inspector_id: "offline-inspector-b".to_owned(),
                inspected_at: "2026-08-03T00:11:00Z".to_owned(),
                results: vec![InspectionResultInput {
                    barcode: source_barcode_b.clone(),
                    outcome: QualityOutcome::Passed,
                    quality_label_id: None,
                    defect_code: None,
                    measurements: serde_json::json!({"voltage": 3.4}),
                    notes: None,
                }],
            })
            .await
            .expect("pass source unit B");

        let first_order = source
            .create_outbound_order(CreateOutboundOrderRequest {
                request_id: "source-order-one".to_owned(),
                idempotency_key: "source-order-one-key".to_owned(),
                order_no: format!("UPGRADE-O1-{}", tenant_id.simple()),
                upstream_receiver_name: "Upgrade Upstream One".to_owned(),
                sku_code: "UPGRADE-SKU".to_owned(),
                sku_name: "Upgrade Model".to_owned(),
                required_quantity: 2,
                required_at: Some("2026-08-03T01:00:00Z".to_owned()),
                actor_id: "offline-order-creator-one".to_owned(),
            })
            .await
            .expect("create first source order");
        let first_allocation = source
            .allocate_outbound_order(AllocateOutboundRequest {
                request_id: "source-allocation-one".to_owned(),
                idempotency_key: "source-allocation-one-key".to_owned(),
                order_id: first_order.order_id.clone(),
                order_line_id: first_order.order_line_id.clone(),
                barcodes: Vec::new(),
                allow_mixed_skus: false,
                actor_id: "offline-allocator-one".to_owned(),
            })
            .await
            .expect("allocate two owners to first source order");
        assert_eq!(first_allocation.allocated_count, 2);
        assert_eq!(
            first_allocation
                .allocations
                .iter()
                .map(|allocation| allocation.owner_party_id.as_str())
                .collect::<HashSet<_>>()
                .len(),
            2
        );
        let first_shipment = source
            .ship_outbound_order(ShipOutboundRequest {
                request_id: "source-shipment-one".to_owned(),
                idempotency_key: "source-shipment-one-key".to_owned(),
                order_id: first_order.order_id.clone(),
                shipment_no: format!("UPGRADE-S1-{}", tenant_id.simple()),
                allocation_ids: first_allocation
                    .allocations
                    .iter()
                    .map(|allocation| allocation.allocation_id.clone())
                    .collect(),
                barcodes: Vec::new(),
                shipped_at: "2026-08-03T01:10:00Z".to_owned(),
                actor_id: "offline-shipper-one".to_owned(),
                warranty: None,
            })
            .await
            .expect("ship first source order");
        let first_delivery = source
            .confirm_outbound_delivery(ConfirmOutboundDeliveryRequest {
                request_id: "source-delivery-one".to_owned(),
                idempotency_key: "source-delivery-one-key".to_owned(),
                shipment_id: first_shipment.shipment_id.clone(),
                confirmation_code: format!("UPGRADE-D1-{}", tenant_id.simple()),
                shipment_line_ids: Vec::new(),
                confirmed_at: "2026-08-03T01:20:00Z".to_owned(),
                confirmed_by: "offline-delivery-confirmer".to_owned(),
                notes: Some("both units accepted before one was returned".to_owned()),
            })
            .await
            .expect("confirm first source delivery");
        let returned_item = first_shipment.items[0].clone();
        let delivered_item = first_shipment.items[1].clone();
        let first_return = source
            .return_outbound_shipment(ReturnOutboundShipmentRequest {
                request_id: "source-return-one".to_owned(),
                idempotency_key: "source-return-one-key".to_owned(),
                shipment_id: first_shipment.shipment_id.clone(),
                shipment_line_ids: vec![returned_item.shipment_line_id.clone()],
                return_no: format!("UPGRADE-RT1-{}", tenant_id.simple()),
                returned_at: "2026-08-03T01:30:00Z".to_owned(),
                reason: "upstream rejected one unit".to_owned(),
                actor_id: "offline-return-operator".to_owned(),
            })
            .await
            .expect("return one source shipment line");
        let retest = source
            .complete_inspection(CompleteInspectionRequest {
                request_id: "source-retest".to_owned(),
                idempotency_key: "source-retest-key".to_owned(),
                inspection_no: format!("UPGRADE-QR-{}", tenant_id.simple()),
                inspection_kind: InspectionKind::Retest,
                inspector_id: "offline-retest-inspector".to_owned(),
                inspected_at: "2026-08-03T01:40:00Z".to_owned(),
                results: vec![InspectionResultInput {
                    barcode: returned_item.barcode.clone(),
                    outcome: QualityOutcome::Passed,
                    quality_label_id: None,
                    defect_code: None,
                    measurements: serde_json::json!({"retest": true}),
                    notes: Some("return retest passed".to_owned()),
                }],
            })
            .await
            .expect("retest returned source unit");
        let second_order = source
            .create_outbound_order(CreateOutboundOrderRequest {
                request_id: "source-order-two".to_owned(),
                idempotency_key: "source-order-two-key".to_owned(),
                order_no: format!("UPGRADE-O2-{}", tenant_id.simple()),
                upstream_receiver_name: "Upgrade Upstream Two".to_owned(),
                sku_code: "UPGRADE-SKU".to_owned(),
                sku_name: "Upgrade Model".to_owned(),
                required_quantity: 1,
                required_at: Some("2026-08-03T02:00:00Z".to_owned()),
                actor_id: "offline-order-creator-two".to_owned(),
            })
            .await
            .expect("create second source order");
        let second_allocation = source
            .allocate_outbound_order(AllocateOutboundRequest {
                request_id: "source-allocation-two".to_owned(),
                idempotency_key: "source-allocation-two-key".to_owned(),
                order_id: second_order.order_id.clone(),
                order_line_id: second_order.order_line_id.clone(),
                barcodes: vec![returned_item.barcode.clone()],
                allow_mixed_skus: false,
                actor_id: "offline-allocator-two".to_owned(),
            })
            .await
            .expect("allocate returned source unit again");
        let second_shipment = source
            .ship_outbound_order(ShipOutboundRequest {
                request_id: "source-shipment-two".to_owned(),
                idempotency_key: "source-shipment-two-key".to_owned(),
                order_id: second_order.order_id.clone(),
                shipment_no: format!("UPGRADE-S2-{}", tenant_id.simple()),
                allocation_ids: vec![second_allocation.allocations[0].allocation_id.clone()],
                barcodes: Vec::new(),
                shipped_at: "2026-08-03T02:10:00Z".to_owned(),
                actor_id: "offline-shipper-two".to_owned(),
                warranty: None,
            })
            .await
            .expect("ship returned source unit again");

        let package_path = directory.path().join("upgrade.invpack");
        let package = OfflineUpgradeExporter::new(source.pool())
            .export(
                &package_path,
                ExportRequest {
                    export_id: Uuid::now_v7().to_string(),
                    workspace_id: source.workspace_id().to_owned(),
                    exported_at: "2026-08-03T03:00:00Z".to_owned(),
                },
            )
            .await
            .expect("export package");
        let validated = validate_package(&package.path).expect("validate package");

        let runtime = NetworkDatabase::connect(&NetworkDatabaseConfig::new(runtime_url))
            .await
            .expect("connect restricted runtime");
        let service = NetworkService::new(runtime).expect("network service");
        let upload = build_network_upgrade_request(&validated, target_workspace_id)
            .expect("build network upload");
        let first = service
            .import_upgrade_package(tenant_id, &session_token, upload.clone())
            .await
            .expect("import package");
        assert_eq!(first.status, NetworkUpgradeImportStatus::Imported);
        assert_eq!(first.checksum, validated.package_checksum);
        let replay = service
            .import_upgrade_package(tenant_id, &session_token, upload)
            .await
            .expect("idempotent replay");
        assert_eq!(replay.status, NetworkUpgradeImportStatus::AlreadyImported);

        let mut verification = admin.begin().await.expect("begin verification");
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *verification)
            .await
            .expect("set verification context");
        let counts = sqlx::query(
            r#"
            SELECT
                (SELECT count(*) FROM workspaces WHERE tenant_id = $1) AS workspaces,
                (SELECT count(*) FROM business_parties WHERE tenant_id = $1) AS parties,
                (SELECT count(*) FROM inbound_receipts WHERE tenant_id = $1) AS receipts,
                (SELECT count(*) FROM inbound_receipt_lines WHERE tenant_id = $1) AS receipt_lines,
                (SELECT count(*) FROM inventory_units WHERE tenant_id = $1) AS units,
                (SELECT count(*) FROM quality_inspections WHERE tenant_id = $1) AS inspections,
                (SELECT count(*) FROM quality_inspection_results WHERE tenant_id = $1) AS inspection_results,
                (SELECT count(*) FROM outbound_orders WHERE tenant_id = $1) AS orders,
                (SELECT count(*) FROM outbound_allocations WHERE tenant_id = $1) AS allocations,
                (SELECT count(*) FROM outbound_shipments WHERE tenant_id = $1) AS shipments,
                (SELECT count(*) FROM outbound_shipment_lines WHERE tenant_id = $1) AS shipment_lines,
                (SELECT count(*) FROM delivery_confirmations WHERE tenant_id = $1) AS deliveries,
                (SELECT count(*) FROM outbound_return_batches WHERE tenant_id = $1) AS returns,
                (SELECT count(*) FROM migration_packages WHERE tenant_id = $1) AS packages
            "#,
        )
        .bind(tenant_id)
        .fetch_one(&mut *verification)
        .await
        .expect("verify imported rows");
        for (column, expected) in [
            ("workspaces", 1_i64),
            ("parties", 6),
            ("receipts", 2),
            ("receipt_lines", 2),
            ("units", 2),
            ("inspections", 3),
            ("inspection_results", 3),
            ("orders", 2),
            ("allocations", 3),
            ("shipments", 2),
            ("shipment_lines", 3),
            ("deliveries", 1),
            ("returns", 1),
            ("packages", 1),
        ] {
            assert_eq!(
                counts.try_get::<i64, _>(column).expect("read count"),
                expected,
                "unexpected imported row count for {column}"
            );
        }

        let imported_scanner_rules: (Option<String>, String) = sqlx::query_as(
            "SELECT serial_prefix, serial_forbidden_chars FROM skus WHERE tenant_id = $1 AND code = 'UPGRADE-SKU'",
        )
        .bind(tenant_id)
        .fetch_one(&mut *verification)
        .await
        .expect("load imported scanner rules");
        assert_eq!(imported_scanner_rules.0.as_deref(), Some("UPGRADE-"));
        assert_eq!(imported_scanner_rules.1, "#, ");

        let imported_suppliers: BTreeMap<String, String> = sqlx::query_as(
            r#"
            SELECT ir.receipt_no, supplier.display_name
              FROM inbound_receipts ir
              JOIN business_parties supplier
                ON supplier.tenant_id = ir.tenant_id
               AND supplier.id = ir.supplier_party_id
             WHERE ir.tenant_id = $1
            "#,
        )
        .bind(tenant_id)
        .fetch_all(&mut *verification)
        .await
        .expect("load imported supplier provenance")
        .into_iter()
        .collect();
        assert_eq!(
            imported_suppliers.get(&receipt_a.receipt_no),
            Some(&"Upgrade Supplier A".to_owned())
        );
        assert_eq!(
            imported_suppliers.get(&receipt_b.receipt_no),
            Some(&"Upgrade Supplier B".to_owned())
        );

        let imported_identities: BTreeSet<(String, String)> = sqlx::query_as::<_, (String, String)>(
            r#"
            SELECT 'receipt', id::text FROM inbound_receipts WHERE tenant_id = $1
            UNION ALL SELECT 'unit', id::text FROM inventory_units WHERE tenant_id = $1
            UNION ALL SELECT 'inspection', id::text FROM quality_inspections WHERE tenant_id = $1
            UNION ALL SELECT 'order', id::text FROM outbound_orders WHERE tenant_id = $1
            UNION ALL SELECT 'allocation', id::text FROM outbound_allocations WHERE tenant_id = $1
            UNION ALL SELECT 'shipment', id::text FROM outbound_shipments WHERE tenant_id = $1
            UNION ALL SELECT 'shipment_line', id::text FROM outbound_shipment_lines WHERE tenant_id = $1
            UNION ALL SELECT 'delivery', id::text FROM delivery_confirmations WHERE tenant_id = $1
            UNION ALL SELECT 'return', id::text FROM outbound_return_batches WHERE tenant_id = $1
            "#,
        )
        .bind(tenant_id)
        .fetch_all(&mut *verification)
        .await
        .expect("load imported stable identities")
        .into_iter()
        .collect();
        let expected_identities: BTreeSet<(String, String)> = [
            ("receipt", receipt_a.receipt_id.as_str()),
            ("receipt", receipt_b.receipt_id.as_str()),
            ("unit", receipt_a.units[0].inventory_unit_id.as_str()),
            ("unit", receipt_b.units[0].inventory_unit_id.as_str()),
            ("inspection", inspection_a.inspection_id.as_str()),
            ("inspection", inspection_b.inspection_id.as_str()),
            ("inspection", retest.inspection_id.as_str()),
            ("order", first_order.order_id.as_str()),
            ("order", second_order.order_id.as_str()),
            (
                "allocation",
                first_allocation.allocations[0].allocation_id.as_str(),
            ),
            (
                "allocation",
                first_allocation.allocations[1].allocation_id.as_str(),
            ),
            (
                "allocation",
                second_allocation.allocations[0].allocation_id.as_str(),
            ),
            ("shipment", first_shipment.shipment_id.as_str()),
            ("shipment", second_shipment.shipment_id.as_str()),
            (
                "shipment_line",
                first_shipment.items[0].shipment_line_id.as_str(),
            ),
            (
                "shipment_line",
                first_shipment.items[1].shipment_line_id.as_str(),
            ),
            (
                "shipment_line",
                second_shipment.items[0].shipment_line_id.as_str(),
            ),
            ("delivery", first_delivery.confirmation_id.as_str()),
            ("return", first_return.return_batch_id.as_str()),
        ]
        .into_iter()
        .map(|(entity, id)| (entity.to_owned(), id.to_owned()))
        .collect();
        assert_eq!(imported_identities, expected_identities);

        let inventory_projection: BTreeMap<String, (String, String)> =
            sqlx::query_as::<_, (String, String, String)>(
                "SELECT barcode, inventory_status, quality_status FROM inventory_units WHERE tenant_id = $1",
            )
            .bind(tenant_id)
            .fetch_all(&mut *verification)
            .await
            .expect("load imported inventory projection")
            .into_iter()
            .map(|(barcode, inventory_status, quality_status)| {
                (barcode, (inventory_status, quality_status))
            })
            .collect();
        assert_eq!(inventory_projection.len(), 2);
        assert_eq!(
            inventory_projection.get(&returned_item.barcode),
            Some(&("shipped".to_owned(), "passed".to_owned()))
        );
        assert_eq!(
            inventory_projection.get(&delivered_item.barcode),
            Some(&("delivered".to_owned(), "passed".to_owned()))
        );

        let shipment_line_statuses: BTreeMap<String, String> = sqlx::query_as(
            "SELECT id::text, status FROM outbound_shipment_lines WHERE tenant_id = $1",
        )
        .bind(tenant_id)
        .fetch_all(&mut *verification)
        .await
        .expect("load imported shipment line statuses")
        .into_iter()
        .collect();
        assert_eq!(
            shipment_line_statuses.get(&returned_item.shipment_line_id),
            Some(&"returned".to_owned())
        );
        assert_eq!(
            shipment_line_statuses.get(&delivered_item.shipment_line_id),
            Some(&"delivered".to_owned())
        );
        assert_eq!(
            shipment_line_statuses.get(&second_shipment.items[0].shipment_line_id),
            Some(&"shipped".to_owned())
        );

        let actor_rows: Vec<(String, Uuid, String)> = sqlx::query_as(
            r#"
            SELECT 'receipt', actor_id, source_actor_id FROM inbound_receipts WHERE tenant_id = $1
            UNION ALL SELECT 'inspection', inspector_id, source_actor_id FROM quality_inspections WHERE tenant_id = $1
            UNION ALL SELECT 'order', actor_id, source_actor_id FROM outbound_orders WHERE tenant_id = $1
            UNION ALL SELECT 'allocation', allocated_by, source_actor_id FROM outbound_allocations WHERE tenant_id = $1
            UNION ALL SELECT 'shipment', actor_id, source_actor_id FROM outbound_shipments WHERE tenant_id = $1
            UNION ALL SELECT 'delivery', confirmed_by, source_actor_id FROM delivery_confirmations WHERE tenant_id = $1
            UNION ALL SELECT 'return', actor_id, source_actor_id FROM outbound_return_batches WHERE tenant_id = $1
            UNION ALL SELECT 'movement', actor_id, source_actor_id FROM stock_movements WHERE tenant_id = $1
            UNION ALL SELECT 'audit', actor_id, source_actor_id FROM audit_logs WHERE tenant_id = $1
            "#,
        )
        .bind(tenant_id)
        .fetch_all(&mut *verification)
        .await
        .expect("load imported actor provenance");
        assert!(
            actor_rows
                .iter()
                .all(|(_, actor_id, _)| *actor_id == user_id),
            "every imported target actor must be the authenticated importer"
        );
        let source_actors: BTreeSet<String> = actor_rows
            .into_iter()
            .map(|(_, _, source_actor_id)| source_actor_id)
            .collect();
        assert_eq!(
            source_actors,
            [
                "offline-receiver-a",
                "offline-receiver-b",
                "offline-inspector-a",
                "offline-inspector-b",
                "offline-order-creator-one",
                "offline-allocator-one",
                "offline-shipper-one",
                "offline-delivery-confirmer",
                "offline-return-operator",
                "offline-retest-inspector",
                "offline-order-creator-two",
                "offline-allocator-two",
                "offline-shipper-two",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );

        let immutable_error = sqlx::query(
            "UPDATE inbound_receipts SET source_actor_id = 'tampered' WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(Uuid::parse_str(&receipt_a.receipt_id).expect("receipt UUID"))
        .execute(&mut *verification)
        .await
        .expect_err("source actor provenance must be immutable");
        let sqlstate = immutable_error
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.into_owned());
        assert_eq!(sqlstate.as_deref(), Some("55000"));
        verification
            .rollback()
            .await
            .expect("rollback verification");
        admin.close().await;
    }
}
