//! One-time, offline SQLite to network PostgreSQL upgrade packages.
//!
//! Version 1 uses an uncompressed directory whose name ends in `.invpack`.
//! Keeping the package inspectable avoids adding an archive dependency and makes
//! every file independently verifiable. A later archive representation must
//! increment `PACKAGE_FORMAT_VERSION` and retain the same safety properties.
//! Local activation data and network credentials, sessions and entitlements are
//! intentionally outside the exported table allowlist.

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{Column, Row, Sqlite, SqlitePool, Transaction, TypeInfo, ValueRef};
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
