import {
  type FormEvent,
  useCallback,
  useEffect,
  useMemo,
  useState,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ArrowLeft,
  Boxes,
  CheckCircle2,
  ClipboardCheck,
  Gauge,
  PackagePlus,
  RefreshCw,
  Search,
  ShieldAlert,
  type LucideIcon,
} from "lucide-react";
import "./InventoryWorkspace.css";

type WorkspacePage = "overview" | "receipt" | "quality" | "inventory";
type InventoryStatus =
  | "received"
  | "available"
  | "reserved"
  | "shipped"
  | "delivered"
  | "quarantined"
  | "scrapped"
  | "returned_to_owner"
  | "voided";
type QualityStatus = "untested" | "testing" | "passed" | "failed" | "waived";
type InspectionKind = "initial" | "retest";
type QualityOutcome = "passed" | "failed";
type Notice = { type: "success" | "error"; text: string };

export interface InventoryWorkspaceProps {
  onBackToLegacy?: () => void;
  actorId?: string;
}

interface PostReceiptRequest {
  request_id: string;
  idempotency_key: string;
  receipt_no: string;
  owner_name: string;
  sku_code: string;
  sku_name: string;
  source_reference: string | null;
  received_at: string;
  actor_id: string;
  barcodes: string[];
  notes: string | null;
}

interface ReceiptUnitDto {
  inventory_unit_id: string;
  barcode: string;
}

interface PostReceiptResponse {
  receipt_id: string;
  receipt_line_id: string;
  receipt_no: string;
  owner_party_id: string;
  sku_id: string;
  received_count: number;
  units: ReceiptUnitDto[];
  idempotent_replay: boolean;
}

interface InspectionResultInput {
  barcode: string;
  outcome: QualityOutcome;
  defect_code: string | null;
  measurements: Record<string, never>;
  notes: string | null;
}

interface CompleteInspectionRequest {
  request_id: string;
  idempotency_key: string;
  inspection_no: string;
  inspection_kind: InspectionKind;
  inspector_id: string;
  inspected_at: string;
  results: InspectionResultInput[];
}

interface InspectedUnitDto {
  inventory_unit_id: string;
  barcode: string;
  outcome: QualityOutcome;
  inventory_status: InventoryStatus;
  quality_status: QualityStatus;
  location_id: string;
  version: number;
}

interface CompleteInspectionResponse {
  inspection_id: string;
  inspection_no: string;
  inspected_count: number;
  passed_count: number;
  failed_count: number;
  units: InspectedUnitDto[];
  idempotent_replay: boolean;
}

interface InventoryListQuery {
  search: string | null;
  owner_party_id: string | null;
  sku_id: string | null;
  inventory_status: InventoryStatus | null;
  quality_status: QualityStatus | null;
  limit: number;
  offset: number;
}

interface InventoryListItem {
  inventory_unit_id: string;
  barcode: string;
  receipt_id: string;
  receipt_no: string;
  owner_party_id: string;
  owner_name: string;
  sku_id: string;
  sku_code: string;
  sku_name: string;
  location_id: string;
  location_code: string;
  location_name: string;
  inventory_status: InventoryStatus;
  quality_status: QualityStatus;
  version: number;
  received_at: string;
  updated_at: string;
}

interface InventoryListResponse {
  items: InventoryListItem[];
  total: number;
  limit: number;
  offset: number;
}

interface InventoryStatusSummary {
  received: number;
  available: number;
  reserved: number;
  shipped: number;
  delivered: number;
  quarantined: number;
  scrapped: number;
  returned_to_owner: number;
  voided: number;
}

interface QualityStatusSummary {
  untested: number;
  testing: number;
  passed: number;
  failed: number;
  waived: number;
}

interface DashboardDto {
  total_units: number;
  inventory: InventoryStatusSummary;
  quality: QualityStatusSummary;
}

interface NavigationItem {
  id: WorkspacePage;
  label: string;
  description: string;
  icon: LucideIcon;
}

const navigationItems: NavigationItem[] = [
  { id: "overview", label: "概览", description: "库存与质检态势", icon: Gauge },
  { id: "receipt", label: "入库", description: "批量扫码收货", icon: PackagePlus },
  { id: "quality", label: "质检", description: "初检与复检", icon: ClipboardCheck },
  { id: "inventory", label: "库存", description: "单件库存查询", icon: Boxes },
];

