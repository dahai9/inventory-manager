//! Two-phase import for shipment/return spreadsheets created by the V1 app.
//!
//! Historical sheets do not prove inbound time, owner, SKU, quality, delivery,
//! or customer semantics. The importer therefore preserves raw rows and writes
//! explicit `unknown` provenance instead of manufacturing those facts.

use super::sqlite::{now_utc, OfflineDatabase};
use calamine::{open_workbook_auto_from_rs, Data, Reader};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Row, Sqlite, Transaction};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};
use uuid::Uuid;

const MAX_LEGACY_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LEGACY_ROWS: usize = 250_000;
const MAX_LEGACY_COLUMNS: usize = 512;
const UNKNOWN_EVENT_TIME: &str = "1970-01-01T00:00:00Z";
const UNKNOWN_OWNER_NORMALIZED: &str = "__legacy_unknown_goods_owner__";
const UNKNOWN_RECEIVER_NORMALIZED: &str = "__legacy_unknown_upstream_receiver__";
const UNKNOWN_SKU_CODE: &str = "__LEGACY_UNKNOWN__";

#[derive(Debug, Error)]
pub enum LegacyImportError {
    #[error("cannot read legacy workbook {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("legacy workbook is invalid: {0}")]
    Workbook(String),
    #[error("legacy column mapping is invalid: {0}")]
    InvalidMapping(String),
    #[error("legacy import request is invalid: {0}")]
    InvalidRequest(String),
    #[error("legacy preview no longer matches the selected workbook or mapping")]
    PreviewChanged,
    #[error("selected rows cannot be imported: {0}")]
    SelectionBlocked(String),
    #[error("legacy import idempotency key was reused with different input: {0}")]
    IdempotencyConflict(String),
    #[error("legacy import storage failed: {0}")]
    Storage(String),
}

