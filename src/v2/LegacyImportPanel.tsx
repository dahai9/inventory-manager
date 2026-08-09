import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import {
  AlertTriangle,
  CheckCircle2,
  FileSpreadsheet,
  LoaderCircle,
  RefreshCw,
} from "lucide-react";

type Notice = { type: "success" | "error"; text: string };
type RowStatus = "ready" | "blocked" | "ignored";

interface LegacyColumnMapping {
  shipment_barcode: number;
  counterparty_name: number | null;
  shipment_time: number | null;
  return_barcode: number | null;
  return_time: number | null;
}

interface LegacyWorkbookSheet {
  name: string;
  headers: string[];
  data_rows: number;
}

interface LegacyWorkbookInfo {
  file_name: string;
  file_sha256: string;
  file_bytes: number;
  sheets: LegacyWorkbookSheet[];
}

interface LegacyRowIssue {
  severity: "warning" | "error";
  code: string;
  field: string | null;
  message: string;
  conflicting_source_rows: number[];
  existing_entity_id: string | null;
}

interface LegacyPreviewRow {
  source_row: number;
  raw_values: string[];
  shipment_barcode: string | null;
  counterparty_raw: string | null;
  shipment_time_raw: string | null;
  shipment_time_normalized: string | null;
  return_barcode: string | null;
  return_time_raw: string | null;
  return_time_normalized: string | null;
  status: RowStatus;
  issues: LegacyRowIssue[];
}

interface LegacyImportPreview {
  preview_id: string;
  file_name: string;
  file_sha256: string;
  file_bytes: number;
  sheet_name: string;
  headers: string[];
  mapping: LegacyColumnMapping;
  summary: {
    total_rows: number;
    ready_rows: number;
    blocked_rows: number;
    ignored_rows: number;
    warning_rows: number;
    shipment_events: number;
    return_events: number;
  };
  assumptions: string[];
  rows: LegacyPreviewRow[];
}

interface LegacyImportCommitReport {
  batch_id: string;
  imported_shipments: number;
  imported_returns: number;
  skipped_rows: number;
  error_rows: number;
  quality_status: string;
  source_kind: string;
  committed_at: string;
  idempotent_replay: boolean;
}

interface LegacyImportPanelProps {
  actorId: string;
  activated: boolean;
  onCommitted?: () => void;
  onBusyChange?: (busy: boolean) => void;
}

const emptyMapping: LegacyColumnMapping = {
  shipment_barcode: 0,
  counterparty_name: null,
  shipment_time: null,
  return_barcode: null,
  return_time: null,
};

function displayError(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  if (typeof error === "object" && error !== null) {
    const message = (error as { message?: unknown }).message;
    if (typeof message === "string") return message;
  }
  return String(error);
}

function createId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function optionalColumn(value: string): number | null {
  return value === "" ? null : Number(value);
}

function autoMapping(headers: string[]): LegacyColumnMapping {
  const normalized = headers.map((header) => header.trim().toLowerCase());
  const find = (...names: string[]) => {
    const index = normalized.findIndex((header) => names.some((name) => header.includes(name)));
    return index >= 0 ? index : null;
  };
  return {
    shipment_barcode: find("出货条码", "出货编码", "shipment", "barcode") ?? 0,
    counterparty_name: find("客户", "收货方", "customer", "receiver"),
    shipment_time: find("出货时间", "shipment time", "shipped at"),
    return_barcode: find("退货条码", "退货编码", "return barcode"),
    return_time: find("退货时间", "return time", "returned at"),
  };
}