const inventoryStatusLabels: Record<InventoryStatus, string> = {
  received: "待检入库",
  available: "可用",
  reserved: "已预留",
  shipped: "已出库",
  delivered: "已交货",
  quarantined: "隔离中",
  scrapped: "已报损",
  returned_to_owner: "已退货主",
  voided: "已作废",
};

const qualityStatusLabels: Record<QualityStatus, string> = {
  untested: "未测试",
  testing: "测试中",
  passed: "合格",
  failed: "不合格",
  waived: "例外放行",
};

function createId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function getLocalDateTimeValue(date = new Date()): string {
  const local = new Date(date.getTime() - date.getTimezoneOffset() * 60_000);
  return local.toISOString().slice(0, 19);
}

function toUtcIso(value: string): string {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) throw new Error("时间格式无效");
  return parsed.toISOString();
}

function makeDocumentNumber(prefix: "RK" | "ZJ"): string {
  const stamp = new Date().toISOString().replace(/[-:TZ.]/g, "").slice(0, 14);
  return `${prefix}-${stamp}-${createId().slice(0, 6).toUpperCase()}`;
}

function displayError(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  if (typeof error === "object" && error !== null) {
    const record = error as Record<string, unknown>;
    if (typeof record.message === "string") return record.message;
    try {
      return JSON.stringify(error);
    } catch {
      return "发生未知错误";
    }
  }
  return String(error);
}

function formatDateTime(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString("zh-CN", { hour12: false });
}

function getDefaultActorId(): string {
  const storageKey = "inventory-v2-offline-actor-id";
  const existing = window.localStorage.getItem(storageKey);
  if (existing) return existing;
  const created = createId();
  window.localStorage.setItem(storageKey, created);
  return created;
}

function emptyInventoryQuery(): InventoryListQuery {
  return {
    search: null,
    owner_party_id: null,
    sku_id: null,
    inventory_status: null,
    quality_status: null,
    limit: 500,
    offset: 0,
  };
}

