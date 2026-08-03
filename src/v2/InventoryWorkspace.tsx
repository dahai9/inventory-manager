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
  Truck,
  type LucideIcon,
} from "lucide-react";
import "./InventoryWorkspace.css";

type WorkspacePage = "overview" | "receipt" | "quality" | "inventory" | "outbound";
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

interface CreateOutboundOrderRequest {
  request_id: string;
  idempotency_key: string;
  order_no: string;
  upstream_receiver_name: string;
  sku_code: string;
  sku_name: string;
  required_quantity: number;
  required_at: string | null;
  actor_id: string;
}

interface CreateOutboundOrderResponse {
  order_id: string;
  order_line_id: string;
  order_no: string;
  upstream_receiver_id: string;
  sku_id: string;
  required_quantity: number;
  idempotent_replay: boolean;
}

interface AllocationItemDto {
  allocation_id: string;
  barcode: string;
  owner_party_id: string;
  sku_id: string;
}

interface AllocateOutboundResponse {
  order_id: string;
  order_line_id: string;
  allocated_count: number;
  order_status: string;
  allocations: AllocationItemDto[];
  idempotent_replay: boolean;
}

interface ShipmentItemDto {
  shipment_line_id: string;
  allocation_id: string;
  barcode: string;
  owner_party_id: string;
  sku_id: string;
}

interface ShipOutboundResponse {
  shipment_id: string;
  shipment_no: string;
  shipped_count: number;
  order_status: string;
  items: ShipmentItemDto[];
  idempotent_replay: boolean;
}

interface ConfirmOutboundDeliveryResponse {
  confirmation_id: string;
  confirmation_code: string;
  delivered_count: number;
  shipment_status: string;
  idempotent_replay: boolean;
}