export default function LegacyImportPanel({ actorId, activated, onCommitted, onBusyChange }: LegacyImportPanelProps) {
  const [sourcePath, setSourcePath] = useState("");
  const [workbook, setWorkbook] = useState<LegacyWorkbookInfo | null>(null);
  const [sheetName, setSheetName] = useState("");
  const [mapping, setMapping] = useState<LegacyColumnMapping>(emptyMapping);
  const [preview, setPreview] = useState<LegacyImportPreview | null>(null);
  const [selectedRows, setSelectedRows] = useState<Set<number>>(() => new Set());
  const [report, setReport] = useState<LegacyImportCommitReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const busyCountRef = useRef(0);
  const busyGenerationRef = useRef(0);
  const onBusyChangeRef = useRef(onBusyChange);
  onBusyChangeRef.current = onBusyChange;

  const beginBusy = useCallback(() => {
    const generation = busyGenerationRef.current;
    busyCountRef.current += 1;
    if (busyCountRef.current === 1) onBusyChangeRef.current?.(true);

    let ended = false;
    return () => {
      if (ended || generation !== busyGenerationRef.current) return;
      ended = true;
      busyCountRef.current = Math.max(0, busyCountRef.current - 1);
      if (busyCountRef.current === 0) onBusyChangeRef.current?.(false);
    };
  }, []);

  useEffect(() => () => {
    busyGenerationRef.current += 1;
    busyCountRef.current = 0;
    onBusyChangeRef.current?.(false);
  }, []);

  const sheet = useMemo(
    () => workbook?.sheets.find((candidate) => candidate.name === sheetName) ?? null,
    [sheetName, workbook],
  );
  const readyRows = useMemo(
    () => preview?.rows.filter((row) => row.status === "ready") ?? [],
    [preview],
  );

  function resetPreview() {
    setPreview(null);
    setSelectedRows(new Set());
    setReport(null);
  }

  async function chooseWorkbook() {
    const endBusy = beginBusy();
    try {
      const selected = await open({
        title: "选择历史 Excel 文件",
        directory: false,
        multiple: false,
        filters: [{ name: "电子表格", extensions: ["xlsx", "xls", "xlsb", "xlsm", "ods"] }],
      });
      if (!selected || Array.isArray(selected)) return;
      setLoading(true);
      setNotice(null);
      resetPreview();
      try {
        const info = await invoke<LegacyWorkbookInfo>("v2_inspect_legacy_workbook", { path: selected });
        const firstSheet = info.sheets[0];
        if (!firstSheet) throw new Error("工作簿中没有可导入的工作表");
        setSourcePath(selected);
        setWorkbook(info);
        setSheetName(firstSheet.name);
        setMapping(autoMapping(firstSheet.headers));
      } catch (error) {
        setSourcePath("");
        setWorkbook(null);
        setSheetName("");
        setNotice({ type: "error", text: `读取工作簿失败：${displayError(error)}` });
      } finally {
        setLoading(false);
      }
    } finally {
      endBusy();
    }
  }

  function selectSheet(nextSheetName: string) {
    const nextSheet = workbook?.sheets.find((candidate) => candidate.name === nextSheetName);
    setSheetName(nextSheetName);
    setMapping(nextSheet ? autoMapping(nextSheet.headers) : emptyMapping);
    setNotice(null);
    resetPreview();
  }

  function updateMapping<Key extends keyof LegacyColumnMapping>(key: Key, value: LegacyColumnMapping[Key]) {
    setMapping((current) => ({ ...current, [key]: value }));
    setNotice(null);
    resetPreview();
  }

  async function buildPreview() {
    if (!sourcePath || !sheetName) return;
    const endBusy = beginBusy();
    setLoading(true);
    setNotice(null);
    setReport(null);
    try {
      const output = await invoke<LegacyImportPreview>("v2_preview_legacy_excel", {
        input: { source_path: sourcePath, sheet_name: sheetName, mapping },
      });
      setPreview(output);
      setSelectedRows(new Set(output.rows.filter((row) => row.status === "ready").map((row) => row.source_row)));
      if (output.summary.blocked_rows > 0) {
        setNotice({ type: "error", text: `${output.summary.blocked_rows} 行存在冲突或无效数据，已从提交范围排除。` });
      }
    } catch (error) {
      setPreview(null);
      setSelectedRows(new Set());
      setNotice({ type: "error", text: `生成预览失败：${displayError(error)}` });
    } finally {
      setLoading(false);
      endBusy();
    }
  }

  function toggleRow(sourceRow: number) {
    setSelectedRows((current) => {
      const next = new Set(current);
      if (next.has(sourceRow)) next.delete(sourceRow);
      else next.add(sourceRow);
      return next;
    });
  }

  function toggleAllReady() {
    setSelectedRows((current) => {
      const allSelected = readyRows.length > 0 && readyRows.every((row) => current.has(row.source_row));
      return allSelected ? new Set() : new Set(readyRows.map((row) => row.source_row));
    });
  }

  async function commitImport() {
    if (!preview || !sourcePath || selectedRows.size === 0) return;
    if (!activated) {
      setNotice({ type: "error", text: "离线授权无效，当前仅允许查询、备份和升级导出。" });
      return;
    }
    const endBusy = beginBusy();
    setLoading(true);
    setNotice(null);
    try {
      const operationId = createId();
      const output = await invoke<LegacyImportCommitReport>("v2_commit_legacy_excel", {
        input: {
          source_path: sourcePath,
          sheet_name: preview.sheet_name,
          mapping: preview.mapping,
          preview_id: preview.preview_id,
          selected_source_rows: Array.from(selectedRows).sort((left, right) => left - right),
          actor_id: actorId,
          request_id: operationId,
          idempotency_key: `legacy-excel:${operationId}`,
        },
      });
      setReport(output);
      setNotice({
        type: "success",
        text: `已导入 ${output.imported_shipments} 条历史出货和 ${output.imported_returns} 条历史退回；质检事实保持未知。`,
      });
      onCommitted?.();
    } catch (error) {
      setNotice({ type: "error", text: `提交导入失败：${displayError(error)}` });
    } finally {
      setLoading(false);
      endBusy();
    }
  }

  return (
    <section className="v2-page" aria-labelledby="v2-legacy-import-title">
      <div className="v2-page-heading">
        <div>
          <span className="v2-eyebrow">历史数据迁移</span>
          <h2 id="v2-legacy-import-title">Excel 导入</h2>
          <p>历史记录进入 SQLite 事实库，原文件摘要和逐行处理结果永久留存。</p>
        </div>
        <button className="v2-button" type="button" onClick={() => void chooseWorkbook()} disabled={loading}>
          <FileSpreadsheet size={16} /> 选择文件
        </button>
      </div>

      {notice && <div className={`v2-notice ${notice.type}`}>{notice.text}</div>}
      {!activated && <div className="v2-notice error">离线授权无效，Excel 业务数据导入已锁定。</div>}

      {workbook && sheet && (
        <div className="v2-import-layout">
          <section className="v2-panel v2-import-mapping">
            <div className="v2-settings-heading">
              <div><h3>{workbook.file_name}</h3><small>{(workbook.file_bytes / 1024).toFixed(1)} KiB · SHA-256 {workbook.file_sha256.slice(0, 12)}…</small></div>
              <FileSpreadsheet size={20} />
            </div>
            <label><span>工作表</span><select value={sheetName} onChange={(event) => selectSheet(event.target.value)}>{workbook.sheets.map((item) => <option key={item.name} value={item.name}>{item.name}（{item.data_rows} 行）</option>)}</select></label>
            <div className="v2-import-map-grid">
              <ColumnSelect label="出货条码 *" headers={sheet.headers} value={mapping.shipment_barcode} required onChange={(value) => updateMapping("shipment_barcode", value ?? 0)} />
              <ColumnSelect label="客户 / 收货方" headers={sheet.headers} value={mapping.counterparty_name} onChange={(value) => updateMapping("counterparty_name", value)} />
              <ColumnSelect label="出货时间" headers={sheet.headers} value={mapping.shipment_time} onChange={(value) => updateMapping("shipment_time", value)} />
              <ColumnSelect label="退货条码" headers={sheet.headers} value={mapping.return_barcode} onChange={(value) => updateMapping("return_barcode", value)} />
              <ColumnSelect label="退货时间" headers={sheet.headers} value={mapping.return_time} onChange={(value) => updateMapping("return_time", value)} />
            </div>
            <button className="v2-button primary wide" type="button" onClick={() => void buildPreview()} disabled={loading}>
              {loading ? <LoaderCircle className="v2-spin" size={16} /> : <RefreshCw size={16} />} 生成逐行预览
            </button>
          </section>

          <section className="v2-panel v2-import-facts">
            <AlertTriangle size={20} />
            <div><strong>历史事实边界</strong><span>货主、型号、入库时间和客户字段含义保存为 unknown；质检状态保存为 untested，不生成合格或放行记录。</span></div>
          </section>
        </div>
      )}

      {preview && (
        <section className="v2-panel v2-import-preview">
          <div className="v2-import-summary">
            <span>总行数 <strong>{preview.summary.total_rows}</strong></span>
            <span>可导入 <strong>{preview.summary.ready_rows}</strong></span>
            <span>阻断 <strong>{preview.summary.blocked_rows}</strong></span>
            <span>忽略 <strong>{preview.summary.ignored_rows}</strong></span>
            <span>含警告 <strong>{preview.summary.warning_rows}</strong></span>
          </div>
          <div className="v2-selection-heading">
            <label className="v2-check-label"><input type="checkbox" checked={readyRows.length > 0 && readyRows.every((row) => selectedRows.has(row.source_row))} onChange={toggleAllReady} />选择全部可导入行</label>
            <span>已选 {selectedRows.size} / {readyRows.length}</span>
          </div>
          <div className="v2-table-wrap">
            <table className="v2-import-table">
              <thead><tr><th aria-label="选择" /><th>源行</th><th>出货条码</th><th>退货条码</th><th>客户原值</th><th>时间</th><th>处理结果</th></tr></thead>
              <tbody>{preview.rows.map((row) => (
                <tr key={row.source_row} className={row.status === "blocked" ? "blocked" : ""}>
                  <td><input type="checkbox" aria-label={`选择第 ${row.source_row} 行`} disabled={row.status !== "ready"} checked={selectedRows.has(row.source_row)} onChange={() => toggleRow(row.source_row)} /></td>
                  <td>{row.source_row}</td>
                  <td><strong className="v2-mono">{row.shipment_barcode ?? "—"}</strong></td>
                  <td><strong className="v2-mono">{row.return_barcode ?? "—"}</strong></td>
                  <td>{row.counterparty_raw ?? "unknown"}</td>
                  <td><small>{row.shipment_time_normalized ?? "出货 unknown"}</small><small>{row.return_barcode ? (row.return_time_normalized ?? "退回 unknown") : ""}</small></td>
                  <td><span className={`v2-badge import-${row.status}`}>{row.status === "ready" ? "可导入" : row.status === "blocked" ? "阻断" : "忽略"}</span>{row.issues.map((issue) => <small key={`${row.source_row}-${issue.code}-${issue.field ?? "row"}`} className={issue.severity === "error" ? "v2-row-error" : ""}>{issue.message}</small>)}</td>
                </tr>
              ))}</tbody>
            </table>
          </div>
          <div className="v2-form-actions">
            <span className="v2-import-selection">已选择 {selectedRows.size} 行</span>
            <button className="v2-button primary" type="button" onClick={() => void commitImport()} disabled={loading || !activated || selectedRows.size === 0}>
              {loading ? <LoaderCircle className="v2-spin" size={16} /> : <CheckCircle2 size={16} />} 提交历史导入
            </button>
          </div>
        </section>
      )}

      {report && <div className="v2-upgrade-result success"><strong>导入批次已提交</strong><span>batch_id：{report.batch_id}</span><span>来源：{report.source_kind}</span><span>质检状态：{report.quality_status}</span><span>{report.idempotent_replay ? "幂等重放" : "首次提交"}</span></div>}
    </section>
  );
}

interface ColumnSelectProps {
  label: string;
  headers: string[];
  value: number | null;
  required?: boolean;
  onChange: (value: number | null) => void;
}

function ColumnSelect({ label, headers, value, required = false, onChange }: ColumnSelectProps) {
  return (
    <label>
      <span>{label}</span>
      <select value={value ?? ""} onChange={(event) => onChange(optionalColumn(event.target.value))}>
        {!required && <option value="">不映射</option>}
        {headers.map((header, index) => <option key={`${index}-${header}`} value={index}>{header || `未命名列 ${index + 1}`}</option>)}
      </select>
    </label>
  );
}