export default function InventoryWorkspace({
  onBackToLegacy,
  actorId,
}: InventoryWorkspaceProps) {
  const [page, setPage] = useState<WorkspacePage>("overview");
  const [resolvedActorId] = useState(() => actorId?.trim() || getDefaultActorId());

  const [dashboard, setDashboard] = useState<DashboardDto | null>(null);
  const [dashboardLoading, setDashboardLoading] = useState(false);
  const [dashboardError, setDashboardError] = useState<string | null>(null);

  const [ownerName, setOwnerName] = useState("");
  const [skuCode, setSkuCode] = useState("");
  const [skuName, setSkuName] = useState("");
  const [receivedAt, setReceivedAt] = useState(getLocalDateTimeValue);
  const [sourceReference, setSourceReference] = useState("");
  const [barcodeLines, setBarcodeLines] = useState("");
  const [receiptLoading, setReceiptLoading] = useState(false);
  const [receiptNotice, setReceiptNotice] = useState<Notice | null>(null);

  const [qualityItems, setQualityItems] = useState<InventoryListItem[]>([]);
  const [qualityLoading, setQualityLoading] = useState(false);
  const [qualityNotice, setQualityNotice] = useState<Notice | null>(null);
  const [selectedBarcodes, setSelectedBarcodes] = useState<Set<string>>(() => new Set());
  const [inspectionKind, setInspectionKind] = useState<InspectionKind>("initial");
  const [inspectionOutcome, setInspectionOutcome] = useState<QualityOutcome>("passed");
  const [defectCode, setDefectCode] = useState("");
  const [inspectionNotes, setInspectionNotes] = useState("");

  const [inventoryItems, setInventoryItems] = useState<InventoryListItem[]>([]);
  const [inventoryTotal, setInventoryTotal] = useState(0);
  const [inventoryLoading, setInventoryLoading] = useState(false);
  const [inventoryError, setInventoryError] = useState<string | null>(null);
  const [inventorySearch, setInventorySearch] = useState("");
  const [inventoryStatus, setInventoryStatus] = useState<InventoryStatus | "">("");
  const [qualityStatus, setQualityStatus] = useState<QualityStatus | "">("");

  const barcodes = useMemo(
    () => barcodeLines.split(/\r?\n/).map((value) => value.trim()).filter(Boolean),
    [barcodeLines],
  );

  const eligibleQualityItems = useMemo(() => {
    const requiredStatus: QualityStatus = inspectionKind === "initial" ? "untested" : "failed";
    return qualityItems.filter((item) => item.quality_status === requiredStatus);
  }, [inspectionKind, qualityItems]);

  const refreshDashboard = useCallback(async () => {
    setDashboardLoading(true);
    setDashboardError(null);
    try {
      const response = await invoke<DashboardDto>("v2_get_dashboard", {
        query: { owner_party_id: null, sku_id: null },
      });
      setDashboard(response);
    } catch (error) {
      setDashboardError(displayError(error));
    } finally {
      setDashboardLoading(false);
    }
  }, []);

  const refreshQualityItems = useCallback(async () => {
    setQualityLoading(true);
    setQualityNotice(null);
    try {
      const response = await invoke<InventoryListResponse>("v2_list_inventory", {
        query: emptyInventoryQuery(),
      });
      setQualityItems(
        response.items.filter((item) => item.quality_status === "untested" || item.quality_status === "failed"),
      );
      setSelectedBarcodes(new Set());
    } catch (error) {
      setQualityNotice({ type: "error", text: `读取待检库存失败：${displayError(error)}` });
    } finally {
      setQualityLoading(false);
    }
  }, []);

  const refreshInventory = useCallback(async () => {
    setInventoryLoading(true);
    setInventoryError(null);
    try {
      const query: InventoryListQuery = {
        ...emptyInventoryQuery(),
        search: inventorySearch.trim() || null,
        inventory_status: inventoryStatus || null,
        quality_status: qualityStatus || null,
      };
      const response = await invoke<InventoryListResponse>("v2_list_inventory", { query });
      setInventoryItems(response.items);
      setInventoryTotal(response.total);
    } catch (error) {
      setInventoryError(displayError(error));
    } finally {
      setInventoryLoading(false);
    }
  }, [inventorySearch, inventoryStatus, qualityStatus]);

  useEffect(() => {
    if (page === "overview") void refreshDashboard();
    if (page === "quality") void refreshQualityItems();
    if (page === "inventory") void refreshInventory();
  }, [page, refreshDashboard, refreshInventory, refreshQualityItems]);

  useEffect(() => {
    setSelectedBarcodes(new Set());
    setQualityNotice(null);
  }, [inspectionKind]);

  async function submitReceipt(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setReceiptNotice(null);
    const uniqueBarcodes = new Set(barcodes);
    if (!ownerName.trim() || !skuCode.trim() || !skuName.trim()) {
      setReceiptNotice({ type: "error", text: "请完整填写货主、型号编码和型号名称。" });
      return;
    }
    if (barcodes.length === 0) {
      setReceiptNotice({ type: "error", text: "请至少扫描一个条码，每行一个。" });
      return;
    }
    if (uniqueBarcodes.size !== barcodes.length) {
      setReceiptNotice({ type: "error", text: "本批次存在重复条码，请检查后再提交。" });
      return;
    }

    setReceiptLoading(true);
    try {
      const operationId = createId();
      const request: PostReceiptRequest = {
        request_id: operationId,
        idempotency_key: `receipt:${operationId}`,
        receipt_no: makeDocumentNumber("RK"),
        owner_name: ownerName.trim(),
        sku_code: skuCode.trim(),
        sku_name: skuName.trim(),
        source_reference: sourceReference.trim() || null,
        received_at: toUtcIso(receivedAt),
        actor_id: resolvedActorId,
        barcodes,
        notes: null,
      };
      const response = await invoke<PostReceiptResponse>("v2_post_receipt", { input: request });
      setReceiptNotice({
        type: "success",
        text: `${response.receipt_no} 已原子入库 ${response.received_count} 件${
          response.idempotent_replay ? "（幂等回放）" : ""
        }。新入库单件默认标记为未测试。`,
      });
      setBarcodeLines("");
      setSourceReference("");
      setReceivedAt(getLocalDateTimeValue());
      await refreshDashboard();
    } catch (error) {
      setReceiptNotice({ type: "error", text: `入库失败：${displayError(error)}` });
    } finally {
      setReceiptLoading(false);
    }
  }

  function toggleQualityBarcode(barcode: string) {
    setSelectedBarcodes((current) => {
      const next = new Set(current);
      if (next.has(barcode)) next.delete(barcode);
      else next.add(barcode);
      return next;
    });
  }

  async function submitInspection(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setQualityNotice(null);
    if (selectedBarcodes.size === 0) {
      setQualityNotice({ type: "error", text: "请至少选择一件待检库存。" });
      return;
    }
    if (inspectionOutcome === "failed" && !defectCode.trim() && !inspectionNotes.trim()) {
      setQualityNotice({ type: "error", text: "不合格结果请填写缺陷代码或备注。" });
      return;
    }

    setQualityLoading(true);
    try {
      const operationId = createId();
      const request: CompleteInspectionRequest = {
        request_id: operationId,
        idempotency_key: `inspection:${operationId}`,
        inspection_no: makeDocumentNumber("ZJ"),
        inspection_kind: inspectionKind,
        inspector_id: resolvedActorId,
        inspected_at: new Date().toISOString(),
        results: Array.from(selectedBarcodes).map((barcode) => ({
          barcode,
          outcome: inspectionOutcome,
          defect_code: defectCode.trim() || null,
          measurements: {},
          notes: inspectionNotes.trim() || null,
        })),
      };
      const response = await invoke<CompleteInspectionResponse>("v2_complete_inspection", { input: request });
      setQualityNotice({
        type: "success",
        text: `${response.inspection_no} 已完成：合格 ${response.passed_count} 件，不合格 ${response.failed_count} 件${
          response.idempotent_replay ? "（幂等回放）" : ""
        }。`,
      });
      setDefectCode("");
      setInspectionNotes("");
      setSelectedBarcodes(new Set());
      const listResponse = await invoke<InventoryListResponse>("v2_list_inventory", {
        query: emptyInventoryQuery(),
      });
      setQualityItems(
        listResponse.items.filter(
          (item) => item.quality_status === "untested" || item.quality_status === "failed",
        ),
      );
      await refreshDashboard();
    } catch (error) {
      setQualityNotice({ type: "error", text: `质检提交失败：${displayError(error)}` });
    } finally {
      setQualityLoading(false);
    }
  }

  function renderOverview() {
    const inventory = dashboard?.inventory;
    const quality = dashboard?.quality;
    return (
      <section className="v2-page" aria-labelledby="v2-overview-title">
        <div className="v2-page-heading">
          <div>
            <span className="v2-eyebrow">库存工作台</span>
            <h2 id="v2-overview-title">业务概览</h2>
            <p>查看单件库存、待检和隔离情况。</p>
          </div>
          <button className="v2-button" type="button" onClick={() => void refreshDashboard()} disabled={dashboardLoading}>
            <RefreshCw size={16} className={dashboardLoading ? "v2-spin" : ""} /> 刷新
          </button>
        </div>
        {dashboardError && <div className="v2-notice error">读取概览失败：{dashboardError}</div>}
        <div className="v2-metric-grid" aria-busy={dashboardLoading}>
          <article className="v2-metric-card primary"><span>库存单件总数</span><strong>{dashboard?.total_units ?? "—"}</strong><small>所有状态的可追踪条码</small></article>
          <article className="v2-metric-card"><span>可用库存</span><strong>{inventory?.available ?? "—"}</strong><small>质检合格，可参与凑单</small></article>
          <article className="v2-metric-card warning"><span>待检库存</span><strong>{quality?.untested ?? "—"}</strong><small>入库后尚未完成初检</small></article>
          <article className="v2-metric-card danger"><span>隔离库存</span><strong>{inventory?.quarantined ?? "—"}</strong><small>不合格或退回待复检</small></article>
        </div>
        <div className="v2-summary-grid">
          <article className="v2-panel">
            <h3>库存状态</h3>
            <div className="v2-summary-list">
              <span>待检入库 <strong>{inventory?.received ?? 0}</strong></span>
              <span>已预留 <strong>{inventory?.reserved ?? 0}</strong></span>
              <span>已出库 <strong>{inventory?.shipped ?? 0}</strong></span>
              <span>已交货 <strong>{inventory?.delivered ?? 0}</strong></span>
            </div>
          </article>
          <article className="v2-panel">
            <h3>质检状态</h3>
            <div className="v2-summary-list">
              <span>合格 <strong>{quality?.passed ?? 0}</strong></span>
              <span>不合格 <strong>{quality?.failed ?? 0}</strong></span>
              <span>测试中 <strong>{quality?.testing ?? 0}</strong></span>
              <span>例外放行 <strong>{quality?.waived ?? 0}</strong></span>
            </div>
          </article>
        </div>
      </section>
    );
  }

  function renderReceipt() {
    return (
      <section className="v2-page" aria-labelledby="v2-receipt-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">入库管理</span><h2 id="v2-receipt-title">新建入库批次</h2><p>同一货主、同一型号的一批货物一次提交，整批成功或整批回滚。</p></div>
        </div>
        <form className="v2-panel v2-form" onSubmit={submitReceipt}>
          <div className="v2-form-grid">
            <label><span>货主 / 供货客户 *</span><input value={ownerName} onChange={(event) => setOwnerName(event.target.value)} placeholder="例如：客户 A" autoComplete="off" /></label>
            <label><span>型号编码 *</span><input value={skuCode} onChange={(event) => setSkuCode(event.target.value)} placeholder="例如：DDR4-32G-3200" autoComplete="off" /></label>
            <label><span>型号名称 *</span><input value={skuName} onChange={(event) => setSkuName(event.target.value)} placeholder="例如：32G 3200 内存" autoComplete="off" /></label>
            <label><span>入库时间 *</span><input type="datetime-local" step="1" value={receivedAt} onChange={(event) => setReceivedAt(event.target.value)} /></label>
            <label className="v2-span-two"><span>来源单号 / 备注</span><input value={sourceReference} onChange={(event) => setSourceReference(event.target.value)} placeholder="可选，例如供应商送货单号" autoComplete="off" /></label>
          </div>
          <label className="v2-scan-field">
            <span>单件条码 / SN *</span>
            <textarea value={barcodeLines} onChange={(event) => setBarcodeLines(event.target.value)} placeholder={"扫码后按回车，每行一个条码\n也可一次粘贴多行"} autoFocus spellCheck={false} />
            <small>已识别 {barcodes.length} 件；提交时会在数据库事务中检查所有条码唯一性。</small>
          </label>
          {receiptNotice && <div className={`v2-notice ${receiptNotice.type}`}>{receiptNotice.text}</div>}
          <div className="v2-form-actions">
            <button className="v2-button" type="button" onClick={() => setBarcodeLines("")} disabled={receiptLoading || barcodes.length === 0}>清空条码</button>
            <button className="v2-button primary" type="submit" disabled={receiptLoading}>{receiptLoading ? "正在原子入库…" : `确认入库 ${barcodes.length} 件`}</button>
          </div>
        </form>
      </section>
    );
  }

  function renderQuality() {
    return (
      <section className="v2-page" aria-labelledby="v2-quality-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">质量管理</span><h2 id="v2-quality-title">单件质检</h2><p>未测试库存做初检，不合格隔离库存可做复检。</p></div>
          <button className="v2-button" type="button" onClick={() => void refreshQualityItems()} disabled={qualityLoading}><RefreshCw size={16} className={qualityLoading ? "v2-spin" : ""} /> 刷新</button>
        </div>
        <form className="v2-quality-layout" onSubmit={submitInspection}>
          <div className="v2-panel v2-quality-list-panel">
            <div className="v2-segmented" aria-label="质检类型">
              <button type="button" className={inspectionKind === "initial" ? "active" : ""} onClick={() => setInspectionKind("initial")}>初检（未测试）</button>
              <button type="button" className={inspectionKind === "retest" ? "active" : ""} onClick={() => setInspectionKind("retest")}>复检（不合格）</button>
            </div>
            <div className="v2-selection-heading"><strong>选择库存单件</strong><span>已选 {selectedBarcodes.size} / {eligibleQualityItems.length}</span></div>
            <div className="v2-select-list" aria-busy={qualityLoading}>
              {!qualityLoading && eligibleQualityItems.length === 0 && <div className="v2-empty"><CheckCircle2 size={28} /> 当前没有符合条件的待检库存</div>}
              {eligibleQualityItems.map((item) => (
                <label className="v2-select-item" key={item.inventory_unit_id}>
                  <input type="checkbox" checked={selectedBarcodes.has(item.barcode)} onChange={() => toggleQualityBarcode(item.barcode)} />
                  <span className="v2-item-main"><strong>{item.barcode}</strong><small>{item.owner_name} · {item.sku_code} / {item.sku_name}</small></span>
                  <span className={`v2-badge quality-${item.quality_status}`}>{qualityStatusLabels[item.quality_status]}</span>
                </label>
              ))}
            </div>
          </div>
          <aside className="v2-panel v2-inspection-form">
            <h3>本次质检结果</h3>
            <label><span>结果 *</span><select value={inspectionOutcome} onChange={(event) => setInspectionOutcome(event.target.value as QualityOutcome)}><option value="passed">合格</option><option value="failed">不合格</option></select></label>
            <label><span>缺陷代码</span><input value={defectCode} onChange={(event) => setDefectCode(event.target.value)} placeholder="不合格时建议填写" /></label>
            <label><span>质检备注</span><textarea value={inspectionNotes} onChange={(event) => setInspectionNotes(event.target.value)} placeholder="记录现象、测试方法或复检说明" /></label>
            <div className="v2-rule-hint"><ShieldAlert size={17} /><span>不合格单件会进入隔离区；只有合格或经授权放行的单件可参与出库分配。</span></div>
            <button className="v2-button primary wide" type="submit" disabled={qualityLoading || selectedBarcodes.size === 0}>{qualityLoading ? "正在提交…" : `提交 ${selectedBarcodes.size} 件质检结果`}</button>
          </aside>
        </form>
        {qualityNotice && <div className={`v2-notice ${qualityNotice.type}`}>{qualityNotice.text}</div>}
      </section>
    );
  }

  function renderInventory() {
    return (
      <section className="v2-page" aria-labelledby="v2-inventory-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">库存台账</span><h2 id="v2-inventory-title">单件库存</h2><p>按条码、货主、型号或单号搜索当前库存投影。</p></div>
          <button className="v2-button" type="button" onClick={() => void refreshInventory()} disabled={inventoryLoading}><RefreshCw size={16} className={inventoryLoading ? "v2-spin" : ""} /> 刷新</button>
        </div>
        <form className="v2-panel v2-filters" onSubmit={(event) => { event.preventDefault(); void refreshInventory(); }}>
          <label className="v2-search"><Search size={17} /><input value={inventorySearch} onChange={(event) => setInventorySearch(event.target.value)} placeholder="条码、货主、型号、入库单号" /></label>
          <select aria-label="库存状态" value={inventoryStatus} onChange={(event) => setInventoryStatus(event.target.value as InventoryStatus | "")}><option value="">全部库存状态</option>{Object.entries(inventoryStatusLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select>
          <select aria-label="质检状态" value={qualityStatus} onChange={(event) => setQualityStatus(event.target.value as QualityStatus | "")}><option value="">全部质检状态</option>{Object.entries(qualityStatusLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select>
          <button className="v2-button primary" type="submit" disabled={inventoryLoading}>查询</button>
        </form>
        {inventoryError && <div className="v2-notice error">读取库存失败：{inventoryError}</div>}
        <div className="v2-panel v2-table-panel">
          <div className="v2-table-meta">共 {inventoryTotal} 件{inventoryTotal > inventoryItems.length ? `，当前显示前 ${inventoryItems.length} 件` : ""}</div>
          <div className="v2-table-wrap">
            <table>
              <thead><tr><th>条码 / SN</th><th>货主</th><th>产品型号</th><th>入库时间</th><th>库存状态</th><th>质检状态</th></tr></thead>
              <tbody>
                {!inventoryLoading && inventoryItems.length === 0 && <tr><td className="v2-table-empty" colSpan={6}>没有匹配的库存记录</td></tr>}
                {inventoryItems.map((item) => (
                  <tr key={item.inventory_unit_id}>
                    <td><strong className="v2-mono">{item.barcode}</strong><small>{item.receipt_no}</small></td>
                    <td>{item.owner_name}</td>
                    <td><strong>{item.sku_code}</strong><small>{item.sku_name}</small></td>
                    <td>{formatDateTime(item.received_at)}</td>
                    <td><span className={`v2-badge inventory-${item.inventory_status}`}>{inventoryStatusLabels[item.inventory_status]}</span></td>
                    <td><span className={`v2-badge quality-${item.quality_status}`}>{qualityStatusLabels[item.quality_status]}</span></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      </section>
    );
  }

  return (
    <div className="v2-workspace">
      <aside className="v2-sidebar">
        <div className="v2-brand"><span><Boxes size={22} /></span><div><strong>库存管理 V2</strong><small>离线单用户工作区</small></div></div>
        <nav aria-label="库存模块">
          {navigationItems.map((item) => {
            const Icon = item.icon;
            return <button key={item.id} type="button" className={page === item.id ? "active" : ""} onClick={() => setPage(item.id)}><Icon size={19} /><span><strong>{item.label}</strong><small>{item.description}</small></span></button>;
          })}
        </nav>
        <div className="v2-sidebar-footer">
          <span>本地数据模式</span><small>SQLite · 单用户 · 原子事务</small>
          {onBackToLegacy && <button className="v2-back-button" type="button" onClick={onBackToLegacy}><ArrowLeft size={16} /> 返回旧版工具</button>}
        </div>
      </aside>
      <main className="v2-content">
        {page === "overview" && renderOverview()}
        {page === "receipt" && renderReceipt()}
        {page === "quality" && renderQuality()}
        {page === "inventory" && renderInventory()}
      </main>
    </div>
  );
}