interface ReturnOutboundShipmentResponse {
  return_batch_id: string;
  return_no: string;
  quarantined_count: number;
  idempotent_replay: boolean;
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
  { id: "outbound", label: "出库", description: "凑单交货与退回", icon: Truck },
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

function makeDocumentNumber(prefix: "RK" | "ZJ" | "CK" | "TH"): string {
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

  const [outboundReceiver, setOutboundReceiver] = useState("");
  const [outboundOrderNo, setOutboundOrderNo] = useState("");
  const [outboundSkuCode, setOutboundSkuCode] = useState("");
  const [outboundSkuName, setOutboundSkuName] = useState("");
  const [outboundQuantity, setOutboundQuantity] = useState("1");
  const [outboundBarcodes, setOutboundBarcodes] = useState("");
  const [outboundShipmentNo, setOutboundShipmentNo] = useState("");
  const [outboundConfirmationCode, setOutboundConfirmationCode] = useState("");
  const [outboundReturnReason, setOutboundReturnReason] = useState("");
  const [outboundNotice, setOutboundNotice] = useState<Notice | null>(null);
  const [outboundLoading, setOutboundLoading] = useState(false);
  const [outboundOrder, setOutboundOrder] = useState<CreateOutboundOrderResponse | null>(null);
  const [outboundAllocation, setOutboundAllocation] = useState<AllocateOutboundResponse | null>(null);
  const [outboundShipment, setOutboundShipment] = useState<ShipOutboundResponse | null>(null);

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

  async function createOutboundOrder(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setOutboundNotice(null);
    const quantity = Number.parseInt(outboundQuantity, 10);
    if (!outboundReceiver.trim() || !outboundSkuCode.trim() || !outboundSkuName.trim() || !outboundOrderNo.trim()) {
      setOutboundNotice({ type: "error", text: "请完整填写上游收货方、订单号和产品型号。" });
      return;
    }
    if (!Number.isInteger(quantity) || quantity <= 0) {
      setOutboundNotice({ type: "error", text: "需求数量必须是大于 0 的整数。" });
      return;
    }
    setOutboundLoading(true);
    try {
      const operationId = createId();
      const response = await invoke<CreateOutboundOrderResponse>("v2_create_outbound_order", {
        input: {
          request_id: operationId,
          idempotency_key: `outbound-order:${operationId}`,
          order_no: outboundOrderNo.trim(),
          upstream_receiver_name: outboundReceiver.trim(),
          sku_code: outboundSkuCode.trim(),
          sku_name: outboundSkuName.trim(),
          required_quantity: quantity,
          required_at: null,
          actor_id: resolvedActorId,
        } satisfies CreateOutboundOrderRequest,
      });
      setOutboundOrder(response);
      setOutboundAllocation(null);
      setOutboundShipment(null);
      setOutboundBarcodes("");
      setOutboundShipmentNo("");
      setOutboundConfirmationCode("");
      setOutboundNotice({ type: "success", text: `${response.order_no} 已建单，请继续分配可用库存。` });
    } catch (error) {
      setOutboundNotice({ type: "error", text: `建单失败：${displayError(error)}` });
    } finally {
      setOutboundLoading(false);
    }
  }

  async function allocateOutboundOrder() {
    if (!outboundOrder) return;
    setOutboundLoading(true);
    setOutboundNotice(null);
    try {
      const operationId = createId();
      const response = await invoke<AllocateOutboundResponse>("v2_allocate_outbound_order", {
        input: {
          request_id: operationId,
          idempotency_key: `outbound-allocation:${operationId}`,
          order_id: outboundOrder.order_id,
          order_line_id: outboundOrder.order_line_id,
          barcodes: outboundBarcodes.split(/\r?\n/).map((value) => value.trim()).filter(Boolean),
          actor_id: resolvedActorId,
        },
      });
      setOutboundAllocation(response);
      setOutboundBarcodes("");
      setOutboundNotice({ type: "success", text: `已分配 ${response.allocated_count} 件，状态：${response.order_status}。` });
    } catch (error) {
      setOutboundNotice({ type: "error", text: `库存分配失败：${displayError(error)}` });
    } finally {
      setOutboundLoading(false);
    }
  }

  async function shipOutboundOrder() {
    if (!outboundOrder || !outboundAllocation || outboundAllocation.allocations.length === 0) return;
    setOutboundLoading(true);
    setOutboundNotice(null);
    try {
      const operationId = createId();
      const response = await invoke<ShipOutboundResponse>("v2_ship_outbound_order", {
        input: {
          request_id: operationId,
          idempotency_key: `outbound-shipment:${operationId}`,
          order_id: outboundOrder.order_id,
          shipment_no: outboundShipmentNo.trim() || makeDocumentNumber("CK"),
          allocation_ids: outboundAllocation.allocations.map((item) => item.allocation_id),
          barcodes: [],
          shipped_at: new Date().toISOString(),
          actor_id: resolvedActorId,
        },
      });
      setOutboundShipment(response);
      setOutboundShipmentNo(response.shipment_no);
      setOutboundNotice({ type: "success", text: `${response.shipment_no} 已出库 ${response.shipped_count} 件。` });
      await refreshDashboard();
    } catch (error) {
      setOutboundNotice({ type: "error", text: `出库失败：${displayError(error)}` });
    } finally {
      setOutboundLoading(false);
    }
  }

  async function confirmOutboundDelivery() {
    if (!outboundShipment) return;
    if (!outboundConfirmationCode.trim()) {
      setOutboundNotice({ type: "error", text: "请输入上游确认码。" });
      return;
    }
    setOutboundLoading(true);
    setOutboundNotice(null);
    try {
      const operationId = createId();
      const response = await invoke<ConfirmOutboundDeliveryResponse>("v2_confirm_outbound_delivery", {
        input: {
          request_id: operationId,
          idempotency_key: `outbound-delivery:${operationId}`,
          shipment_id: outboundShipment.shipment_id,
          confirmation_code: outboundConfirmationCode.trim(),
          shipment_line_ids: [],
          confirmed_at: new Date().toISOString(),
          confirmed_by: resolvedActorId,
          notes: null,
        },
      });
      setOutboundNotice({ type: "success", text: `已确认交货 ${response.delivered_count} 件，批次状态：${response.shipment_status}。` });
      await refreshDashboard();
    } catch (error) {
      setOutboundNotice({ type: "error", text: `交货确认失败：${displayError(error)}` });
    } finally {
      setOutboundLoading(false);
    }
  }

  async function returnOutboundShipment() {
    if (!outboundShipment) return;
    if (!outboundReturnReason.trim()) {
      setOutboundNotice({ type: "error", text: "请输入退回原因。" });
      return;
    }
    setOutboundLoading(true);
    setOutboundNotice(null);
    try {
      const operationId = createId();
      const response = await invoke<ReturnOutboundShipmentResponse>("v2_return_outbound_shipment", {
        input: {
          request_id: operationId,
          idempotency_key: `outbound-return:${operationId}`,
          shipment_id: outboundShipment.shipment_id,
          shipment_line_ids: [],
          return_no: makeDocumentNumber("TH"),
          returned_at: new Date().toISOString(),
          reason: outboundReturnReason.trim(),
          actor_id: resolvedActorId,
        },
      });
      setOutboundNotice({ type: "success", text: `${response.return_no} 已登记退回 ${response.quarantined_count} 件，并进入隔离区待复检。` });
      setOutboundReturnReason("");
      await refreshDashboard();
    } catch (error) {
      setOutboundNotice({ type: "error", text: `退回登记失败：${displayError(error)}` });
    } finally {
      setOutboundLoading(false);
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

  function renderOutbound() {
    return (
      <section className="v2-page" aria-labelledby="v2-outbound-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">出库管理</span><h2 id="v2-outbound-title">凑单交货</h2><p>多个货主的合格库存可以组成同一张上游订单，所有单件仍保留来源追踪。</p></div>
        </div>
        <div className="v2-outbound-layout">
          <form className="v2-panel v2-form" onSubmit={createOutboundOrder}>
            <h3>1. 建立上游需求</h3>
            <div className="v2-form-grid">
              <label><span>上游收货方 *</span><input value={outboundReceiver} onChange={(event) => setOutboundReceiver(event.target.value)} placeholder="例如：上游客户 A" /></label>
              <label><span>订单号 *</span><input value={outboundOrderNo} onChange={(event) => setOutboundOrderNo(event.target.value)} placeholder="例如：SO-20260803-01" /></label>
              <label><span>型号编码 *</span><input value={outboundSkuCode} onChange={(event) => setOutboundSkuCode(event.target.value)} placeholder="例如：DDR4-32G-3200" /></label>
              <label><span>型号名称 *</span><input value={outboundSkuName} onChange={(event) => setOutboundSkuName(event.target.value)} placeholder="例如：32G 3200 内存" /></label>
              <label><span>需求数量 *</span><input type="number" min="1" step="1" value={outboundQuantity} onChange={(event) => setOutboundQuantity(event.target.value)} /></label>
            </div>
            <div className="v2-form-actions"><button className="v2-button primary" type="submit" disabled={outboundLoading}>{outboundLoading ? "处理中…" : outboundOrder ? "重新建单" : "创建出库订单"}</button></div>
          </form>
          <div className="v2-panel v2-outbound-workflow">
            <h3>2. 分配合格库存</h3>
            <p className="v2-outbound-meta">{outboundOrder ? `${outboundOrder.order_no} · 需求 ${outboundOrder.required_quantity} 件` : "请先创建订单"}</p>
            <label><span>指定条码（可选）</span><textarea value={outboundBarcodes} onChange={(event) => setOutboundBarcodes(event.target.value)} placeholder="留空则按入库时间 FIFO；指定时每行一个条码" disabled={!outboundOrder || outboundLoading} /></label>
            <button className="v2-button primary wide" type="button" onClick={() => void allocateOutboundOrder()} disabled={!outboundOrder || outboundLoading}>{outboundLoading ? "处理中…" : "分配库存"}</button>
            {outboundAllocation && <div className="v2-code-list"><strong>已分配 {outboundAllocation.allocated_count} 件</strong>{outboundAllocation.allocations.map((item) => <span key={item.allocation_id}><b>{item.barcode}</b><small>货主 {item.owner_party_id}</small></span>)}</div>}
          </div>
          <div className="v2-panel v2-outbound-workflow">
            <h3>3. 扫码出库</h3>
            <label><span>出库批次号</span><input value={outboundShipmentNo} onChange={(event) => setOutboundShipmentNo(event.target.value)} placeholder="留空自动生成" disabled={!outboundAllocation || outboundLoading} /></label>
            <button className="v2-button primary wide" type="button" onClick={() => void shipOutboundOrder()} disabled={!outboundAllocation || outboundLoading}>{outboundLoading ? "处理中…" : "确认出库"}</button>
            {outboundShipment && <div className="v2-code-list"><strong>{outboundShipment.shipment_no} · 已出库 {outboundShipment.shipped_count} 件</strong>{outboundShipment.items.map((item) => <span key={item.shipment_line_id}><b>{item.barcode}</b><small>出库行已建立</small></span>)}</div>}
          </div>
          <div className="v2-panel v2-outbound-workflow">
            <h3>4. 交货确认或退回</h3>
            <label><span>上游确认码</span><input value={outboundConfirmationCode} onChange={(event) => setOutboundConfirmationCode(event.target.value)} placeholder="例如：签收单号 / 确认码" disabled={!outboundShipment || outboundLoading} /></label>
            <button className="v2-button primary wide" type="button" onClick={() => void confirmOutboundDelivery()} disabled={!outboundShipment || outboundLoading}>确认已交货</button>
            <label><span>退回原因</span><textarea value={outboundReturnReason} onChange={(event) => setOutboundReturnReason(event.target.value)} placeholder="退回后全部进入隔离区，复检通过前不可再次出库" disabled={!outboundShipment || outboundLoading} /></label>
            <button className="v2-button wide" type="button" onClick={() => void returnOutboundShipment()} disabled={!outboundShipment || outboundLoading}>登记退回并隔离</button>
          </div>
        </div>
        {outboundNotice && <div className={`v2-notice ${outboundNotice.type}`}>{outboundNotice.text}</div>}
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
        {page === "outbound" && renderOutbound()}
      </main>
    </div>
  );
}
