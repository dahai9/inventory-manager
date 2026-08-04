//! Consistent, self-verifying backup packages for the offline SQLite edition.
//!
//! The live database uses WAL, so copying only its main file can silently omit
//! committed rows. This module asks SQLite to create the snapshot with
//! `VACUUM INTO`, then publishes the verified database and its metadata as one
//! directory.

use super::sqlite::now_utc;
use super::upgrade::{LOGICAL_SCHEMA_VERSION, PRODUCT_ID};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{ConnectOptions, SqlitePool};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

pub const BACKUP_FORMAT_VERSION: u32 = 1;
pub const DATABASE_FILE_NAME: &str = "database.sqlite3";
pub const METADATA_FILE_NAME: &str = "metadata.json";

const MAX_METADATA_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupMetadata {
    pub product: String,
    pub backup_format_version: u32,
    pub logical_schema_version: i64,
    pub sqlite_migration_version: i64,
    pub source_instance_id: String,
    pub source_workspace_id: String,
    pub exported_at: String,
    pub database_file: String,
    pub database_bytes: u64,
    pub database_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedBackup {
    pub path: PathBuf,
    pub metadata: BackupMetadata,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RestoreOptions {
    /// When set, reject a package created by another offline installation.
    pub expected_source_instance_id: Option<String>,
    /// When set, reject a package created for another offline workspace.
    pub expected_source_workspace_id: Option<String>,
    /// Existing databases are protected by default. Enabling replacement also
    /// requires the target to be closed and creates a pre-restore backup.
    pub replace_existing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoredBackup {
    pub target_path: PathBuf,
    pub metadata: BackupMetadata,
    pub pre_restore_backup: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("invalid backup request: {0}")]
    InvalidRequest(String),
    #[error("backup already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("backup I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("SQLite backup operation '{operation}' failed: {source}")]
    Sqlite {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("backup metadata JSON failed at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("SQLite integrity check failed: {0}")]
    Integrity(String),
    #[error("incompatible backup: {0}")]
    Incompatible(String),
    #[error("backup checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("restore target already exists: {0}")]
    RestoreTargetExists(PathBuf),
    #[error("restore target appears to be open or has active SQLite sidecars: {0}")]
    RestoreTargetBusy(PathBuf),
    #[error("restore identity mismatch for {field}: expected {expected}, got {actual}")]
    IdentityMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error(
        "restore publish failed and rollback also failed; publish: {publish}; rollback: {rollback}; pre-restore backup: {pre_restore_backup}"
    )]
    RestoreRollbackFailed {
        publish: String,
        rollback: String,
        pre_restore_backup: PathBuf,
    },
}

#[derive(Debug)]
struct SnapshotIdentity {
    sqlite_migration_version: i64,
    source_workspace_id: String,
    source_instance_id: String,
}

/// Creates a self-contained backup package from a live SQLite pool.
///
/// `destination` is a new directory. The function never copies the active
/// database file: SQLite itself materializes a transaction-consistent snapshot
/// with `VACUUM INTO`, including committed changes still present only in WAL.
pub async fn create_consistent_backup(
    pool: &SqlitePool,
    destination: &Path,
) -> Result<BackupMetadata, BackupError> {
    validate_destination(destination)?;

    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_dir_all(parent)?;

    let staging = parent.join(format!(".inventory-backup-{}.partial", Uuid::now_v7()));
    create_dir(&staging)?;

    let result = create_in_staging(pool, destination, &staging).await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

async fn create_in_staging(
    pool: &SqlitePool,
    destination: &Path,
    staging: &Path,
) -> Result<BackupMetadata, BackupError> {
    let database_path = staging.join(DATABASE_FILE_NAME);
    let database_path_text = database_path.to_str().ok_or_else(|| {
        BackupError::InvalidRequest(format!(
            "backup path is not valid Unicode: {}",
            database_path.display()
        ))
    })?;

    // VACUUM INTO is the consistency boundary: SQLite materializes a complete
    // snapshot even when committed pages are still in the source WAL.
    let mut connection = pool.acquire().await.map_err(|source| BackupError::Sqlite {
        operation: "acquire source connection",
        source,
    })?;
    sqlx::query("VACUUM INTO ?1")
        .bind(database_path_text)
        .execute(&mut *connection)
        .await
        .map_err(|source| BackupError::Sqlite {
            operation: "VACUUM INTO",
            source,
        })?;
    drop(connection);

    let identity = inspect_snapshot(&database_path).await?;
    if identity.sqlite_migration_version <= 0 {
        return Err(BackupError::Incompatible(format!(
            "SQLite snapshot has no successful migration version: {}",
            identity.sqlite_migration_version
        )));
    }

    let (database_sha256, database_bytes) = sha256_file(&database_path)?;
    let metadata = BackupMetadata {
        product: PRODUCT_ID.to_owned(),
        backup_format_version: BACKUP_FORMAT_VERSION,
        logical_schema_version: LOGICAL_SCHEMA_VERSION,
        sqlite_migration_version: identity.sqlite_migration_version,
        source_instance_id: identity.source_instance_id,
        source_workspace_id: identity.source_workspace_id,
        exported_at: now_utc().map_err(BackupError::InvalidRequest)?,
        database_file: DATABASE_FILE_NAME.to_owned(),
        database_bytes,
        database_sha256,
    };

    let metadata_path = staging.join(METADATA_FILE_NAME);
    let metadata_bytes =
        serde_json::to_vec_pretty(&metadata).map_err(|source| BackupError::Json {
            path: metadata_path.clone(),
            source,
        })?;
    fs::write(&metadata_path, metadata_bytes).map_err(|source| io(&metadata_path, source))?;

    // Verify the package through the same public path used before restore.
    verify_backup_package(staging).await?;
    fs::rename(staging, destination).map_err(|source| io(destination, source))?;

    Ok(metadata)
}

/// Validates package metadata, SHA-256, schema identity and SQLite integrity.
/// It opens the snapshot read-only and never runs migrations or mutates it.
pub async fn verify_backup_package(package_path: &Path) -> Result<VerifiedBackup, BackupError> {
    let metadata_path = package_path.join(METADATA_FILE_NAME);
    let metadata = read_metadata(&metadata_path)?;
    validate_metadata_shape(&metadata)?;

    let database_path = package_path.join(DATABASE_FILE_NAME);
    verify_database_against_metadata(&database_path, &metadata).await?;

    Ok(VerifiedBackup {
        path: package_path.to_path_buf(),
        metadata,
    })
}

/// Restores a verified package to an explicit SQLite database path.
///
/// The application must serialize restore operations with database startup.
/// This function never publishes through an open connection of its own and
/// conservatively rejects existing WAL/SHM/journal sidecars. Existing targets
/// are rejected unless `replace_existing` is explicit; replacements first
/// create a verified pre-restore package in the target directory.
pub async fn restore_backup_to_path(
    package_path: &Path,
    target_path: &Path,
    options: &RestoreOptions,
) -> Result<RestoredBackup, BackupError> {
    let verified = verify_backup_package(package_path).await?;
    validate_expected_identity(&verified.metadata, options)?;
    validate_restore_target_path(package_path, target_path)?;

    let target_exists = target_path.exists();
    if target_exists && !options.replace_existing {
        return Err(BackupError::RestoreTargetExists(target_path.to_path_buf()));
    }

    let parent = target_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_dir_all(parent)?;
    reject_target_inside_package(package_path, parent, target_path)?;

    let restore_id = Uuid::now_v7();
    let staged_path = parent.join(format!(".inventory-restore-{restore_id}.sqlite3.partial"));
    let pre_restore_path = parent.join(format!(".inventory-pre-restore-{restore_id}.invbackup"));

    let result = restore_from_staging(
        package_path,
        target_path,
        &staged_path,
        &pre_restore_path,
        target_exists,
        &verified.metadata,
    )
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&staged_path);
    }
    result
}

async fn restore_from_staging(
    package_path: &Path,
    target_path: &Path,
    staged_path: &Path,
    pre_restore_path: &Path,
    target_exists: bool,
    metadata: &BackupMetadata,
) -> Result<RestoredBackup, BackupError> {
    let package_database = package_path.join(DATABASE_FILE_NAME);
    fs::copy(&package_database, staged_path).map_err(|source| io(staged_path, source))?;
    File::open(staged_path)
        .and_then(|file| file.sync_all())
        .map_err(|source| io(staged_path, source))?;
    verify_database_against_metadata(staged_path, metadata).await?;

    let pre_restore_backup = if target_exists {
        ensure_restore_target_closed(target_path)?;
        let target_pool = open_existing_for_backup(target_path)
            .await
            .map_err(|source| BackupError::Sqlite {
                operation: "open existing restore target",
                source,
            })?;
        let backup_result = create_consistent_backup(&target_pool, pre_restore_path).await;
        target_pool.close().await;

        let existing_metadata = match backup_result {
            Ok(metadata) => metadata,
            Err(error) => return Err(error),
        };
        if let Err(error) = require_same_source_identity(metadata, &existing_metadata) {
            let _ = fs::remove_dir_all(pre_restore_path);
            return Err(error);
        }
        if let Err(error) = ensure_restore_target_closed(target_path) {
            let _ = fs::remove_dir_all(pre_restore_path);
            return Err(error);
        }
        Some(pre_restore_path.to_path_buf())
    } else {
        None
    };

    publish_staged_database(staged_path, target_path, pre_restore_backup.as_deref())?;

    Ok(RestoredBackup {
        target_path: target_path.to_path_buf(),
        metadata: metadata.clone(),
        pre_restore_backup,
    })
}

async fn verify_database_against_metadata(
    database_path: &Path,
    metadata: &BackupMetadata,
) -> Result<(), BackupError> {
    let (actual_sha256, actual_bytes) = sha256_file(database_path)?;
    if actual_bytes != metadata.database_bytes {
        return Err(BackupError::Incompatible(format!(
            "database size mismatch: expected {}, got {}",
            metadata.database_bytes, actual_bytes
        )));
    }
    if actual_sha256 != metadata.database_sha256 {
        return Err(BackupError::ChecksumMismatch {
            expected: metadata.database_sha256.clone(),
            actual: actual_sha256,
        });
    }

    let identity = inspect_snapshot(database_path).await?;
    if identity.sqlite_migration_version != metadata.sqlite_migration_version {
        return Err(BackupError::Incompatible(format!(
            "metadata SQLite migration version {} does not match snapshot version {}",
            metadata.sqlite_migration_version, identity.sqlite_migration_version
        )));
    }
    if identity.source_workspace_id != metadata.source_workspace_id
        || identity.source_instance_id != metadata.source_instance_id
    {
        return Err(BackupError::Incompatible(
            "metadata source identity does not match snapshot".to_owned(),
        ));
    }
    Ok(())
}

/// Runs `PRAGMA integrity_check` against a SQLite snapshot opened read-only.
pub async fn verify_sqlite_integrity(database_path: &Path) -> Result<(), BackupError> {
    let pool = open_read_only(database_path).await.map_err(|source| {
        BackupError::Integrity(format!("cannot open {}: {source}", database_path.display()))
    })?;
    let result = run_integrity_check(&pool).await;
    pool.close().await;
    result
}

async fn inspect_snapshot(database_path: &Path) -> Result<SnapshotIdentity, BackupError> {
    let pool = open_read_only(database_path).await.map_err(|source| {
        BackupError::Integrity(format!("cannot open {}: {source}", database_path.display()))
    })?;

    let result = async {
        run_integrity_check(&pool).await?;

        let sqlite_migration_version: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = TRUE",
        )
        .fetch_one(&pool)
        .await
        .map_err(|source| BackupError::Sqlite {
            operation: "read snapshot migration version",
            source,
        })?;

        let workspaces: Vec<(String, String)> =
            sqlx::query_as("SELECT id, source_instance_id FROM workspaces ORDER BY id")
                .fetch_all(&pool)
                .await
                .map_err(|source| BackupError::Sqlite {
                    operation: "read snapshot source identity",
                    source,
                })?;
        let [(source_workspace_id, source_instance_id)] = workspaces.as_slice() else {
            return Err(BackupError::Incompatible(format!(
                "offline snapshot must contain exactly one workspace, found {}",
                workspaces.len()
            )));
        };

        Ok(SnapshotIdentity {
            sqlite_migration_version,
            source_workspace_id: source_workspace_id.clone(),
            source_instance_id: source_instance_id.clone(),
        })
    }
    .await;

    pool.close().await;
    result
}

async fn open_read_only(database_path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .read_only(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .disable_statement_logging();
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
}

async fn open_existing_for_backup(database_path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .disable_statement_logging();
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
}

async fn run_integrity_check(pool: &SqlitePool) -> Result<(), BackupError> {
    let messages: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(pool)
        .await
        .map_err(|source| BackupError::Integrity(source.to_string()))?;
    if messages.as_slice() == ["ok"] {
        Ok(())
    } else {
        Err(BackupError::Integrity(messages.join("; ")))
    }
}

fn validate_destination(destination: &Path) -> Result<(), BackupError> {
    if destination.as_os_str().is_empty() {
        return Err(BackupError::InvalidRequest(
            "backup destination is empty".to_owned(),
        ));
    }
    if destination.exists() {
        return Err(BackupError::DestinationExists(destination.to_path_buf()));
    }
    Ok(())
}

fn validate_restore_target_path(
    package_path: &Path,
    target_path: &Path,
) -> Result<(), BackupError> {
    if target_path.as_os_str().is_empty() || target_path.file_name().is_none() {
        return Err(BackupError::InvalidRequest(
            "restore target must be an explicit SQLite file path".to_owned(),
        ));
    }
    if target_path == package_path || target_path == package_path.join(DATABASE_FILE_NAME) {
        return Err(BackupError::InvalidRequest(
            "restore target cannot overwrite its source package".to_owned(),
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(target_path) {
        if metadata.file_type().is_symlink() {
            return Err(BackupError::InvalidRequest(
                "restore target cannot be a symbolic link".to_owned(),
            ));
        }
        if !metadata.is_file() {
            return Err(BackupError::InvalidRequest(format!(
                "restore target is not a regular file: {}",
                target_path.display()
            )));
        }
    }
    Ok(())
}

fn reject_target_inside_package(
    package_path: &Path,
    target_parent: &Path,
    target_path: &Path,
) -> Result<(), BackupError> {
    let canonical_package =
        fs::canonicalize(package_path).map_err(|source| io(package_path, source))?;
    let resolved_target = if target_path.exists() {
        fs::canonicalize(target_path).map_err(|source| io(target_path, source))?
    } else {
        let canonical_parent =
            fs::canonicalize(target_parent).map_err(|source| io(target_parent, source))?;
        canonical_parent.join(target_path.file_name().ok_or_else(|| {
            BackupError::InvalidRequest("restore target has no file name".to_owned())
        })?)
    };
    if resolved_target.starts_with(canonical_package) {
        return Err(BackupError::InvalidRequest(
            "restore target cannot be inside its source package".to_owned(),
        ));
    }
    Ok(())
}

fn validate_expected_identity(
    metadata: &BackupMetadata,
    options: &RestoreOptions,
) -> Result<(), BackupError> {
    if let Some(expected) = options.expected_source_instance_id.as_deref() {
        if expected.trim().is_empty() {
            return Err(BackupError::InvalidRequest(
                "expected source instance ID is empty".to_owned(),
            ));
        }
        require_identity("source_instance_id", expected, &metadata.source_instance_id)?;
    }
    if let Some(expected) = options.expected_source_workspace_id.as_deref() {
        if expected.trim().is_empty() {
            return Err(BackupError::InvalidRequest(
                "expected source workspace ID is empty".to_owned(),
            ));
        }
        require_identity(
            "source_workspace_id",
            expected,
            &metadata.source_workspace_id,
        )?;
    }
    Ok(())
}

fn require_same_source_identity(
    expected: &BackupMetadata,
    actual: &BackupMetadata,
) -> Result<(), BackupError> {
    require_identity(
        "target source_instance_id",
        &expected.source_instance_id,
        &actual.source_instance_id,
    )?;
    require_identity(
        "target source_workspace_id",
        &expected.source_workspace_id,
        &actual.source_workspace_id,
    )
}

fn require_identity(field: &'static str, expected: &str, actual: &str) -> Result<(), BackupError> {
    if expected == actual {
        Ok(())
    } else {
        Err(BackupError::IdentityMismatch {
            field,
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        })
    }
}

fn ensure_restore_target_closed(target_path: &Path) -> Result<(), BackupError> {
    if sqlite_sidecar_paths(target_path)
        .iter()
        .any(|path| path.exists())
    {
        return Err(BackupError::RestoreTargetBusy(target_path.to_path_buf()));
    }
    Ok(())
}

fn sqlite_sidecar_paths(database_path: &Path) -> [PathBuf; 3] {
    [
        sqlite_sidecar_path(database_path, "-wal"),
        sqlite_sidecar_path(database_path, "-shm"),
        sqlite_sidecar_path(database_path, "-journal"),
    ]
}

fn sqlite_sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
    let mut path = database_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn publish_staged_database(
    staged_path: &Path,
    target_path: &Path,
    pre_restore_backup: Option<&Path>,
) -> Result<(), BackupError> {
    match (target_path.exists(), pre_restore_backup) {
        (true, None) => {
            return Err(BackupError::RestoreTargetExists(target_path.to_path_buf()));
        }
        (false, Some(_)) => {
            return Err(BackupError::InvalidRequest(
                "restore target disappeared before replacement".to_owned(),
            ));
        }
        _ => {}
    }

    if target_path.exists() {
        ensure_restore_target_closed(target_path)?;
        let pre_restore_backup = pre_restore_backup
            .ok_or_else(|| BackupError::RestoreTargetExists(target_path.to_path_buf()))?;
        publish_replacement(staged_path, target_path, pre_restore_backup)?;
    } else {
        fs::rename(staged_path, target_path).map_err(|source| io(target_path, source))?;
    }
    sync_parent_directory(target_path)?;
    Ok(())
}

#[cfg(unix)]
fn publish_replacement(
    staged_path: &Path,
    target_path: &Path,
    _pre_restore_backup: &Path,
) -> Result<(), BackupError> {
    // POSIX rename replaces an existing regular file atomically.
    fs::rename(staged_path, target_path).map_err(|source| io(target_path, source))
}

#[cfg(not(unix))]
fn publish_replacement(
    staged_path: &Path,
    target_path: &Path,
    pre_restore_backup: &Path,
) -> Result<(), BackupError> {
    // std does not expose replace-existing rename on Windows. Move the closed
    // target aside, publish the staged file, and roll back on any publish error.
    let parent = target_path.parent().unwrap_or_else(|| Path::new("."));
    let rollback_path = parent.join(format!(
        ".inventory-restore-{}.sqlite3.rollback",
        Uuid::now_v7()
    ));
    fs::rename(target_path, &rollback_path).map_err(|source| io(target_path, source))?;
    if let Err(publish) = fs::rename(staged_path, target_path) {
        if let Err(rollback) = fs::rename(&rollback_path, target_path) {
            return Err(BackupError::RestoreRollbackFailed {
                publish: publish.to_string(),
                rollback: rollback.to_string(),
                pre_restore_backup: pre_restore_backup.to_path_buf(),
            });
        }
        return Err(io(target_path, publish));
    }
    let _ = fs::remove_file(rollback_path);
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(target_path: &Path) -> Result<(), BackupError> {
    let parent = target_path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io(parent, source))
}

#[cfg(not(unix))]
fn sync_parent_directory(_target_path: &Path) -> Result<(), BackupError> {
    Ok(())
}

fn validate_metadata_shape(metadata: &BackupMetadata) -> Result<(), BackupError> {
    if metadata.product != PRODUCT_ID {
        return Err(BackupError::Incompatible(format!(
            "backup product '{}' is not '{}'",
            metadata.product, PRODUCT_ID
        )));
    }
    if metadata.backup_format_version != BACKUP_FORMAT_VERSION {
        return Err(BackupError::Incompatible(format!(
            "backup format version {} is not supported",
            metadata.backup_format_version
        )));
    }
    if metadata.logical_schema_version != LOGICAL_SCHEMA_VERSION {
        return Err(BackupError::Incompatible(format!(
            "logical schema version {} is not supported",
            metadata.logical_schema_version
        )));
    }
    if metadata.sqlite_migration_version <= 0 {
        return Err(BackupError::Incompatible(format!(
            "invalid SQLite migration version {}",
            metadata.sqlite_migration_version
        )));
    }
    if metadata.database_file != DATABASE_FILE_NAME {
        return Err(BackupError::Incompatible(format!(
            "unexpected database file '{}'",
            metadata.database_file
        )));
    }
    if metadata.database_sha256.len() != 64
        || !metadata
            .database_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(BackupError::Incompatible(
            "database SHA-256 is not lowercase hexadecimal".to_owned(),
        ));
    }
    if metadata.source_instance_id.trim().is_empty()
        || metadata.source_workspace_id.trim().is_empty()
        || metadata.exported_at.trim().is_empty()
    {
        return Err(BackupError::Incompatible(
            "backup source identity or export timestamp is empty".to_owned(),
        ));
    }
    Ok(())
}

fn read_metadata(path: &Path) -> Result<BackupMetadata, BackupError> {
    let length = fs::metadata(path).map_err(|source| io(path, source))?.len();
    if length > MAX_METADATA_BYTES {
        return Err(BackupError::Incompatible(format!(
            "metadata exceeds {} bytes",
            MAX_METADATA_BYTES
        )));
    }
    let bytes = fs::read(path).map_err(|source| io(path, source))?;
    serde_json::from_slice(&bytes).map_err(|source| BackupError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn sha256_file(path: &Path) -> Result<(String, u64), BackupError> {
    let file = File::open(path).map_err(|source| io(path, source))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| io(path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), bytes))
}

fn create_dir(path: &Path) -> Result<(), BackupError> {
    fs::create_dir(path).map_err(|source| io(path, source))
}

fn create_dir_all(path: &Path) -> Result<(), BackupError> {
    fs::create_dir_all(path).map_err(|source| io(path, source))
}

fn io(path: &Path, source: std::io::Error) -> BackupError {
    BackupError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::OfflineDatabase;
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("库存备份测试-{}", Uuid::now_v7()))
    }

    async fn database_at(root: &Path) -> OfflineDatabase {
        fs::create_dir_all(root).expect("create test root");
        OfflineDatabase::open(&root.join("离线数据.sqlite3"))
            .await
            .expect("open test database")
    }

    async fn set_workspace_name(database: &OfflineDatabase, name: &str) {
        sqlx::query("UPDATE workspaces SET name = ?1 WHERE id = ?2")
            .bind(name)
            .bind(database.workspace_id())
            .execute(database.pool())
            .await
            .expect("update workspace name");
    }

    async fn workspace_name(database_path: &Path, workspace_id: &str) -> String {
        let pool = open_read_only(database_path)
            .await
            .expect("open restored database read-only");
        let name = sqlx::query_scalar("SELECT name FROM workspaces WHERE id = ?1")
            .bind(workspace_id)
            .fetch_one(&pool)
            .await
            .expect("read restored workspace name");
        pool.close().await;
        name
    }

    fn restore_options(metadata: &BackupMetadata, replace_existing: bool) -> RestoreOptions {
        RestoreOptions {
            expected_source_instance_id: Some(metadata.source_instance_id.clone()),
            expected_source_workspace_id: Some(metadata.source_workspace_id.clone()),
            replace_existing,
        }
    }

    #[tokio::test]
    async fn backup_includes_committed_wal_rows_and_verifies_metadata() {
        let root = test_root();
        let database = database_at(&root).await;

        sqlx::query("PRAGMA wal_autocheckpoint = 0")
            .execute(database.pool())
            .await
            .expect("disable automatic WAL checkpoint");
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(database.pool())
            .await
            .expect("start with an empty WAL");
        sqlx::query("UPDATE workspaces SET name = 'WAL 中的工作区' WHERE id = ?1")
            .bind(database.workspace_id())
            .execute(database.pool())
            .await
            .expect("commit a row to WAL");

        let wal_path = root.join("离线数据.sqlite3-wal");
        assert!(
            fs::metadata(&wal_path).expect("WAL exists").len() > 0,
            "test must exercise committed data that has not been checkpointed"
        );

        let package = root.join("每日备份.invbackup");
        let metadata = create_consistent_backup(database.pool(), &package)
            .await
            .expect("create consistent backup");
        let verified = verify_backup_package(&package)
            .await
            .expect("verify backup package");
        verify_sqlite_integrity(&package.join(DATABASE_FILE_NAME))
            .await
            .expect("run public SQLite integrity check");

        assert_eq!(verified.metadata, metadata);
        assert_eq!(metadata.source_workspace_id, database.workspace_id());
        assert_eq!(metadata.database_sha256.len(), 64);
        assert!(!package.join(format!("{DATABASE_FILE_NAME}-wal")).exists());

        let snapshot = open_read_only(&package.join(DATABASE_FILE_NAME))
            .await
            .expect("open snapshot");
        let backed_up_name: String =
            sqlx::query_scalar("SELECT name FROM workspaces WHERE id = ?1")
                .bind(database.workspace_id())
                .fetch_one(&snapshot)
                .await
                .expect("read backed up workspace");
        snapshot.close().await;
        assert_eq!(backed_up_name, "WAL 中的工作区");

        database.pool().close().await;
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[tokio::test]
    async fn verification_rejects_database_checksum_tampering() {
        let root = test_root();
        let database = database_at(&root).await;
        let package = root.join("checksum.invbackup");
        create_consistent_backup(database.pool(), &package)
            .await
            .expect("create backup");

        let database_path = package.join(DATABASE_FILE_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&database_path)
            .expect("open backup for tampering");
        file.seek(SeekFrom::Start(100)).expect("seek into database");
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).expect("read original byte");
        file.seek(SeekFrom::Start(100)).expect("rewind one byte");
        file.write_all(&[byte[0] ^ 0xff]).expect("tamper backup");
        drop(file);

        let error = verify_backup_package(&package)
            .await
            .expect_err("checksum tampering must fail");
        assert!(matches!(error, BackupError::ChecksumMismatch { .. }));

        database.pool().close().await;
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[tokio::test]
    async fn integrity_check_rejects_corruption_even_with_updated_checksum() {
        let root = test_root();
        let database = database_at(&root).await;
        let package = root.join("corrupt.invbackup");
        create_consistent_backup(database.pool(), &package)
            .await
            .expect("create backup");

        let database_path = package.join(DATABASE_FILE_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&database_path)
            .expect("open snapshot for corruption");
        file.seek(SeekFrom::Start(0)).expect("seek to header");
        file.write_all(b"BROKEN DATABASE!")
            .expect("corrupt database header");
        drop(file);

        let metadata_path = package.join(METADATA_FILE_NAME);
        let mut metadata = read_metadata(&metadata_path).expect("read metadata");
        let (digest, bytes) = sha256_file(&database_path).expect("hash corrupt snapshot");
        metadata.database_sha256 = digest;
        metadata.database_bytes = bytes;
        fs::write(
            &metadata_path,
            serde_json::to_vec_pretty(&metadata).expect("serialize metadata"),
        )
        .expect("update metadata checksum");

        let error = verify_backup_package(&package)
            .await
            .expect_err("integrity check must reject corrupted SQLite");
        assert!(matches!(error, BackupError::Integrity(_)));

        database.pool().close().await;
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[tokio::test]
    async fn restore_to_non_ascii_path_verifies_identity_and_contents() {
        let root = test_root();
        let database = database_at(&root).await;
        set_workspace_name(&database, "待恢复工作区").await;
        let package = root.join("可恢复备份.invbackup");
        let metadata = create_consistent_backup(database.pool(), &package)
            .await
            .expect("create restore source");

        let target = root.join("恢复目录").join("恢复后的库存.sqlite3");
        let restored =
            restore_backup_to_path(&package, &target, &restore_options(&metadata, false))
                .await
                .expect("restore to new non-ASCII path");

        assert_eq!(restored.target_path, target);
        assert_eq!(restored.metadata, metadata);
        assert_eq!(restored.pre_restore_backup, None);
        assert_eq!(
            workspace_name(&target, &metadata.source_workspace_id).await,
            "待恢复工作区"
        );

        database.pool().close().await;
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[tokio::test]
    async fn replacing_existing_target_requires_opt_in_and_keeps_pre_restore_backup() {
        let root = test_root();
        let database = database_at(&root).await;
        set_workspace_name(&database, "初始版本").await;
        let initial_package = root.join("initial.invbackup");
        let initial_metadata = create_consistent_backup(database.pool(), &initial_package)
            .await
            .expect("create initial backup");
        let target = root.join("恢复目标.sqlite3");
        restore_backup_to_path(
            &initial_package,
            &target,
            &restore_options(&initial_metadata, false),
        )
        .await
        .expect("seed restore target");

        let target_database = OfflineDatabase::open(&target)
            .await
            .expect("open target for a later local change");
        set_workspace_name(&target_database, "恢复前本地版本").await;
        target_database.pool().close().await;

        let exists_error = restore_backup_to_path(
            &initial_package,
            &target,
            &restore_options(&initial_metadata, false),
        )
        .await
        .expect_err("existing target requires explicit replacement");
        assert!(matches!(exists_error, BackupError::RestoreTargetExists(_)));

        set_workspace_name(&database, "最新备份版本").await;
        let latest_package = root.join("latest.invbackup");
        let latest_metadata = create_consistent_backup(database.pool(), &latest_package)
            .await
            .expect("create latest backup");
        let restored = restore_backup_to_path(
            &latest_package,
            &target,
            &restore_options(&latest_metadata, true),
        )
        .await
        .expect("replace closed target");

        assert_eq!(
            workspace_name(&target, &latest_metadata.source_workspace_id).await,
            "最新备份版本"
        );
        let pre_restore = restored
            .pre_restore_backup
            .expect("replacement keeps pre-restore backup");
        let verified_pre_restore = verify_backup_package(&pre_restore)
            .await
            .expect("pre-restore backup is recoverable");
        assert_eq!(
            workspace_name(
                &pre_restore.join(DATABASE_FILE_NAME),
                &verified_pre_restore.metadata.source_workspace_id,
            )
            .await,
            "恢复前本地版本"
        );

        database.pool().close().await;
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[tokio::test]
    async fn restore_rejects_an_open_wal_target_without_overwriting_it() {
        let root = test_root();
        let database = database_at(&root).await;
        set_workspace_name(&database, "备份内容").await;
        let package = root.join("open-target.invbackup");
        let metadata = create_consistent_backup(database.pool(), &package)
            .await
            .expect("create restore source");
        let target = root.join("打开中的目标.sqlite3");
        restore_backup_to_path(&package, &target, &restore_options(&metadata, false))
            .await
            .expect("seed restore target");

        let open_target = OfflineDatabase::open(&target)
            .await
            .expect("keep target open");
        set_workspace_name(&open_target, "打开时的内容").await;
        assert!(sqlite_sidecar_path(&target, "-shm").exists());

        let error = restore_backup_to_path(&package, &target, &restore_options(&metadata, true))
            .await
            .expect_err("open target must never be replaced");
        assert!(matches!(error, BackupError::RestoreTargetBusy(_)));
        let current_name: String = sqlx::query_scalar("SELECT name FROM workspaces WHERE id = ?1")
            .bind(&metadata.source_workspace_id)
            .fetch_one(open_target.pool())
            .await
            .expect("open database remains usable");
        assert_eq!(current_name, "打开时的内容");

        open_target.pool().close().await;
        database.pool().close().await;
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[tokio::test]
    async fn restore_rejects_corrupt_package_and_cleans_staging_file() {
        let root = test_root();
        let database = database_at(&root).await;
        let package = root.join("restore-corrupt.invbackup");
        let metadata = create_consistent_backup(database.pool(), &package)
            .await
            .expect("create backup");
        let database_path = package.join(DATABASE_FILE_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&database_path)
            .expect("open backup for corruption");
        file.seek(SeekFrom::Start(100)).expect("seek into backup");
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).expect("read byte");
        file.seek(SeekFrom::Start(100)).expect("rewind byte");
        file.write_all(&[byte[0] ^ 0xff]).expect("corrupt byte");
        drop(file);

        let target = root.join("不应创建.sqlite3");
        let error = restore_backup_to_path(&package, &target, &restore_options(&metadata, false))
            .await
            .expect_err("corrupt package must not restore");
        assert!(matches!(error, BackupError::ChecksumMismatch { .. }));
        assert!(!target.exists());
        assert!(!fs::read_dir(&root)
            .expect("list test root")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".inventory-restore-")));

        database.pool().close().await;
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[tokio::test]
    async fn restore_rejects_configured_source_identity_mismatch() {
        let root = test_root();
        let database = database_at(&root).await;
        let package = root.join("identity.invbackup");
        let metadata = create_consistent_backup(database.pool(), &package)
            .await
            .expect("create backup");
        let target = root.join("identity-target.sqlite3");
        let options = RestoreOptions {
            expected_source_instance_id: None,
            expected_source_workspace_id: Some(Uuid::now_v7().to_string()),
            replace_existing: false,
        };

        let error = restore_backup_to_path(&package, &target, &options)
            .await
            .expect_err("workspace mismatch must fail");
        assert!(matches!(
            error,
            BackupError::IdentityMismatch {
                field: "source_workspace_id",
                ..
            }
        ));
        assert!(!target.exists());
        assert_ne!(
            options.expected_source_workspace_id.as_deref(),
            Some(metadata.source_workspace_id.as_str())
        );

        database.pool().close().await;
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[tokio::test]
    async fn replacement_rejects_an_existing_database_from_another_workspace() {
        let root = test_root();
        let source = database_at(&root.join("来源")).await;
        let package = root.join("source.invbackup");
        let metadata = create_consistent_backup(source.pool(), &package)
            .await
            .expect("create source backup");

        let target = root.join("其他工作区.sqlite3");
        let other = OfflineDatabase::open(&target)
            .await
            .expect("create another workspace database");
        let other_workspace_id = other.workspace_id().to_owned();
        set_workspace_name(&other, "其他工作区原数据").await;
        other.pool().close().await;

        let error = restore_backup_to_path(&package, &target, &restore_options(&metadata, true))
            .await
            .expect_err("different target workspace must not be replaced");
        assert!(matches!(
            error,
            BackupError::IdentityMismatch {
                field: "target source_instance_id",
                ..
            }
        ));
        assert_eq!(
            workspace_name(&target, &other_workspace_id).await,
            "其他工作区原数据"
        );
        assert!(!fs::read_dir(&root)
            .expect("list test root")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".inventory-pre-restore-")));

        source.pool().close().await;
        fs::remove_dir_all(root).expect("remove test root");
    }
}
