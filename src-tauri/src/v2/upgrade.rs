//! One-time, offline SQLite to network PostgreSQL upgrade packages.
//!
//! Version 1 uses an uncompressed directory whose name ends in `.invpack`.
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
pub const PACKAGE_FORMAT_VERSION: u32 = 1;
pub const LOGICAL_SCHEMA_VERSION: i64 = 1;

const MANIFEST_FILE: &str = "manifest.json";
const CHECKSUMS_FILE: &str = "checksums.json";
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_JSONL_LINE_BYTES: usize = 16 * 1024 * 1024;

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
        let database_schema_version: Option<i64> =
            sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
                .fetch_one(&mut *transaction)
                .await?;
        if database_schema_version != Some(LOGICAL_SCHEMA_VERSION) {
            return Err(UpgradeError::Incompatible(format!(
                "SQLite schema version {:?}, expected {}",
                database_schema_version, LOGICAL_SCHEMA_VERSION
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
    if manifest.package_format_version != PACKAGE_FORMAT_VERSION
        || checksums.package_format_version != PACKAGE_FORMAT_VERSION
    {
        return Err(UpgradeError::Incompatible(format!(
            "package format must be version {PACKAGE_FORMAT_VERSION}"
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
            "version 1 requires the source workspace and an empty, unassigned target workspace"
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
            "outbound_shipment_lines",
            &["workspace_id", "inventory_unit_id"],
        ),
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

    for (table, field, target) in [
        ("party_roles", "party_id", "business_parties"),
        ("locations", "warehouse_id", "warehouses"),
        ("inbound_receipts", "owner_party_id", "business_parties"),
        ("inbound_receipts", "warehouse_id", "warehouses"),
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
    ] {
        validate_foreign_key(records, &ids, table, field, target, false)?;
    }
    for field in ["from_location_id", "to_location_id"] {
        validate_foreign_key(records, &ids, "stock_movements", field, "locations", true)?;
    }

    validate_inbound_quantities(records)?;
    validate_outbound_quantities(records)?;
    validate_inventory_projections(records)?;
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
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

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
    transaction
        .record_imported(claim)
        .await
        .map_err(PostgresImportError::Adapter)?;
    transaction
        .commit()
        .await
        .map_err(PostgresImportError::Adapter)?;
    Ok(ImportOutcome::Imported {
        migration_id: package.manifest.migration_id.clone(),
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
pub struct PgUpgradeAdapter<'a> {
    pool: &'a PgPool,
    tenant_id: Uuid,
    session_token: String,
    required_permission: String,
}

impl<'a> PgUpgradeAdapter<'a> {
    pub fn new(
        database: &'a NetworkDatabase,
        tenant_id: Uuid,
        session_token: impl Into<String>,
        required_permission: impl Into<String>,
    ) -> Self {
        Self {
            pool: database.pool(),
            tenant_id,
            session_token: session_token.into(),
            required_permission: required_permission.into(),
        }
    }

    pub fn from_pool(
        pool: &'a PgPool,
        tenant_id: Uuid,
        session_token: impl Into<String>,
        required_permission: impl Into<String>,
    ) -> Self {
        Self {
            pool,
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

impl<'a> PostgresUpgradeAdapter for PgUpgradeAdapter<'a> {
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
        Ok(())
    }

    async fn record_imported_impl(&mut self, claim: MigrationClaim) -> Result<(), PgUpgradeError> {
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
        sqlx::query(
            r#"
            INSERT INTO migration_packages
                (tenant_id, id, workspace_id, export_id, direction,
                 schema_version, checksum, status, created_at, imported_at,
                 migration_id, source_instance_id, source_workspace_id, actor_id)
            VALUES ($1, $2, $3, $4, 'offline_to_network', $5, $6, 'imported',
                    CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, $7, $8, $9, $10)
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
        .execute(&mut *self.transaction)
        .await?;
        Ok(())
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
                    (tenant_id, id, normalized_name, display_name, created_at)
                VALUES ($1, $2, $3, $4, $5::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "business_parties", "id")?)
            .bind(string_field(record, "business_parties", "normalized_name")?)
            .bind(string_field(record, "business_parties", "display_name")?)
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
                    (tenant_id, id, code, name, tracking_mode, active, created_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz)"#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "skus", "id")?)
            .bind(string_field(record, "skus", "code")?)
            .bind(string_field(record, "skus", "name")?)
            .bind(string_field(record, "skus", "tracking_mode")?)
            .bind(bool_field(record, "skus", "active")?)
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
                     source_reference, received_at, status, actor_id,
                     idempotency_key, request_id, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz, $8, $9,
                        $10, $11, $12::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "inbound_receipts", "id")?)
            .bind(string_field(record, "inbound_receipts", "receipt_no")?)
            .bind(uuid_field(record, "inbound_receipts", "owner_party_id")?)
            .bind(uuid_field(record, "inbound_receipts", "warehouse_id")?)
            .bind(optional_string_field(
                record,
                "inbound_receipts",
                "source_reference",
            )?)
            .bind(string_field(record, "inbound_receipts", "received_at")?)
            .bind(string_field(record, "inbound_receipts", "status")?)
            .bind(actor_id)
            .bind(string_field(record, "inbound_receipts", "idempotency_key")?)
            .bind(string_field(record, "inbound_receipts", "request_id")?)
            .bind(string_field(record, "inbound_receipts", "created_at")?)
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
        for record in records_for(records, "quality_inspections") {
            sqlx::query(
                r#"
                INSERT INTO quality_inspections
                    (tenant_id, id, inspection_no, inspection_type, status,
                     inspector_id, inspected_at, idempotency_key, request_id,
                     created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz, $8, $9,
                        $10::timestamptz)
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
                     defect_code, measurements_json, notes, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::timestamptz)
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
                     authorized_at, revoked_at)
                VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "quality_waivers", "id")?)
            .bind(uuid_field(record, "quality_waivers", "inventory_unit_id")?)
            .bind(string_field(record, "quality_waivers", "reason")?)
            .bind(actor_id)
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
                     status, actor_id, idempotency_key, request_id, created_at)
                VALUES ($1, $2, $3, $4, $5::timestamptz, $6, $7, $8, $9,
                        $10::timestamptz)
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
                     sku_id, status, allocated_by, allocated_at, released_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz,
                        $9::timestamptz)
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
                     shipped_at, actor_id, idempotency_key, request_id, created_at)
                VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7, $8, $9,
                        $10::timestamptz)
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
            .bind(string_field(
                record,
                "outbound_shipments",
                "idempotency_key",
            )?)
            .bind(string_field(record, "outbound_shipments", "request_id")?)
            .bind(string_field(record, "outbound_shipments", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }

        for record in records_for(records, "outbound_shipment_lines") {
            sqlx::query(
                r#"
                INSERT INTO outbound_shipment_lines
                    (tenant_id, id, outbound_shipment_id, outbound_allocation_id,
                     inventory_unit_id, scanned_barcode_snapshot, created_at, status)
                VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz, $8)
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
            // Replay delivery and return rows below so their database
            // triggers perform the same state transitions as live operations.
            .bind("shipped")
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "delivery_confirmations") {
            sqlx::query(
                r#"
                INSERT INTO delivery_confirmations
                    (tenant_id, id, outbound_shipment_id, confirmation_code,
                     confirmed_by, confirmed_at, notes, idempotency_key,
                     request_id, created_at)
                VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7, $8, $9,
                        $10::timestamptz)
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
        for record in records_for(records, "delivery_confirmation_lines") {
            sqlx::query(
                r#"
                INSERT INTO delivery_confirmation_lines
                    (tenant_id, id, delivery_confirmation_id,
                     outbound_shipment_line_id, result, exception_notes, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "delivery_confirmation_lines", "id")?)
            .bind(uuid_field(
                record,
                "delivery_confirmation_lines",
                "delivery_confirmation_id",
            )?)
            .bind(uuid_field(
                record,
                "delivery_confirmation_lines",
                "outbound_shipment_line_id",
            )?)
            .bind(string_field(
                record,
                "delivery_confirmation_lines",
                "result",
            )?)
            .bind(optional_string_field(
                record,
                "delivery_confirmation_lines",
                "exception_notes",
            )?)
            .bind(string_field(
                record,
                "delivery_confirmation_lines",
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
                     idempotency_key, request_id, created_at)
                VALUES ($1, $2, $3, $4::timestamptz, $5, $6, $7, $8::timestamptz)
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
        for record in records_for(records, "outbound_return_lines") {
            sqlx::query(
                r#"
                INSERT INTO outbound_return_lines
                    (tenant_id, id, return_batch_id, outbound_shipment_line_id,
                     inventory_unit_id, reason, disposition, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "outbound_return_lines", "id")?)
            .bind(uuid_field(
                record,
                "outbound_return_lines",
                "return_batch_id",
            )?)
            .bind(uuid_field(
                record,
                "outbound_return_lines",
                "outbound_shipment_line_id",
            )?)
            .bind(uuid_field(
                record,
                "outbound_return_lines",
                "inventory_unit_id",
            )?)
            .bind(string_field(record, "outbound_return_lines", "reason")?)
            .bind(string_field(
                record,
                "outbound_return_lines",
                "disposition",
            )?)
            .bind(string_field(record, "outbound_return_lines", "created_at")?)
            .execute(&mut *self.transaction)
            .await?;
        }
        for record in records_for(records, "stock_movements") {
            sqlx::query(
                r#"
                INSERT INTO stock_movements
                    (tenant_id, id, inventory_unit_id, movement_type,
                     from_location_id, to_location_id, source_type, source_id,
                     actor_id, occurred_at, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::timestamptz,
                        $11::timestamptz)
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
        for record in records_for(records, "audit_logs") {
            sqlx::query(
                r#"
                INSERT INTO audit_logs
                    (tenant_id, id, actor_id, action, entity_type, entity_id,
                     request_id, result, details_json, occurred_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::timestamptz)
                "#,
            )
            .bind(tenant_id)
            .bind(uuid_field(record, "audit_logs", "id")?)
            .bind(actor_id)
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
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
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
    use crate::v2::auth::{hash_token, TokenKind};
    use crate::v2::network::PERMISSION_NETWORK_ACCESS;
    use crate::v2::postgres::NetworkDatabaseConfig;
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
        pool.execute(
            "CREATE TABLE _sqlx_migrations (version BIGINT NOT NULL, success BOOLEAN NOT NULL)",
        )
        .await
        .expect("create migration metadata");
        sqlx::query("INSERT INTO _sqlx_migrations (version, success) VALUES (1, 1)")
            .execute(&pool)
            .await
            .expect("record migration");

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
        assert_eq!(validated.entity_counts.get("audit_logs"), Some(&1));
        assert_eq!(validated.package_checksum, first.package_checksum);
        let audit = String::from_utf8(fs::read(first.path.join("audit.jsonl")).unwrap())
            .expect("audit is UTF-8");
        assert!(!audit.contains("must-not-leak"));
        assert!(audit.contains("[REDACTED]"));
        assert!(audit.contains("kept"));
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
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            let events = Arc::clone(&self.events);
            async move {
                events.lock().expect("event lock").push("record".into());
                Ok(())
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
                migration_id: package.manifest.migration_id.clone()
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
        for (permission_id, code) in [
            (access_permission_id, PERMISSION_NETWORK_ACCESS),
            (upgrade_permission_id, "inventory.upgrade.import"),
        ] {
            sqlx::query("INSERT INTO permissions (tenant_id, id, code, description) VALUES ($1, $2, $3, $4)")
                .bind(tenant_id)
                .bind(permission_id)
                .bind(code)
                .bind(code)
                .execute(&mut *setup)
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

        let source = test_pool().await;
        sqlx::query("UPDATE workspaces SET source_instance_id = ?1 WHERE id = ?2")
            .bind(source_instance_id.to_string())
            .bind(&source.1)
            .execute(&source.0)
            .await
            .expect("set source instance");
        let source_party_id = Uuid::now_v7().to_string();
        let source_sku_id = Uuid::now_v7().to_string();
        let source_warehouse_id = Uuid::now_v7().to_string();
        let source_location_id = Uuid::now_v7().to_string();
        let source_receipt_id = Uuid::now_v7().to_string();
        let source_line_id = Uuid::now_v7().to_string();
        let source_unit_id = Uuid::now_v7().to_string();
        let source_barcode = format!("UPGRADE-SN-{tenant_id}");
        let source_now = "2026-08-03T00:00:00Z";
        sqlx::query("INSERT INTO business_parties (id, workspace_id, normalized_name, display_name, created_at) VALUES (?1, ?2, 'upgrade-owner', 'Upgrade Owner', ?3)")
            .bind(&source_party_id).bind(&source.1).bind(source_now)
            .execute(&source.0).await.expect("insert source party");
        sqlx::query("INSERT INTO party_roles (workspace_id, party_id, role, created_at) VALUES (?1, ?2, 'goods_owner', ?3)")
            .bind(&source.1).bind(&source_party_id).bind(source_now)
            .execute(&source.0).await.expect("insert source party role");
        sqlx::query("INSERT INTO skus (id, workspace_id, code, name, tracking_mode, active, created_at) VALUES (?1, ?2, 'UPGRADE-SKU', 'Upgrade SKU', 'serial', 1, ?3)")
            .bind(&source_sku_id).bind(&source.1).bind(source_now)
            .execute(&source.0).await.expect("insert source sku");
        sqlx::query("INSERT INTO warehouses (id, workspace_id, code, name, created_at) VALUES (?1, ?2, 'UPGRADE-WH', 'Upgrade Warehouse', ?3)")
            .bind(&source_warehouse_id).bind(&source.1).bind(source_now)
            .execute(&source.0).await.expect("insert source warehouse");
        sqlx::query("INSERT INTO locations (id, workspace_id, warehouse_id, code, name, kind, created_at) VALUES (?1, ?2, ?3, 'RECEIVING', 'Receiving', 'receiving', ?4)")
            .bind(&source_location_id).bind(&source.1).bind(&source_warehouse_id).bind(source_now)
            .execute(&source.0).await.expect("insert source location");
        sqlx::query("INSERT INTO inbound_receipts (id, workspace_id, receipt_no, owner_party_id, warehouse_id, source_reference, received_at, status, actor_id, idempotency_key, request_id, created_at) VALUES (?1, ?2, 'UPGRADE-RCPT', ?3, ?4, 'legacy', ?5, 'posted', 'operator', 'upgrade-receipt', 'upgrade-request', ?5)")
            .bind(&source_receipt_id).bind(&source.1).bind(&source_party_id).bind(&source_warehouse_id).bind(source_now)
            .execute(&source.0).await.expect("insert source receipt");
        sqlx::query("INSERT INTO inbound_receipt_lines (id, workspace_id, receipt_id, sku_id, declared_quantity, scanned_quantity, notes, created_at) VALUES (?1, ?2, ?3, ?4, 1, 1, 'legacy', ?5)")
            .bind(&source_line_id).bind(&source.1).bind(&source_receipt_id).bind(&source_sku_id).bind(source_now)
            .execute(&source.0).await.expect("insert source receipt line");
        sqlx::query("INSERT INTO inventory_units (id, workspace_id, barcode, inbound_receipt_line_id, owner_party_id, sku_id, location_id, inventory_status, quality_status, version, received_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'received', 'untested', 1, ?8, ?8)")
            .bind(&source_unit_id).bind(&source.1).bind(&source_barcode).bind(&source_line_id).bind(&source_party_id).bind(&source_sku_id).bind(&source_location_id).bind(source_now)
            .execute(&source.0).await.expect("insert source inventory unit");
        sqlx::query("INSERT INTO stock_movements (id, workspace_id, inventory_unit_id, movement_type, from_location_id, to_location_id, source_type, source_id, actor_id, occurred_at, created_at) VALUES (?1, ?2, ?3, 'received', NULL, ?4, 'inbound_receipt', ?5, 'operator', ?6, ?6)")
            .bind(Uuid::now_v7().to_string()).bind(&source.1).bind(&source_unit_id).bind(&source_location_id).bind(&source_receipt_id).bind(source_now)
            .execute(&source.0).await.expect("insert source stock movement");
        let directory = TestDirectory::new();
        let package_path = directory.path().join("upgrade.invpack");
        let package = OfflineUpgradeExporter::new(&source.0)
            .export(
                &package_path,
                ExportRequest {
                    export_id: Uuid::now_v7().to_string(),
                    workspace_id: source.1.clone(),
                    exported_at: "2026-08-03T00:00:00Z".to_owned(),
                },
            )
            .await
            .expect("export package");
        let validated = validate_package(&package.path).expect("validate package");

        let runtime = NetworkDatabase::connect(&NetworkDatabaseConfig::new(runtime_url))
            .await
            .expect("connect restricted runtime");
        let adapter = PgUpgradeAdapter::new(
            &runtime,
            tenant_id,
            session_token.clone(),
            "inventory.upgrade.import",
        );
        let target = NetworkUpgradeTarget {
            tenant_id: tenant_id.to_string(),
            workspace_id: target_workspace_id.to_string(),
            actor_id: user_id.to_string(),
        };
        let first = import_to_postgres(&adapter, &validated, target.clone())
            .await
            .expect("import package");
        assert!(matches!(first, ImportOutcome::Imported { .. }));
        let replay = import_to_postgres(&adapter, &validated, target)
            .await
            .expect("idempotent replay");
        assert!(matches!(replay, ImportOutcome::AlreadyImported { .. }));

        let mut verification = admin.begin().await.expect("begin verification");
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *verification)
            .await
            .expect("set verification context");
        let counts: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM workspaces WHERE tenant_id = $1), (SELECT count(*) FROM business_parties WHERE tenant_id = $1), (SELECT count(*) FROM inbound_receipts WHERE tenant_id = $1), (SELECT count(*) FROM inbound_receipt_lines WHERE tenant_id = $1), (SELECT count(*) FROM inventory_units WHERE tenant_id = $1), (SELECT count(*) FROM migration_packages WHERE tenant_id = $1)",
        )
        .bind(tenant_id)
        .fetch_one(&mut *verification)
        .await
        .expect("verify imported rows");
        assert_eq!(counts, (1, 1, 1, 1, 1, 1));
        verification
            .rollback()
            .await
            .expect("rollback verification");
        admin.close().await;
    }
}