pub type LegacyImportResult<T> = Result<T, LegacyImportError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyColumnMapping {
    /// Zero-based worksheet column index.
    pub shipment_barcode: usize,
    pub counterparty_name: Option<usize>,
    pub shipment_time: Option<usize>,
    pub return_barcode: Option<usize>,
    pub return_time: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyWorkbookSheet {
    pub name: String,
    /// One-based Excel row containing the headers.
    pub header_row: u32,
    /// One-based Excel column represented by headers[0].
    pub first_column: u32,
    pub headers: Vec<String>,
    pub data_rows: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyWorkbookInfo {
    pub file_name: String,
    pub file_sha256: String,
    pub file_bytes: u64,
    pub sheets: Vec<LegacyWorkbookSheet>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportPreviewRequest {
    pub source_path: String,
    pub sheet_name: String,
    pub mapping: LegacyColumnMapping,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyIssueSeverity {
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyRowIssue {
    pub severity: LegacyIssueSeverity,
    pub code: String,
    pub field: Option<String>,
    pub message: String,
    #[serde(default)]
    pub conflicting_source_rows: Vec<u32>,
    pub existing_entity_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyPreviewRowStatus {
    Ready,
    Blocked,
    Ignored,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportPreviewRow {
    pub source_row: u32,
    pub raw_values: Vec<String>,
    pub shipment_barcode: Option<String>,
    pub counterparty_raw: Option<String>,
    pub shipment_time_raw: Option<String>,
    pub shipment_time_normalized: Option<String>,
    pub return_barcode: Option<String>,
    pub return_time_raw: Option<String>,
    pub return_time_normalized: Option<String>,
    pub status: LegacyPreviewRowStatus,
    pub issues: Vec<LegacyRowIssue>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportPreviewSummary {
    pub total_rows: u32,
    pub ready_rows: u32,
    pub blocked_rows: u32,
    pub ignored_rows: u32,
    pub warning_rows: u32,
    pub shipment_events: u32,
    pub return_events: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportPreview {
    pub preview_id: String,
    pub file_name: String,
    pub file_sha256: String,
    pub file_bytes: u64,
    pub sheet_name: String,
    pub header_row: u32,
    pub first_column: u32,
    pub headers: Vec<String>,
    pub mapping: LegacyColumnMapping,
    pub summary: LegacyImportPreviewSummary,
    pub assumptions: Vec<String>,
    pub rows: Vec<LegacyImportPreviewRow>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportCommitRequest {
    pub source_path: String,
    pub sheet_name: String,
    pub mapping: LegacyColumnMapping,
    pub preview_id: String,
    /// Source row numbers are one-based Excel row numbers, including header row 1.
    pub selected_source_rows: Vec<u32>,
    pub actor_id: String,
    pub request_id: String,
    pub idempotency_key: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyCommittedRowStatus {
    Imported,
    Skipped,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportCommittedRow {
    pub source_row: u32,
    pub status: LegacyCommittedRowStatus,
    pub issues: Vec<LegacyRowIssue>,
    pub shipment_inventory_unit_id: Option<String>,
    pub outbound_shipment_line_id: Option<String>,
    pub returned_inventory_unit_id: Option<String>,
    pub outbound_return_line_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportCommitReport {
    pub batch_id: String,
    pub preview_id: String,
    pub file_sha256: String,
    pub imported_shipments: u32,
    pub imported_returns: u32,
    pub skipped_rows: u32,
    pub error_rows: u32,
    pub quality_status: String,
    pub source_kind: String,
    pub committed_at: String,
    pub idempotent_replay: bool,
    pub rows: Vec<LegacyImportCommittedRow>,
}

#[derive(Clone)]
struct LoadedFile {
    name: String,
    sha256: String,
    bytes: Arc<[u8]>,
}

#[derive(Clone)]
struct LoadedSheet {
    file: LoadedFile,
    header_row: u32,
    first_column: u32,
    headers: Vec<String>,
    rows: Vec<MappedRow>,
}

#[derive(Clone, Debug)]
struct MappedRow {
    source_row: u32,
    raw_values: Vec<String>,
    shipment_barcode: Option<String>,
    counterparty_raw: Option<String>,
    shipment_time_raw: Option<String>,
    shipment_time_normalized: Option<String>,
    shipment_timestamp: Option<i64>,
    return_barcode: Option<String>,
    return_time_raw: Option<String>,
    return_time_normalized: Option<String>,
    return_timestamp: Option<i64>,
    issues: Vec<LegacyRowIssue>,
}

impl MappedRow {
    fn has_event(&self) -> bool {
        self.shipment_barcode.is_some() || self.return_barcode.is_some()
    }

    fn status(&self) -> LegacyPreviewRowStatus {
        if self
            .issues
            .iter()
            .any(|issue| issue.severity == LegacyIssueSeverity::Error)
        {
            LegacyPreviewRowStatus::Blocked
        } else if !self.has_event() {
            LegacyPreviewRowStatus::Ignored
        } else {
            LegacyPreviewRowStatus::Ready
        }
    }

    fn public(&self) -> LegacyImportPreviewRow {
        LegacyImportPreviewRow {
            source_row: self.source_row,
            raw_values: self.raw_values.clone(),
            shipment_barcode: self.shipment_barcode.clone(),
            counterparty_raw: self.counterparty_raw.clone(),
            shipment_time_raw: self.shipment_time_raw.clone(),
            shipment_time_normalized: self.shipment_time_normalized.clone(),
            return_barcode: self.return_barcode.clone(),
            return_time_raw: self.return_time_raw.clone(),
            return_time_normalized: self.return_time_normalized.clone(),
            status: self.status(),
            issues: self.issues.clone(),
        }
    }
}

#[derive(Clone)]
struct ParsedTime {
    normalized: String,
    unix_timestamp: i64,
}

#[derive(Clone, Default)]
struct GraphLink {
    inventory_unit_id: String,
    outbound_shipment_line_id: String,
    outbound_return_line_id: Option<String>,
}

/// Read workbook metadata without guessing which columns have business meaning.
pub fn inspect_legacy_workbook(path: impl AsRef<Path>) -> LegacyImportResult<LegacyWorkbookInfo> {
    let file = read_file_snapshot(path.as_ref())?;
    let mut workbook = workbook_from_snapshot(&file)?;
    let names = workbook.sheet_names().to_vec();
    if names.is_empty() {
        return Err(LegacyImportError::Workbook("工作簿中没有工作表".to_owned()));
    }
    let mut sheets = Vec::with_capacity(names.len());
    for name in names {
        let range = workbook.worksheet_range(&name).map_err(|error| {
            LegacyImportError::Workbook(format!("无法读取工作表 {name}: {error}"))
        })?;
        let headers: Vec<String> = range
            .rows()
            .next()
            .map(|row| row.iter().map(cell_display).collect())
            .unwrap_or_default();
        if headers.len() > MAX_LEGACY_COLUMNS {
            return Err(LegacyImportError::Workbook(format!(
                "工作表 {name} 超过 {MAX_LEGACY_COLUMNS} 列限制"
            )));
        }
        let data_rows = range.height().saturating_sub(1);
        if data_rows > MAX_LEGACY_ROWS {
            return Err(LegacyImportError::Workbook(format!(
                "工作表 {name} 超过 {MAX_LEGACY_ROWS} 行限制"
            )));
        }
        let (start_row, start_column) = range.start().unwrap_or((0, 0));
        sheets.push(LegacyWorkbookSheet {
            name,
            header_row: start_row.saturating_add(1),
            first_column: start_column.saturating_add(1),
            headers,
            data_rows: u32::try_from(data_rows).unwrap_or(u32::MAX),
        });
    }
    Ok(LegacyWorkbookInfo {
        file_name: file.name,
        file_sha256: file.sha256,
        file_bytes: file.bytes.len() as u64,
        sheets,
    })
}

impl OfflineDatabase {
    pub async fn preview_legacy_excel(
        &self,
        request: LegacyImportPreviewRequest,
    ) -> LegacyImportResult<LegacyImportPreview> {
        let sheet_name = required("sheet_name", request.sheet_name)?;
        let mut loaded = load_mapped_sheet(
            Path::new(request.source_path.trim()),
            &sheet_name,
            &request.mapping,
        )?;
        let preview_id = preview_id(
            self.workspace_id(),
            &loaded.file.sha256,
            &sheet_name,
            &request.mapping,
        )?;
        let existing = load_existing_barcodes_pool(
            self,
            loaded
                .rows
                .iter()
                .filter_map(|row| row.shipment_barcode.as_deref()),
        )
        .await?;
        add_relational_issues(&mut loaded.rows, &existing);
        Ok(build_preview(
            loaded,
            preview_id,
            sheet_name,
            request.mapping,
        ))
    }

    pub async fn commit_legacy_excel(
        &self,
        request: LegacyImportCommitRequest,
    ) -> LegacyImportResult<LegacyImportCommitReport> {
        let request = normalize_commit_request(request)?;
        let request_hash = commit_request_hash(&request)?;
        if let Some(report) =
            load_commit_replay_pool(self, &request.idempotency_key, &request_hash).await?
        {
            return Ok(report);
        }

        let mut loaded = load_mapped_sheet(
            Path::new(&request.source_path),
            &request.sheet_name,
            &request.mapping,
        )?;
        let actual_preview_id = preview_id(
            self.workspace_id(),
            &loaded.file.sha256,
            &request.sheet_name,
            &request.mapping,
        )?;
        if actual_preview_id != request.preview_id {
            return Err(LegacyImportError::PreviewChanged);
        }

        let all_source_rows = loaded
            .rows
            .iter()
            .map(|row| row.source_row)
            .collect::<BTreeSet<_>>();
        let missing_rows = request
            .selected_source_rows
            .difference(&all_source_rows)
            .copied()
            .collect::<Vec<_>>();
        if !missing_rows.is_empty() {
            return Err(LegacyImportError::InvalidRequest(format!(
                "选择的源行不存在: {missing_rows:?}"
            )));
        }

        let selected_base = loaded
            .rows
            .iter()
            .filter(|row| request.selected_source_rows.contains(&row.source_row))
            .cloned()
            .collect::<Vec<_>>();
        let preview_existing = load_existing_barcodes_pool(
            self,
            loaded
                .rows
                .iter()
                .filter_map(|row| row.shipment_barcode.as_deref()),
        )
        .await?;
        add_relational_issues(&mut loaded.rows, &preview_existing);
        let committed_at = now_utc().map_err(|error| storage("read commit time", error))?;
        let batch_id = Uuid::now_v7().to_string();
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| storage("begin legacy import transaction", error))?;

        let writable = sqlx::query(
            "UPDATE workspaces SET read_only = read_only WHERE id = ?1 AND read_only = 0",
        )
        .bind(self.workspace_id())
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage("lock writable workspace", error))?;
        if writable.rows_affected() != 1 {
            return Err(LegacyImportError::SelectionBlocked(
                "离线工作区已归档为只读".to_owned(),
            ));
        }

        if let Some(report) = load_commit_replay_transaction(
            &mut transaction,
            self.workspace_id(),
            &request.idempotency_key,
            &request_hash,
        )
        .await?
        {
            transaction
                .rollback()
                .await
                .map_err(|error| storage("finish legacy import replay", error))?;
            return Ok(report);
        }

        let mut selected = selected_base;
        let existing = load_existing_barcodes_transaction(
            &mut transaction,
            self.workspace_id(),
            selected
                .iter()
                .filter_map(|row| row.shipment_barcode.as_deref()),
        )
        .await?;
        add_relational_issues(&mut selected, &existing);
        let blocked = selected
            .iter()
            .filter(|row| row.status() != LegacyPreviewRowStatus::Ready)
            .map(|row| {
                let codes = row
                    .issues
                    .iter()
                    .filter(|issue| issue.severity == LegacyIssueSeverity::Error)
                    .map(|issue| issue.code.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}[{codes}]", row.source_row)
            })
            .collect::<Vec<_>>();
        if !blocked.is_empty() {
            return Err(LegacyImportError::SelectionBlocked(blocked.join("; ")));
        }
        if !selected.iter().any(|row| row.shipment_barcode.is_some()) {
            return Err(LegacyImportError::SelectionBlocked(
                "至少需要选择一条有效出货记录".to_owned(),
            ));
        }

        let graph = apply_legacy_graph(
            &mut transaction,
            self,
            &batch_id,
            &selected,
            &request,
            &committed_at,
        )
        .await?;
        let rows = build_commit_rows(
            &loaded.rows,
            &selected,
            &request.selected_source_rows,
            &graph,
        );
        let imported_shipments = selected
            .iter()
            .filter(|row| row.shipment_barcode.is_some())
            .count() as u32;
        let imported_returns = selected
            .iter()
            .filter(|row| row.return_barcode.is_some())
            .count() as u32;
        let skipped_rows = rows
            .iter()
            .filter(|row| row.status == LegacyCommittedRowStatus::Skipped)
            .count() as u32;
        let error_rows = rows
            .iter()
            .filter(|row| row.status == LegacyCommittedRowStatus::Error)
            .count() as u32;
        let report = LegacyImportCommitReport {
            batch_id: batch_id.clone(),
            preview_id: request.preview_id.clone(),
            file_sha256: loaded.file.sha256.clone(),
            imported_shipments,
            imported_returns,
            skipped_rows,
            error_rows,
            quality_status: "untested".to_owned(),
            source_kind: "legacy_migration".to_owned(),
            committed_at: committed_at.clone(),
            idempotent_replay: false,
            rows,
        };
        persist_legacy_result(
            &mut transaction,
            self.workspace_id(),
            &batch_id,
            &loaded,
            &request,
            &request_hash,
            &report,
            &committed_at,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|error| storage("commit legacy import transaction", error))?;
        Ok(report)
    }
}

fn normalize_commit_request(
    request: LegacyImportCommitRequest,
) -> LegacyImportResult<NormalizedCommitRequest> {
    let source_path = required("source_path", request.source_path)?;
    let sheet_name = required("sheet_name", request.sheet_name)?;
    let preview_id = required("preview_id", request.preview_id)?;
    if preview_id.len() != 64
        || !preview_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(LegacyImportError::InvalidRequest(
            "preview_id 必须是小写 SHA-256".to_owned(),
        ));
    }
    let actor_id = required("actor_id", request.actor_id)?;
    let request_id = required("request_id", request.request_id)?;
    let idempotency_key = required("idempotency_key", request.idempotency_key)?;
    let selected_source_rows = request
        .selected_source_rows
        .into_iter()
        .collect::<BTreeSet<_>>();
    if selected_source_rows.is_empty() {
        return Err(LegacyImportError::InvalidRequest(
            "selected_source_rows 不能为空".to_owned(),
        ));
    }
    if selected_source_rows.iter().any(|row| *row < 2) {
        return Err(LegacyImportError::InvalidRequest(
            "源行号必须从 2 开始，行 1 是表头".to_owned(),
        ));
    }
    Ok(NormalizedCommitRequest {
        source_path,
        sheet_name,
        mapping: request.mapping,
        preview_id,
        selected_source_rows,
        actor_id,
        request_id,
        idempotency_key,
    })
}

#[derive(Clone)]
struct NormalizedCommitRequest {
    source_path: String,
    sheet_name: String,
    mapping: LegacyColumnMapping,
    preview_id: String,
    selected_source_rows: BTreeSet<u32>,
    actor_id: String,
    request_id: String,
    idempotency_key: String,
}

fn commit_request_hash(request: &NormalizedCommitRequest) -> LegacyImportResult<String> {
    let value = serde_json::json!({
        "preview_id": request.preview_id,
        "sheet_name": request.sheet_name,
        "mapping": request.mapping,
        "selected_source_rows": request.selected_source_rows,
        "actor_id": request.actor_id,
        "request_id": request.request_id,
    });
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| LegacyImportError::InvalidRequest(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

async fn load_commit_replay_pool(
    database: &OfflineDatabase,
    idempotency_key: &str,
    request_hash: &str,
) -> LegacyImportResult<Option<LegacyImportCommitReport>> {
    let row = sqlx::query(
        "SELECT request_hash, response_json FROM legacy_import_batches WHERE workspace_id = ?1 AND idempotency_key = ?2",
    )
    .bind(database.workspace_id())
    .bind(idempotency_key)
    .fetch_optional(database.pool())
    .await
    .map_err(|error| storage("load legacy import replay", error))?;
    decode_replay(row, idempotency_key, request_hash)
}

async fn load_commit_replay_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    idempotency_key: &str,
    request_hash: &str,
) -> LegacyImportResult<Option<LegacyImportCommitReport>> {
    let row = sqlx::query(
        "SELECT request_hash, response_json FROM legacy_import_batches WHERE workspace_id = ?1 AND idempotency_key = ?2",
    )
    .bind(workspace_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| storage("load transactional legacy import replay", error))?;
    decode_replay(row, idempotency_key, request_hash)
}

fn decode_replay(
    row: Option<sqlx::sqlite::SqliteRow>,
    idempotency_key: &str,
    request_hash: &str,
) -> LegacyImportResult<Option<LegacyImportCommitReport>> {
    let Some(row) = row else {
        return Ok(None);
    };
    let stored_hash: String = row.try_get("request_hash")?;
    if stored_hash != request_hash {
        return Err(LegacyImportError::IdempotencyConflict(
            idempotency_key.to_owned(),
        ));
    }
    let response_json: String = row.try_get("response_json")?;
    let mut report: LegacyImportCommitReport = serde_json::from_str(&response_json)
        .map_err(|error| storage("decode legacy import replay", error))?;
    report.idempotent_replay = true;
    Ok(Some(report))
}

async fn load_existing_barcodes_transaction<'a>(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    barcodes: impl Iterator<Item = &'a str>,
) -> LegacyImportResult<HashMap<String, String>> {
    let values = barcodes
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut result = HashMap::new();
    for chunk in values.chunks(400) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT barcode, id FROM inventory_units WHERE workspace_id = ",
        );
        query.push_bind(workspace_id);
        query.push(" AND barcode IN (");
        let mut separated = query.separated(", ");
        for barcode in chunk {
            separated.push_bind(barcode);
        }
        separated.push_unseparated(")");
        let rows = query
            .build()
            .fetch_all(&mut **transaction)
            .await
            .map_err(|error| storage("recheck legacy barcode conflicts", error))?;
        for row in rows {
            result.insert(row.try_get("barcode")?, row.try_get("id")?);
        }
    }
    Ok(result)
}

async fn apply_legacy_graph(
    transaction: &mut Transaction<'_, Sqlite>,
    database: &OfflineDatabase,
    batch_id: &str,
    rows: &[MappedRow],
    request: &NormalizedCommitRequest,
    now: &str,
) -> LegacyImportResult<HashMap<String, GraphLink>> {
    let workspace_id = database.workspace_id();
    let owner_id = ensure_special_party(
        transaction,
        workspace_id,
        UNKNOWN_OWNER_NORMALIZED,
        "未知货主（历史迁移）",
        Some("goods_owner"),
        now,
    )
    .await?;
    let receiver_id = ensure_special_party(
        transaction,
        workspace_id,
        UNKNOWN_RECEIVER_NORMALIZED,
        "未知上游收货方（历史迁移）",
        Some("upstream_receiver"),
        now,
    )
    .await?;
    let sku_id = ensure_unknown_sku(transaction, workspace_id, now).await?;
    let receipt_id = Uuid::now_v7().to_string();
    let receipt_line_id = Uuid::now_v7().to_string();
    let order_id = Uuid::now_v7().to_string();
    let order_line_id = Uuid::now_v7().to_string();
    let shipment_count = rows
        .iter()
        .filter(|row| row.shipment_barcode.is_some())
        .count();
    let quantity = i64::try_from(shipment_count)
        .map_err(|_| LegacyImportError::InvalidRequest("出货数量超出范围".to_owned()))?;
    let short_id = batch_id
        .chars()
        .filter(|value| *value != '-')
        .take(12)
        .collect::<String>();

    sqlx::query(
        r#"
        INSERT INTO inbound_receipts (
            id, workspace_id, receipt_no, owner_party_id, warehouse_id,
            source_reference, received_at, status, actor_id, idempotency_key,
            request_id, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, 'legacy_migration/unknown', ?6,
                  'posted', ?7, ?8, ?9, ?10)
        "#,
    )
    .bind(&receipt_id)
    .bind(workspace_id)
    .bind(format!("LEGACY-IN-{short_id}"))
    .bind(&owner_id)
    .bind(database.warehouse_id())
    .bind(UNKNOWN_EVENT_TIME)
    .bind(&request.actor_id)
    .bind(format!("legacy-inbound-{batch_id}"))
    .bind(&request.request_id)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("insert legacy inbound placeholder", error))?;
    sqlx::query(
        r#"
        INSERT INTO inbound_receipt_lines (
            id, workspace_id, receipt_id, sku_id, declared_quantity,
            scanned_quantity, notes, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?5,
                  'legacy_migration: owner, SKU and received_at are unknown', ?6)
        "#,
    )
    .bind(&receipt_line_id)
    .bind(workspace_id)
    .bind(&receipt_id)
    .bind(&sku_id)
    .bind(quantity)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("insert legacy inbound line", error))?;
    sqlx::query(
        r#"
        INSERT INTO outbound_orders (
            id, workspace_id, order_no, upstream_receiver_id, required_at,
            status, actor_id, idempotency_key, request_id, created_at
        ) VALUES (?1, ?2, ?3, ?4, NULL, 'shipped', ?5, ?6, ?7, ?8)
        "#,
    )
    .bind(&order_id)
    .bind(workspace_id)
    .bind(format!("LEGACY-ORDER-{short_id}"))
    .bind(&receiver_id)
    .bind(&request.actor_id)
    .bind(format!("legacy-order-{batch_id}"))
    .bind(&request.request_id)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("insert legacy outbound order", error))?;
    sqlx::query(
        r#"
        INSERT INTO outbound_order_lines (
            id, workspace_id, outbound_order_id, sku_id, required_quantity,
            allocated_quantity, shipped_quantity, delivered_quantity, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?5, 0, ?6)
        "#,
    )
    .bind(&order_line_id)
    .bind(workspace_id)
    .bind(&order_id)
    .bind(&sku_id)
    .bind(quantity)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("insert legacy outbound order line", error))?;

    let returns = rows
        .iter()
        .filter_map(|row| {
            row.return_barcode
                .as_ref()
                .map(|barcode| (barcode.clone(), row))
        })
        .collect::<HashMap<_, _>>();
    let mut graph = HashMap::new();
    for row in rows.iter().filter(|row| row.shipment_barcode.is_some()) {
        let barcode = row.shipment_barcode.as_ref().expect("filtered shipment");
        let returned = returns.get(barcode).copied();
        let inventory_unit_id = Uuid::now_v7().to_string();
        let allocation_id = Uuid::now_v7().to_string();
        let shipment_id = Uuid::now_v7().to_string();
        let shipment_line_id = Uuid::now_v7().to_string();
        let shipment_time = row
            .shipment_time_normalized
            .as_deref()
            .unwrap_or(UNKNOWN_EVENT_TIME);
        let (inventory_status, location_id) = if returned.is_some() {
            ("quarantined", database.quarantine_location_id())
        } else {
            ("shipped", database.shipping_location_id())
        };

        sqlx::query(
            r#"
            INSERT INTO inventory_units (
                id, workspace_id, barcode, inbound_receipt_line_id, owner_party_id,
                sku_id, location_id, inventory_status, quality_status, version,
                received_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'untested', 1, ?9, ?10)
            "#,
        )
        .bind(&inventory_unit_id)
        .bind(workspace_id)
        .bind(barcode)
        .bind(&receipt_line_id)
        .bind(&owner_id)
        .bind(&sku_id)
        .bind(location_id)
        .bind(inventory_status)
        .bind(UNKNOWN_EVENT_TIME)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(|error| storage("insert legacy inventory unit", error))?;
        sqlx::query(
            r#"
            INSERT INTO outbound_allocations (
                id, workspace_id, outbound_order_line_id, inventory_unit_id,
                status, allocated_by, allocated_at, released_at
            ) VALUES (?1, ?2, ?3, ?4, 'shipped', ?5, ?6, NULL)
            "#,
        )
        .bind(&allocation_id)
        .bind(workspace_id)
        .bind(&order_line_id)
        .bind(&inventory_unit_id)
        .bind(&request.actor_id)
        .bind(shipment_time)
        .execute(&mut **transaction)
        .await
        .map_err(|error| storage("insert legacy allocation", error))?;
        sqlx::query(
            r#"
            INSERT INTO outbound_shipments (
                id, workspace_id, shipment_no, outbound_order_id, status,
                shipped_at, actor_id, idempotency_key, request_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, 'posted', ?5, ?6, ?7, ?8, ?9)
            "#,
        )
        .bind(&shipment_id)
        .bind(workspace_id)
        .bind(format!("LEGACY-SHIP-{short_id}-{}", row.source_row))
        .bind(&order_id)
        .bind(shipment_time)
        .bind(&request.actor_id)
        .bind(format!("legacy-shipment-{batch_id}-{}", row.source_row))
        .bind(&request.request_id)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(|error| storage("insert legacy shipment", error))?;
        sqlx::query(
            r#"
            INSERT INTO outbound_shipment_lines (
                id, workspace_id, outbound_shipment_id, outbound_allocation_id,
                inventory_unit_id, scanned_barcode_snapshot, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
        )
        .bind(&shipment_line_id)
        .bind(workspace_id)
        .bind(&shipment_id)
        .bind(&allocation_id)
        .bind(&inventory_unit_id)
        .bind(barcode)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(|error| storage("insert legacy shipment line", error))?;
        insert_legacy_movement(
            transaction,
            workspace_id,
            &inventory_unit_id,
            "shipped",
            Some(database.shipping_location_id()),
            None,
            &shipment_id,
            &request.actor_id,
            shipment_time,
            now,
        )
        .await?;

        let mut link = GraphLink {
            inventory_unit_id: inventory_unit_id.clone(),
            outbound_shipment_line_id: shipment_line_id.clone(),
            outbound_return_line_id: None,
        };
        if let Some(return_row) = returned {
            let return_batch_id = Uuid::now_v7().to_string();
            let return_line_id = Uuid::now_v7().to_string();
            // The business time remains explicitly unknown in legacy_import_rows.
            // Reusing the known shipment time only keeps the synthetic core graph
            // from sorting an unknown return before its shipment.
            let return_time = return_row
                .return_time_normalized
                .as_deref()
                .unwrap_or(shipment_time);
            sqlx::query(
                r#"
                INSERT INTO outbound_return_batches (
                    id, workspace_id, return_no, returned_at, actor_id,
                    idempotency_key, request_id, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )
            .bind(&return_batch_id)
            .bind(workspace_id)
            .bind(format!(
                "LEGACY-RETURN-{short_id}-{}",
                return_row.source_row
            ))
            .bind(return_time)
            .bind(&request.actor_id)
            .bind(format!(
                "legacy-return-{batch_id}-{}",
                return_row.source_row
            ))
            .bind(&request.request_id)
            .bind(now)
            .execute(&mut **transaction)
            .await
            .map_err(|error| storage("insert legacy return batch", error))?;
            sqlx::query(
                r#"
                INSERT INTO outbound_return_lines (
                    id, workspace_id, return_batch_id, outbound_shipment_line_id,
                    inventory_unit_id, reason, disposition, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5,
                          'legacy_migration/unknown', 'quarantine', ?6)
                "#,
            )
            .bind(&return_line_id)
            .bind(workspace_id)
            .bind(&return_batch_id)
            .bind(&shipment_line_id)
            .bind(&inventory_unit_id)
            .bind(now)
            .execute(&mut **transaction)
            .await
            .map_err(|error| storage("insert legacy return line", error))?;
            insert_legacy_movement(
                transaction,
                workspace_id,
                &inventory_unit_id,
                "returned",
                None,
                Some(database.quarantine_location_id()),
                &return_batch_id,
                &request.actor_id,
                return_time,
                now,
            )
            .await?;
            link.outbound_return_line_id = Some(return_line_id);
        }
        graph.insert(barcode.clone(), link);
    }
    Ok(graph)
}

async fn ensure_special_party(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    normalized_name: &str,
    display_name: &str,
    role: Option<&str>,
    now: &str,
) -> LegacyImportResult<String> {
    sqlx::query(
        "INSERT INTO business_parties (id, workspace_id, normalized_name, display_name, created_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(workspace_id, normalized_name) DO NOTHING",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(workspace_id)
    .bind(normalized_name)
    .bind(display_name)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("ensure legacy placeholder party", error))?;
    let (party_id, stored_display_name): (String, String) = sqlx::query_as(
        "SELECT id, display_name FROM business_parties WHERE workspace_id = ?1 AND normalized_name = ?2",
    )
    .bind(workspace_id)
    .bind(normalized_name)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| storage("load legacy placeholder party", error))?;
    if stored_display_name != display_name {
        return Err(LegacyImportError::SelectionBlocked(format!(
            "保留的历史占位主体 {normalized_name} 已被其他数据占用"
        )));
    }
    if let Some(role) = role {
        sqlx::query(
            "INSERT INTO party_roles (workspace_id, party_id, role, created_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(workspace_id, party_id, role) DO NOTHING",
        )
        .bind(workspace_id)
        .bind(&party_id)
        .bind(role)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(|error| storage("ensure legacy placeholder role", error))?;
    }
    Ok(party_id)
}

async fn ensure_unknown_sku(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    now: &str,
) -> LegacyImportResult<String> {
    sqlx::query(
        "INSERT INTO skus (id, workspace_id, code, name, tracking_mode, active, created_at) VALUES (?1, ?2, ?3, '未知型号（历史迁移）', 'serial', 0, ?4) ON CONFLICT(workspace_id, code) DO NOTHING",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(workspace_id)
    .bind(UNKNOWN_SKU_CODE)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("ensure legacy unknown SKU", error))?;
    let (sku_id, name, tracking_mode, active): (String, String, String, i64) = sqlx::query_as(
        "SELECT id, name, tracking_mode, active FROM skus WHERE workspace_id = ?1 AND code = ?2",
    )
    .bind(workspace_id)
    .bind(UNKNOWN_SKU_CODE)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| storage("load legacy unknown SKU", error))?;
    if name != "未知型号（历史迁移）" || tracking_mode != "serial" || active != 0 {
        return Err(LegacyImportError::SelectionBlocked(format!(
            "保留的历史未知 SKU 编码 {UNKNOWN_SKU_CODE} 已被其他数据占用"
        )));
    }
    Ok(sku_id)
}

#[allow(clippy::too_many_arguments)]
async fn insert_legacy_movement(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    inventory_unit_id: &str,
    movement_type: &str,
    from_location_id: Option<&str>,
    to_location_id: Option<&str>,
    source_id: &str,
    actor_id: &str,
    occurred_at: &str,
    now: &str,
) -> LegacyImportResult<()> {
    sqlx::query(
        r#"
        INSERT INTO stock_movements (
            id, workspace_id, inventory_unit_id, movement_type, from_location_id,
            to_location_id, source_type, source_id, actor_id, occurred_at, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'legacy_migration', ?7, ?8, ?9, ?10)
        "#,
    )
    .bind(Uuid::now_v7().to_string())
    .bind(workspace_id)
    .bind(inventory_unit_id)
    .bind(movement_type)
    .bind(from_location_id)
    .bind(to_location_id)
    .bind(source_id)
    .bind(actor_id)
    .bind(occurred_at)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("insert legacy stock movement", error))?;
    Ok(())
}

fn build_commit_rows(
    preview_rows: &[MappedRow],
    selected_rows: &[MappedRow],
    selected_source_rows: &BTreeSet<u32>,
    graph: &HashMap<String, GraphLink>,
) -> Vec<LegacyImportCommittedRow> {
    let selected = selected_rows
        .iter()
        .map(|row| (row.source_row, row))
        .collect::<HashMap<_, _>>();
    preview_rows
        .iter()
        .map(|preview| {
            let imported = selected_source_rows.contains(&preview.source_row);
            let effective = selected
                .get(&preview.source_row)
                .copied()
                .unwrap_or(preview);
            let shipment = effective
                .shipment_barcode
                .as_ref()
                .and_then(|barcode| graph.get(barcode));
            let returned = effective
                .return_barcode
                .as_ref()
                .and_then(|barcode| graph.get(barcode));
            LegacyImportCommittedRow {
                source_row: preview.source_row,
                status: if imported {
                    LegacyCommittedRowStatus::Imported
                } else if preview.status() == LegacyPreviewRowStatus::Blocked {
                    LegacyCommittedRowStatus::Error
                } else {
                    LegacyCommittedRowStatus::Skipped
                },
                issues: effective.issues.clone(),
                shipment_inventory_unit_id: shipment.map(|link| link.inventory_unit_id.clone()),
                outbound_shipment_line_id: shipment
                    .map(|link| link.outbound_shipment_line_id.clone()),
                returned_inventory_unit_id: returned.map(|link| link.inventory_unit_id.clone()),
                outbound_return_line_id: returned
                    .and_then(|link| link.outbound_return_line_id.clone()),
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn persist_legacy_result(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    batch_id: &str,
    loaded: &LoadedSheet,
    request: &NormalizedCommitRequest,
    request_hash: &str,
    report: &LegacyImportCommitReport,
    now: &str,
) -> LegacyImportResult<()> {
    let mapping_json = serde_json::to_string(&request.mapping)
        .map_err(|error| storage("encode legacy mapping", error))?;
    let selected_rows_json = serde_json::to_string(&request.selected_source_rows)
        .map_err(|error| storage("encode selected legacy rows", error))?;
    let response_json =
        serde_json::to_string(report).map_err(|error| storage("encode legacy report", error))?;
    sqlx::query(
        r#"
        INSERT INTO legacy_import_batches (
            id, workspace_id, source_file_name, source_file_sha256, source_file_bytes,
            sheet_name, preview_id, mapping_json, selected_rows_json, request_hash,
            response_json, status, source_kind, actor_id, request_id, idempotency_key,
            imported_shipments, imported_returns, skipped_rows, error_rows,
            created_at, committed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                  'committed', 'legacy_migration', ?12, ?13, ?14, ?15, ?16,
                  ?17, ?18, ?19, ?19)
        "#,
    )
    .bind(batch_id)
    .bind(workspace_id)
    .bind(&loaded.file.name)
    .bind(&loaded.file.sha256)
    .bind(
        i64::try_from(loaded.file.bytes.len()).map_err(|_| {
            LegacyImportError::InvalidRequest("文件大小超出 SQLite 范围".to_owned())
        })?,
    )
    .bind(&request.sheet_name)
    .bind(&request.preview_id)
    .bind(mapping_json)
    .bind(selected_rows_json)
    .bind(request_hash)
    .bind(response_json)
    .bind(&request.actor_id)
    .bind(&request.request_id)
    .bind(&request.idempotency_key)
    .bind(i64::from(report.imported_shipments))
    .bind(i64::from(report.imported_returns))
    .bind(i64::from(report.skipped_rows))
    .bind(i64::from(report.error_rows))
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("insert legacy import batch", error))?;

    let report_rows = report
        .rows
        .iter()
        .map(|row| (row.source_row, row))
        .collect::<HashMap<_, _>>();
    for row in &loaded.rows {
        let report_row = report_rows
            .get(&row.source_row)
            .ok_or_else(|| storage("persist legacy row", "missing report row"))?;
        let raw_values_json = serde_json::to_string(&row.raw_values)
            .map_err(|error| storage("encode raw legacy row", error))?;
        let issues_json = serde_json::to_string(&report_row.issues)
            .map_err(|error| storage("encode legacy row issues", error))?;
        let row_status = match report_row.status {
            LegacyCommittedRowStatus::Imported => "imported",
            LegacyCommittedRowStatus::Skipped => "skipped",
            LegacyCommittedRowStatus::Error => "error",
        };
        let shipment_time_fact = if row.shipment_barcode.is_none() {
            "not_applicable"
        } else if row.shipment_time_normalized.is_some() {
            "known"
        } else {
            "unknown"
        };
        let return_time_fact = if row.return_barcode.is_none() {
            "not_applicable"
        } else if row.return_time_normalized.is_some() {
            "known"
        } else {
            "unknown"
        };
        sqlx::query(
            r#"
            INSERT INTO legacy_import_rows (
                id, workspace_id, batch_id, source_row, row_status, raw_values_json,
                issues_json, shipment_barcode, return_barcode, counterparty_raw,
                shipment_time_raw, return_time_raw, shipment_time_normalized,
                return_time_normalized, shipment_time_fact, return_time_fact,
                source_kind, received_at_fact, owner_fact, sku_fact, quality_fact,
                quality_status_snapshot, counterparty_semantics,
                shipment_inventory_unit_id, outbound_shipment_line_id,
                returned_inventory_unit_id, outbound_return_line_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                      ?13, ?14, ?15, ?16, 'legacy_migration', 'unknown', 'unknown',
                      'unknown', 'unknown', 'untested', 'unknown', ?17, ?18, ?19, ?20, ?21)
            "#,
        )
        .bind(Uuid::now_v7().to_string())
        .bind(workspace_id)
        .bind(batch_id)
        .bind(i64::from(row.source_row))
        .bind(row_status)
        .bind(raw_values_json)
        .bind(issues_json)
        .bind(&row.shipment_barcode)
        .bind(&row.return_barcode)
        .bind(&row.counterparty_raw)
        .bind(&row.shipment_time_raw)
        .bind(&row.return_time_raw)
        .bind(&row.shipment_time_normalized)
        .bind(&row.return_time_normalized)
        .bind(shipment_time_fact)
        .bind(return_time_fact)
        .bind(&report_row.shipment_inventory_unit_id)
        .bind(&report_row.outbound_shipment_line_id)
        .bind(&report_row.returned_inventory_unit_id)
        .bind(&report_row.outbound_return_line_id)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(|error| storage("insert legacy row report", error))?;
    }
    let details = serde_json::json!({
        "source_kind": "legacy_migration",
        "source_file_sha256": loaded.file.sha256,
        "sheet_name": request.sheet_name,
        "imported_shipments": report.imported_shipments,
        "imported_returns": report.imported_returns,
        "owner_fact": "unknown",
        "sku_fact": "unknown",
        "quality_status": "untested",
        "counterparty_semantics": "unknown",
    });
    sqlx::query(
        r#"
        INSERT INTO audit_logs (
            id, workspace_id, actor_id, action, entity_type, entity_id,
            request_id, result, details_json, occurred_at
        ) VALUES (?1, ?2, ?3, 'legacy_import.committed', 'legacy_import_batch',
                  ?4, ?5, 'success', ?6, ?7)
        "#,
    )
    .bind(Uuid::now_v7().to_string())
    .bind(workspace_id)
    .bind(&request.actor_id)
    .bind(batch_id)
    .bind(&request.request_id)
    .bind(serde_json::to_string(&details).map_err(|error| storage("encode audit", error))?)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(|error| storage("insert legacy import audit", error))?;
    Ok(())
}

fn build_preview(
    loaded: LoadedSheet,
    preview_id: String,
    sheet_name: String,
    mapping: LegacyColumnMapping,
) -> LegacyImportPreview {
    let mut summary = LegacyImportPreviewSummary {
        total_rows: loaded.rows.len() as u32,
        ..LegacyImportPreviewSummary::default()
    };
    for row in &loaded.rows {
        match row.status() {
            LegacyPreviewRowStatus::Ready => summary.ready_rows += 1,
            LegacyPreviewRowStatus::Blocked => summary.blocked_rows += 1,
            LegacyPreviewRowStatus::Ignored => summary.ignored_rows += 1,
        }
        if row
            .issues
            .iter()
            .any(|issue| issue.severity == LegacyIssueSeverity::Warning)
        {
            summary.warning_rows += 1;
        }
        summary.shipment_events += u32::from(row.shipment_barcode.is_some());
        summary.return_events += u32::from(row.return_barcode.is_some());
    }
    LegacyImportPreview {
        preview_id,
        file_name: loaded.file.name,
        file_sha256: loaded.file.sha256,
        file_bytes: loaded.file.bytes.len() as u64,
        sheet_name,
        header_row: loaded.header_row,
        first_column: loaded.first_column,
        headers: loaded.headers,
        mapping,
        summary,
        assumptions: vec![
            "历史表格不能证明货主、SKU、入库时间和客户字段含义；这些事实按 unknown 保存".to_owned(),
            "历史表格没有质检事实；库存单件固定保存为 untested，不生成 passed 或 waived 记录"
                .to_owned(),
            "出货记录只证明已出库，不自动生成交货确认".to_owned(),
        ],
        rows: loaded.rows.iter().map(MappedRow::public).collect(),
    }
}

fn read_file_snapshot(path: &Path) -> LegacyImportResult<LoadedFile> {
    if path.as_os_str().is_empty() {
        return Err(LegacyImportError::InvalidRequest(
            "source_path 不能为空".to_owned(),
        ));
    }
    let metadata = std::fs::metadata(path).map_err(|source| LegacyImportError::Io {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(LegacyImportError::Workbook(
            "选择的路径不是普通文件".to_owned(),
        ));
    }
    if metadata.len() > MAX_LEGACY_FILE_BYTES {
        return Err(LegacyImportError::Workbook(format!(
            "文件超过 {MAX_LEGACY_FILE_BYTES} 字节限制"
        )));
    }
    let bytes = std::fs::read(path).map_err(|source| LegacyImportError::Io {
        path: path.to_owned(),
        source,
    })?;
    if bytes.len() as u64 > MAX_LEGACY_FILE_BYTES {
        return Err(LegacyImportError::Workbook(format!(
            "文件超过 {MAX_LEGACY_FILE_BYTES} 字节限制"
        )));
    }
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "legacy-workbook".to_owned());
    Ok(LoadedFile {
        name,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        bytes: Arc::from(bytes),
    })
}

fn workbook_from_snapshot(
    file: &LoadedFile,
) -> LegacyImportResult<calamine::Sheets<Cursor<Arc<[u8]>>>> {
    open_workbook_auto_from_rs(Cursor::new(Arc::clone(&file.bytes)))
        .map_err(|error| LegacyImportError::Workbook(error.to_string()))
}

fn load_mapped_sheet(
    path: &Path,
    sheet_name: &str,
    mapping: &LegacyColumnMapping,
) -> LegacyImportResult<LoadedSheet> {
    let file = read_file_snapshot(path)?;
    let mut workbook = workbook_from_snapshot(&file)?;
    if !workbook.sheet_names().iter().any(|name| name == sheet_name) {
        return Err(LegacyImportError::Workbook(format!(
            "找不到工作表 {sheet_name}"
        )));
    }
    let range = workbook
        .worksheet_range(sheet_name)
        .map_err(|error| LegacyImportError::Workbook(format!("无法读取工作表: {error}")))?;
    if range.height() == 0 {
        return Err(LegacyImportError::Workbook("工作表为空".to_owned()));
    }
    if range.height().saturating_sub(1) > MAX_LEGACY_ROWS {
        return Err(LegacyImportError::Workbook(format!(
            "工作表超过 {MAX_LEGACY_ROWS} 行限制"
        )));
    }
    if range.width() > MAX_LEGACY_COLUMNS {
        return Err(LegacyImportError::Workbook(format!(
            "工作表超过 {MAX_LEGACY_COLUMNS} 列限制"
        )));
    }
    let (start_row, start_column) = range.start().unwrap_or((0, 0));
    let mut rows = range.rows();
    let headers: Vec<String> = rows
        .next()
        .ok_or_else(|| LegacyImportError::Workbook("工作表为空".to_owned()))?
        .iter()
        .map(cell_display)
        .collect();
    validate_mapping(mapping, &headers)?;
    let mut mapped = Vec::with_capacity(range.height().saturating_sub(1));
    for (index, row) in rows.enumerate() {
        let index = u32::try_from(index)
            .map_err(|_| LegacyImportError::Workbook("源行号超出范围".to_owned()))?;
        let source_row = start_row
            .checked_add(index)
            .and_then(|value| value.checked_add(2))
            .ok_or_else(|| LegacyImportError::Workbook("源行号超出范围".to_owned()))?;
        mapped.push(map_row(source_row, row, &headers, mapping));
    }
    Ok(LoadedSheet {
        file,
        header_row: start_row.saturating_add(1),
        first_column: start_column.saturating_add(1),
        headers,
        rows: mapped,
    })
}

fn validate_mapping(mapping: &LegacyColumnMapping, headers: &[String]) -> LegacyImportResult<()> {
    let mut fields = vec![("shipment_barcode", mapping.shipment_barcode)];
    for (name, value) in [
        ("counterparty_name", mapping.counterparty_name),
        ("shipment_time", mapping.shipment_time),
        ("return_barcode", mapping.return_barcode),
        ("return_time", mapping.return_time),
    ] {
        if let Some(index) = value {
            fields.push((name, index));
        }
    }
    for (name, index) in &fields {
        if *index >= headers.len() {
            return Err(LegacyImportError::InvalidMapping(format!(
                "{name} 列索引 {index} 超出 {} 列范围",
                headers.len()
            )));
        }
    }
    let mut indexes = BTreeMap::<usize, Vec<&str>>::new();
    for (name, index) in fields {
        indexes.entry(index).or_default().push(name);
    }
    if let Some((index, names)) = indexes.iter().find(|(_, names)| names.len() > 1) {
        return Err(LegacyImportError::InvalidMapping(format!(
            "列索引 {index} 被重复映射到 {}",
            names.join(", ")
        )));
    }
    if mapping.return_time.is_some() && mapping.return_barcode.is_none() {
        return Err(LegacyImportError::InvalidMapping(
            "映射退货时间前必须映射退货条码".to_owned(),
        ));
    }
    Ok(())
}

fn map_row(
    source_row: u32,
    row: &[Data],
    headers: &[String],
    mapping: &LegacyColumnMapping,
) -> MappedRow {
    let raw_values = (0..headers.len())
        .map(|index| row.get(index).map(cell_display).unwrap_or_default())
        .collect::<Vec<_>>();
    let mut mapped = MappedRow {
        source_row,
        raw_values,
        shipment_barcode: None,
        counterparty_raw: optional_cell_text(
            row.get(mapping.counterparty_name.unwrap_or(usize::MAX)),
        ),
        shipment_time_raw: None,
        shipment_time_normalized: None,
        shipment_timestamp: None,
        return_barcode: None,
        return_time_raw: None,
        return_time_normalized: None,
        return_timestamp: None,
        issues: Vec::new(),
    };

    mapped.shipment_barcode = barcode_cell(
        row.get(mapping.shipment_barcode),
        "shipment_barcode",
        &mut mapped.issues,
    );
    if let Some(index) = mapping.return_barcode {
        mapped.return_barcode = barcode_cell(row.get(index), "return_barcode", &mut mapped.issues);
    }
    if let Some(index) = mapping.shipment_time {
        let (raw, parsed) = time_cell(
            row.get(index),
            "shipment_time",
            source_row,
            &mut mapped.issues,
        );
        mapped.shipment_time_raw = raw;
        if let Some(parsed) = parsed {
            mapped.shipment_timestamp = Some(parsed.unix_timestamp);
            mapped.shipment_time_normalized = Some(parsed.normalized);
        }
    }
    if let Some(index) = mapping.return_time {
        let (raw, parsed) = time_cell(
            row.get(index),
            "return_time",
            source_row,
            &mut mapped.issues,
        );
        mapped.return_time_raw = raw;
        if let Some(parsed) = parsed {
            mapped.return_timestamp = Some(parsed.unix_timestamp);
            mapped.return_time_normalized = Some(parsed.normalized);
        }
    }

    if mapped.shipment_barcode.is_some() && mapped.shipment_time_normalized.is_none() {
        mapped.issues.push(warning(
            "unknown_shipment_time",
            Some("shipment_time"),
            "缺少可验证的出货时间，将以 unknown 事实保存",
        ));
    }
    if mapped.return_barcode.is_some() && mapped.return_time_normalized.is_none() {
        mapped.issues.push(warning(
            "unknown_return_time",
            Some("return_time"),
            "缺少可验证的退货时间，将以 unknown 事实保存",
        ));
    }
    if mapped.shipment_barcode.is_some() && mapped.counterparty_raw.is_none() {
        mapped.issues.push(warning(
            "unknown_counterparty",
            Some("counterparty_name"),
            "缺少客户字段；上游收货方按 unknown 保存",
        ));
    }
    if mapped.shipment_barcode.is_none() && mapped.shipment_time_raw.is_some() {
        mapped.issues.push(error_issue(
            "shipment_time_without_barcode",
            Some("shipment_time"),
            "存在出货时间但没有出货条码",
        ));
    }
    if mapped.return_barcode.is_none() && mapped.return_time_raw.is_some() {
        mapped.issues.push(error_issue(
            "return_time_without_barcode",
            Some("return_time"),
            "存在退货时间但没有退货条码",
        ));
    }
    mapped
}

fn add_relational_issues(rows: &mut [MappedRow], existing: &HashMap<String, String>) {
    let mut shipments = BTreeMap::<String, Vec<u32>>::new();
    let mut returns = BTreeMap::<String, Vec<u32>>::new();
    let mut shipment_times = HashMap::<String, i64>::new();
    for row in rows.iter() {
        if let Some(barcode) = &row.shipment_barcode {
            shipments
                .entry(barcode.clone())
                .or_default()
                .push(row.source_row);
            if let Some(timestamp) = row.shipment_timestamp {
                shipment_times.entry(barcode.clone()).or_insert(timestamp);
            }
        }
        if let Some(barcode) = &row.return_barcode {
            returns
                .entry(barcode.clone())
                .or_default()
                .push(row.source_row);
        }
    }
    for row in rows.iter_mut() {
        if let Some(barcode) = &row.shipment_barcode {
            if let Some(source_rows) = shipments.get(barcode).filter(|items| items.len() > 1) {
                row.issues.push(conflict_issue(
                    "duplicate_shipment_barcode",
                    "shipment_barcode",
                    format!("出货条码 {barcode} 在当前选择中重复"),
                    source_rows,
                    None,
                ));
            }
            if let Some(entity_id) = existing.get(barcode) {
                row.issues.push(conflict_issue(
                    "existing_inventory_barcode",
                    "shipment_barcode",
                    format!("条码 {barcode} 已存在于当前工作区"),
                    &[],
                    Some(entity_id.clone()),
                ));
            }
        }
        if let Some(barcode) = &row.return_barcode {
            if let Some(source_rows) = returns.get(barcode).filter(|items| items.len() > 1) {
                row.issues.push(conflict_issue(
                    "duplicate_return_barcode",
                    "return_barcode",
                    format!("退货条码 {barcode} 在当前选择中重复"),
                    source_rows,
                    None,
                ));
            }
            match shipments.get(barcode) {
                Some(source_rows) if source_rows.len() == 1 => {
                    if let (Some(shipped_at), Some(returned_at)) =
                        (shipment_times.get(barcode), row.return_timestamp)
                    {
                        if returned_at < *shipped_at {
                            row.issues.push(error_issue(
                                "return_before_shipment",
                                Some("return_time"),
                                "退货时间早于同一条码的出货时间",
                            ));
                        }
                    }
                }
                _ => row.issues.push(error_issue(
                    "return_without_shipment",
                    Some("return_barcode"),
                    "退货条码在当前选择中没有唯一对应的出货记录",
                )),
            }
        }
    }
}

async fn load_existing_barcodes_pool<'a>(
    database: &OfflineDatabase,
    barcodes: impl Iterator<Item = &'a str>,
) -> LegacyImportResult<HashMap<String, String>> {
    let values = barcodes
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut result = HashMap::new();
    for chunk in values.chunks(400) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT barcode, id FROM inventory_units WHERE workspace_id = ",
        );
        query.push_bind(database.workspace_id());
        query.push(" AND barcode IN (");
        let mut separated = query.separated(", ");
        for barcode in chunk {
            separated.push_bind(barcode);
        }
        separated.push_unseparated(")");
        let rows = query
            .build()
            .fetch_all(database.pool())
            .await
            .map_err(|error| storage("check existing legacy barcodes", error))?;
        for row in rows {
            result.insert(row.try_get("barcode")?, row.try_get("id")?);
        }
    }
    Ok(result)
}

fn preview_id(
    workspace_id: &str,
    file_sha256: &str,
    sheet_name: &str,
    mapping: &LegacyColumnMapping,
) -> LegacyImportResult<String> {
    let mapping_json = serde_json::to_vec(mapping)
        .map_err(|error| LegacyImportError::InvalidMapping(error.to_string()))?;
    let mut digest = Sha256::new();
    for value in [
        b"legacy-preview-v1".as_slice(),
        workspace_id.as_bytes(),
        file_sha256.as_bytes(),
        sheet_name.as_bytes(),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    digest.update((mapping_json.len() as u64).to_be_bytes());
    digest.update(mapping_json);
    Ok(format!("{:x}", digest.finalize()))
}

fn barcode_cell(
    cell: Option<&Data>,
    field: &'static str,
    issues: &mut Vec<LegacyRowIssue>,
) -> Option<String> {
    let cell = cell?;
    match cell {
        Data::Empty => None,
        Data::Error(error) => {
            issues.push(error_issue(
                "excel_cell_error",
                Some(field),
                &format!("Excel 单元格错误: {error}"),
            ));
            None
        }
        Data::Int(value) => {
            issues.push(warning(
                "numeric_barcode",
                Some(field),
                "条码来自数值单元格，Excel 可能已经丢失前导零",
            ));
            Some(value.to_string())
        }
        Data::Float(value) => {
            issues.push(warning(
                "numeric_barcode",
                Some(field),
                "条码来自数值单元格，Excel 可能已经丢失前导零或精度",
            ));
            let text = if value.fract() == 0.0 {
                format!("{value:.0}")
            } else {
                value.to_string()
            };
            clean_text(text)
        }
        _ => clean_text(cell_display(cell)),
    }
}

fn time_cell(
    cell: Option<&Data>,
    field: &'static str,
    _source_row: u32,
    issues: &mut Vec<LegacyRowIssue>,
) -> (Option<String>, Option<ParsedTime>) {
    let Some(cell) = cell else {
        return (None, None);
    };
    let raw = clean_text(cell_display(cell));
    let Some(raw_value) = raw.clone() else {
        return (None, None);
    };
    match parse_time_cell(cell) {
        Ok(value) => (raw, Some(value)),
        Err(message) => {
            issues.push(error_issue("invalid_time", Some(field), &message));
            (Some(raw_value), None)
        }
    }
}

fn parse_time_cell(cell: &Data) -> Result<ParsedTime, String> {
    match cell {
        Data::DateTime(value) => {
            if !value.is_datetime() {
                return Err("Excel 时长或时间段不能作为业务时间".to_owned());
            }
            let (year, month, day, hour, minute, second, _) = value.to_ymd_hms_milli();
            parsed_time(year.into(), month, day, hour, minute, second, 8, 0)
        }
        Data::DateTimeIso(value) | Data::String(value) => parse_time_text(value),
        Data::Empty => Err("时间为空".to_owned()),
        _ => parse_time_text(&cell.to_string()),
    }
}

fn parse_time_text(value: &str) -> Result<ParsedTime, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("时间为空".to_owned());
    }
    if !value.is_ascii() {
        return Err(format!("无法识别包含非 ASCII 字符的时间 {value}"));
    }
    let (base, offset_hour, offset_minute) = if let Some(base) = value.strip_suffix('Z') {
        (base, 0, 0)
    } else if value.len() == 25
        && matches!(value.as_bytes().get(19), Some(b'+') | Some(b'-'))
        && value.as_bytes().get(22) == Some(&b':')
    {
        let sign = if value.as_bytes()[19] == b'-' { -1 } else { 1 };
        let hour = parse_u8(&value[20..22], "时区小时")? as i8 * sign;
        let minute = parse_u8(&value[23..25], "时区分钟")? as i8 * sign;
        (&value[..19], hour, minute)
    } else {
        (value, 8, 0)
    };
    let normalized_base = base.replace('T', " ");
    let (date, clock) = match normalized_base.len() {
        10 => (normalized_base.as_str(), "00:00:00"),
        16 => (&normalized_base[..10], &normalized_base[11..]),
        19 => (&normalized_base[..10], &normalized_base[11..]),
        _ => {
            return Err(format!(
                "无法识别时间 {value}，需要 YYYY-MM-DD 或 YYYY-MM-DD HH:MM[:SS]"
            ))
        }
    };
    if normalized_base.len() > 10 && normalized_base.as_bytes().get(10) != Some(&b' ') {
        return Err(format!("无法识别时间 {value}"));
    }
    let year = parse_i32(&date[0..4], "年份")?;
    if date.as_bytes().get(4) != Some(&b'-') || date.as_bytes().get(7) != Some(&b'-') {
        return Err(format!("无法识别日期 {date}"));
    }
    let month = parse_u8(&date[5..7], "月份")?;
    let day = parse_u8(&date[8..10], "日期")?;
    if clock.as_bytes().get(2) != Some(&b':') {
        return Err(format!("无法识别时间 {value}"));
    }
    let hour = parse_u8(&clock[0..2], "小时")?;
    let minute = parse_u8(&clock[3..5], "分钟")?;
    let second = if clock.len() == 8 && clock.as_bytes().get(5) == Some(&b':') {
        parse_u8(&clock[6..8], "秒")?
    } else if clock.len() == 5 {
        0
    } else {
        return Err(format!("无法识别时间 {value}"));
    };
    parsed_time(
        year,
        month,
        day,
        hour,
        minute,
        second,
        offset_hour,
        offset_minute,
    )
}

#[allow(clippy::too_many_arguments)]
fn parsed_time(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    offset_hour: i8,
    offset_minute: i8,
) -> Result<ParsedTime, String> {
    let month = Month::try_from(month).map_err(|error| error.to_string())?;
    let date = Date::from_calendar_date(year, month, day).map_err(|error| error.to_string())?;
    let time = Time::from_hms(hour, minute, second).map_err(|error| error.to_string())?;
    let offset =
        UtcOffset::from_hms(offset_hour, offset_minute, 0).map_err(|error| error.to_string())?;
    let value = PrimitiveDateTime::new(date, time).assume_offset(offset);
    Ok(ParsedTime {
        normalized: value.format(&Rfc3339).map_err(|error| error.to_string())?,
        unix_timestamp: value.unix_timestamp(),
    })
}

fn parse_u8(value: &str, field: &str) -> Result<u8, String> {
    value
        .parse::<u8>()
        .map_err(|_| format!("{field}不是有效数字: {value}"))
}

fn parse_i32(value: &str, field: &str) -> Result<i32, String> {
    value
        .parse::<i32>()
        .map_err(|_| format!("{field}不是有效数字: {value}"))
}

fn cell_display(cell: &Data) -> String {
    match cell {
        Data::DateTime(value) => {
            let (year, month, day, hour, minute, second, _) = value.to_ymd_hms_milli();
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
        }
        Data::Empty => String::new(),
        _ => cell.to_string(),
    }
}

fn optional_cell_text(cell: Option<&Data>) -> Option<String> {
    cell.and_then(|value| clean_text(cell_display(value)))
}

fn clean_text(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn warning(code: &str, field: Option<&str>, message: &str) -> LegacyRowIssue {
    issue(LegacyIssueSeverity::Warning, code, field, message)
}

fn error_issue(code: &str, field: Option<&str>, message: &str) -> LegacyRowIssue {
    issue(LegacyIssueSeverity::Error, code, field, message)
}

fn issue(
    severity: LegacyIssueSeverity,
    code: &str,
    field: Option<&str>,
    message: &str,
) -> LegacyRowIssue {
    LegacyRowIssue {
        severity,
        code: code.to_owned(),
        field: field.map(str::to_owned),
        message: message.to_owned(),
        conflicting_source_rows: Vec::new(),
        existing_entity_id: None,
    }
}

fn conflict_issue(
    code: &str,
    field: &str,
    message: String,
    rows: &[u32],
    existing_entity_id: Option<String>,
) -> LegacyRowIssue {
    LegacyRowIssue {
        severity: LegacyIssueSeverity::Error,
        code: code.to_owned(),
        field: Some(field.to_owned()),
        message,
        conflicting_source_rows: rows.to_vec(),
        existing_entity_id,
    }
}

fn required(field: &str, value: String) -> LegacyImportResult<String> {
    clean_text(value).ok_or_else(|| LegacyImportError::InvalidRequest(format!("{field} 不能为空")))
}

fn storage(context: &str, error: impl std::fmt::Display) -> LegacyImportError {
    LegacyImportError::Storage(format!("{context}: {error}"))
}

impl From<sqlx::Error> for LegacyImportError {
    fn from(error: sqlx::Error) -> Self {
        storage("database operation", error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_xlsxwriter::Workbook;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("inventory-legacy-import-test-{}", Uuid::now_v7()));
            std::fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mapping() -> LegacyColumnMapping {
        LegacyColumnMapping {
            shipment_barcode: 0,
            counterparty_name: Some(1),
            shipment_time: Some(2),
            return_barcode: Some(3),
            return_time: Some(4),
        }
    }

    fn write_workbook(path: &Path, rows: &[[&str; 5]]) {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_worksheet();
        for (column, header) in ["出货条码", "客户", "出货时间", "退货条码", "退货时间"]
            .iter()
            .enumerate()
        {
            sheet
                .write_string(0, column as u16, *header)
                .expect("write header");
        }
        for (row_index, values) in rows.iter().enumerate() {
            for (column, value) in values.iter().enumerate() {
                if !value.is_empty() {
                    sheet
                        .write_string((row_index + 1) as u32, column as u16, *value)
                        .expect("write cell");
                }
            }
        }
        workbook.save(path).expect("save workbook");
    }

    fn preview_request(path: &Path) -> LegacyImportPreviewRequest {
        LegacyImportPreviewRequest {
            source_path: path.to_string_lossy().into_owned(),
            sheet_name: "Sheet1".to_owned(),
            mapping: mapping(),
        }
    }

    fn issue_codes(row: &LegacyImportPreviewRow) -> BTreeSet<&str> {
        row.issues.iter().map(|issue| issue.code.as_str()).collect()
    }

    #[tokio::test]
    async fn preview_reports_duplicate_orphan_and_invalid_rows_without_guessing() {
        let directory = TestDirectory::new();
        let path = directory.path().join("历史记录.xlsx");
        write_workbook(
            &path,
            &[
                ["SN-DUP", "客户 A", "2026-08-01 10:00:00", "", ""],
                ["SN-DUP", "客户 B", "2026-08-01 11:00:00", "", ""],
                ["", "", "", "SN-MISSING", "2026-08-02"],
                ["SN-BAD-TIME", "", "not-a-time", "", ""],
                ["SN-OK", "客户 C", "2026-08-03T12:30:00+08:00", "", ""],
                ["", "", "2026-08-04 08:00:00", "", ""],
            ],
        );
        let info = inspect_legacy_workbook(&path).expect("inspect workbook");
        assert_eq!(info.sheets.len(), 1);
        assert_eq!(info.sheets[0].data_rows, 6);
        assert_eq!(info.file_sha256.len(), 64);

        let database = OfflineDatabase::open(&directory.path().join("preview.sqlite3"))
            .await
            .expect("open database");
        let preview = database
            .preview_legacy_excel(preview_request(&path))
            .await
            .expect("preview workbook");
        assert_eq!(preview.summary.total_rows, 6);
        assert_eq!(preview.summary.blocked_rows, 5);
        assert_eq!(preview.summary.ready_rows, 1);
        assert!(issue_codes(&preview.rows[0]).contains("duplicate_shipment_barcode"));
        assert!(issue_codes(&preview.rows[1]).contains("duplicate_shipment_barcode"));
        assert!(issue_codes(&preview.rows[2]).contains("return_without_shipment"));
        assert!(issue_codes(&preview.rows[3]).contains("invalid_time"));
        assert!(issue_codes(&preview.rows[3]).contains("unknown_counterparty"));
        assert_eq!(
            preview.rows[4].shipment_time_normalized.as_deref(),
            Some("2026-08-03T12:30:00+08:00")
        );
        assert!(issue_codes(&preview.rows[5]).contains("shipment_time_without_barcode"));
        assert_eq!(preview.rows[5].status, LegacyPreviewRowStatus::Blocked);
    }

    #[tokio::test]
    async fn explicit_row_selection_resolves_preview_conflicts() {
        let directory = TestDirectory::new();
        let path = directory.path().join("conflicts.xlsx");
        write_workbook(
            &path,
            &[
                ["SN-ONE", "客户 A", "2026-08-01 10:00:00", "", ""],
                ["SN-ONE", "客户 B", "2026-08-01 11:00:00", "", ""],
                ["", "", "", "SN-ORPHAN", "2026-08-02 10:00:00"],
            ],
        );
        let database = OfflineDatabase::open(&directory.path().join("selection.sqlite3"))
            .await
            .expect("open database");
        let preview = database
            .preview_legacy_excel(preview_request(&path))
            .await
            .expect("preview workbook");
        assert_eq!(preview.summary.blocked_rows, 3);

        let blocked = database
            .commit_legacy_excel(LegacyImportCommitRequest {
                source_path: path.to_string_lossy().into_owned(),
                sheet_name: "Sheet1".to_owned(),
                mapping: mapping(),
                preview_id: preview.preview_id.clone(),
                selected_source_rows: vec![2, 3],
                actor_id: "migration-operator".to_owned(),
                request_id: "legacy-selection-blocked".to_owned(),
                idempotency_key: "legacy-selection-blocked-idem".to_owned(),
            })
            .await
            .expect_err("duplicate selected rows must fail atomically");
        assert!(matches!(blocked, LegacyImportError::SelectionBlocked(_)));
        let count_after_failure: i64 = sqlx::query_scalar("SELECT count(*) FROM inventory_units")
            .fetch_one(database.pool())
            .await
            .expect("count rows after rollback");
        assert_eq!(count_after_failure, 0);
        let batch_after_failure: i64 =
            sqlx::query_scalar("SELECT count(*) FROM legacy_import_batches")
                .fetch_one(database.pool())
                .await
                .expect("count batches after rollback");
        assert_eq!(batch_after_failure, 0);

        let report = database
            .commit_legacy_excel(LegacyImportCommitRequest {
                source_path: path.to_string_lossy().into_owned(),
                sheet_name: "Sheet1".to_owned(),
                mapping: mapping(),
                preview_id: preview.preview_id,
                selected_source_rows: vec![2],
                actor_id: "migration-operator".to_owned(),
                request_id: "legacy-selection-1".to_owned(),
                idempotency_key: "legacy-selection-idem-1".to_owned(),
            })
            .await
            .expect("commit selected row");
        assert_eq!(report.imported_shipments, 1);
        assert_eq!(report.imported_returns, 0);
        assert_eq!(report.error_rows, 2);
        assert_eq!(report.rows[0].status, LegacyCommittedRowStatus::Imported);
        assert_eq!(report.rows[1].status, LegacyCommittedRowStatus::Error);
        let unit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM inventory_units")
            .fetch_one(database.pool())
            .await
            .expect("count units");
        assert_eq!(unit_count, 1);
    }

    #[tokio::test]
    async fn commit_preserves_unknown_provenance_and_never_invents_quality_pass() {
        let directory = TestDirectory::new();
        let path = directory.path().join("commit.xlsx");
        write_workbook(
            &path,
            &[
                ["SN-SHIPPED", "客户 A", "2026-08-01 10:00:00", "", ""],
                ["SN-RETURNED", "客户 B", "", "", ""],
                ["", "", "", "SN-RETURNED", "2026-08-03 09:00:00"],
            ],
        );
        let database = OfflineDatabase::open(&directory.path().join("commit.sqlite3"))
            .await
            .expect("open database");
        let preview = database
            .preview_legacy_excel(preview_request(&path))
            .await
            .expect("preview workbook");
        assert_eq!(preview.summary.blocked_rows, 0);
        let request = LegacyImportCommitRequest {
            source_path: path.to_string_lossy().into_owned(),
            sheet_name: "Sheet1".to_owned(),
            mapping: mapping(),
            preview_id: preview.preview_id.clone(),
            selected_source_rows: vec![2, 3, 4],
            actor_id: "migration-operator".to_owned(),
            request_id: "legacy-commit-1".to_owned(),
            idempotency_key: "legacy-commit-idem-1".to_owned(),
        };
        let report = database
            .commit_legacy_excel(request.clone())
            .await
            .expect("commit workbook");
        assert_eq!(report.imported_shipments, 2);
        assert_eq!(report.imported_returns, 1);
        assert_eq!(report.quality_status, "untested");
        assert_eq!(report.source_kind, "legacy_migration");

        let units: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT barcode, inventory_status, quality_status FROM inventory_units ORDER BY barcode",
        )
        .fetch_all(database.pool())
        .await
        .expect("read imported units");
        assert_eq!(
            units,
            vec![
                (
                    "SN-RETURNED".to_owned(),
                    "quarantined".to_owned(),
                    "untested".to_owned()
                ),
                (
                    "SN-SHIPPED".to_owned(),
                    "shipped".to_owned(),
                    "untested".to_owned()
                ),
            ]
        );
        for table in [
            "quality_inspections",
            "quality_inspection_results",
            "quality_waivers",
            "delivery_confirmations",
        ] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(database.pool())
                .await
                .expect("count invented facts");
            assert_eq!(count, 0, "{table} must remain empty");
        }
        let source_reference: String =
            sqlx::query_scalar("SELECT source_reference FROM inbound_receipts")
                .fetch_one(database.pool())
                .await
                .expect("read legacy source reference");
        assert_eq!(source_reference, "legacy_migration/unknown");
        let provenance: Vec<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT received_at_fact, owner_fact, sku_fact, quality_fact, quality_status_snapshot FROM legacy_import_rows ORDER BY source_row",
        )
        .fetch_all(database.pool())
        .await
        .expect("read provenance");
        assert!(provenance.iter().all(|row| row
            == &(
                "unknown".to_owned(),
                "unknown".to_owned(),
                "unknown".to_owned(),
                "unknown".to_owned(),
                "untested".to_owned()
            )));
        let customer_party_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM business_parties WHERE display_name IN ('客户 A', '客户 B')",
        )
        .fetch_one(database.pool())
        .await
        .expect("check ambiguous customer was not promoted");
        assert_eq!(customer_party_count, 0);
        let unknown_time_fact: String = sqlx::query_scalar(
            "SELECT shipment_time_fact FROM legacy_import_rows WHERE shipment_barcode = 'SN-RETURNED'",
        )
        .fetch_one(database.pool())
        .await
        .expect("read unknown time fact");
        assert_eq!(unknown_time_fact, "unknown");
        let movement_sources: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT source_type FROM stock_movements")
                .fetch_all(database.pool())
                .await
                .expect("read movement provenance");
        assert_eq!(movement_sources, vec!["legacy_migration".to_owned()]);

        let conflict_path = directory.path().join("cross-file-conflict.xlsx");
        write_workbook(
            &conflict_path,
            &[["SN-SHIPPED", "另一个客户", "2026-08-05 10:00:00", "", ""]],
        );
        let conflict_preview = database
            .preview_legacy_excel(preview_request(&conflict_path))
            .await
            .expect("preview cross-file conflict");
        assert_eq!(conflict_preview.summary.blocked_rows, 1);
        assert!(issue_codes(&conflict_preview.rows[0]).contains("existing_inventory_barcode"));

        let replay = database
            .commit_legacy_excel(request)
            .await
            .expect("replay import");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.batch_id, report.batch_id);
        let batch_count: i64 = sqlx::query_scalar("SELECT count(*) FROM legacy_import_batches")
            .fetch_one(database.pool())
            .await
            .expect("count batches");
        assert_eq!(batch_count, 1);
        let unit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM inventory_units")
            .fetch_one(database.pool())
            .await
            .expect("count units");
        assert_eq!(unit_count, 2);
    }
}
