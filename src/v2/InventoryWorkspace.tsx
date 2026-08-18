import {
  type FormEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { confirm, open, save } from "@tauri-apps/plugin-dialog";
import {
  ArrowLeft,
  ArrowRight,
  ArrowDown,
  ArrowUp,
  Bell,
  Ban,
  Boxes,
  CheckCircle2,
  ChevronDown,
  ClipboardCheck,
  Copy,
  FileSpreadsheet,
  Gauge,
  LogOut,
  PackagePlus,
  Pencil,
  Plus,
  RefreshCw,
  RotateCcw,
  Search,
  Settings,
  SlidersHorizontal,
  ShieldAlert,
  Truck,
  Tags,
  Users,
  Warehouse,
  History,
  KeyRound,
  Download,
  Clock3,
  X,
  type LucideIcon,
} from "lucide-react";
import IdentityAdminPanel from "./IdentityAdminPanel";
import LegacyImportPanel from "./LegacyImportPanel";
import "./InventoryWorkspace.css";

type WorkspacePage = "overview" | "catalog" | "receipt" | "quality" | "inventory" | "lifecycle" | "records" | "returns" | "outbound" | "legacy-import" | "users" | "settings";
type WorkspaceMode = "offline" | "network";
type NavigationGroupId = "operations" | "inventory_data" | "catalog" | "system";
type SearchClearPreferenceKey = "inventory" | "lifecycle" | "records";
type CatalogTab = "products" | "parties";
type ReceiptStep = 1 | 2 | 3;
type QualityStep = 1 | 2;
type OutboundStep = 1 | 2 | 3;
type ReturnStep = "scan" | "confirm";
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
type QualityLabelDisposition = "available" | "quarantine";
type Notice = { type: "success" | "warning" | "error"; text: string };

interface WarrantyInput {
  duration_days: number;
  label_snapshot: string;
  starts_at: string | null;
}

interface WarrantyTerms {
  duration_days: number;
  label_snapshot: string;
  starts_at: string;
  expires_at: string;
}

export interface InventoryWorkspaceProps {
  onBackToLegacy?: () => void;
  onRequestActivation?: () => void;
  offlineActivated?: boolean;
  actorId?: string;
}

interface PostReceiptRequest {
  request_id: string;
  idempotency_key: string;
  receipt_no: string;
  owner_name: string;
  supplier_name: string;
  sku_code: string;
  sku_name: string;
  source_reference: string | null;
  received_at: string;
  actor_id: string;
  barcodes: string[];
  notes: string | null;
  warranty: WarrantyInput | null;
}

interface NetworkPostReceiptRequest {
  request_id: string;
  idempotency_key: string;
  receipt_no: string;
  owner_name: string;
  supplier_name: string;
  sku_code: string;
  sku_name: string;
  warehouse_id: string;
  source_reference: string | null;
  received_at: string;
  barcodes: string[];
  notes: string | null;
  warranty: WarrantyInput | null;
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

interface CatalogProduct {
  sku_id: string;
  code: string;
  name: string;
  serial_prefix: string | null;
  serial_forbidden_chars: string;
}

interface CatalogParty {
  party_id: string;
  display_name: string;
  roles: CatalogPartyRole[];
  contact_name: string | null;
  phone: string | null;
  wechat: string | null;
  email: string | null;
  address: string | null;
  notes: string | null;
}

interface ReferenceCatalog {
  products: CatalogProduct[];
  parties: CatalogParty[];
  goods_owners: CatalogParty[];
  suppliers: CatalogParty[];
}

interface SaveCatalogProductRequest {
  sku_id: string | null;
  code: string;
  name: string;
  serial_prefix: string | null;
  serial_forbidden_chars: string;
}

type CatalogPartyRole = "supplier" | "goods_owner" | "upstream_receiver" | "carrier";

interface SaveCatalogPartyRequest {
  party_id: string | null;
  display_name: string;
  roles: CatalogPartyRole[];
  contact_name: string | null;
  phone: string | null;
  wechat: string | null;
  email: string | null;
  address: string | null;
  notes: string | null;
}

interface InspectionResultInput {
  barcode: string;
  outcome: QualityOutcome;
  quality_label_id: string | null;
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

interface NetworkCompleteInspectionRequest {
  request_id: string;
  idempotency_key: string;
  inspection_no: string;
  inspection_kind: InspectionKind;
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

interface QualityLabel {
  quality_label_id: string;
  name: string;
  disposition: QualityLabelDisposition;
  active: boolean;
  usage_count: number;
  name_history: QualityLabelNameHistory[];
  created_at: string;
  updated_at: string;
}

interface QualityLabelNameHistory {
  history_id: string;
  old_name: string;
  new_name: string;
  changed_by: string;
  change_note: string | null;
  changed_at: string;
}

interface SaveQualityLabelRequest {
  quality_label_id: string | null;
  name: string;
  disposition: QualityLabelDisposition;
  active: boolean;
  rename_note: string | null;
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

interface InventoryBarcodeExistsResponse {
  barcode: string;
  exists: boolean;
}

interface InventoryTrace {
  inventory_unit_id: string;
  barcode: string;
  owner_party_id: string;
  owner_name: string;
  sku_id: string;
  sku_code: string;
  sku_name: string;
  receipt_id: string;
  receipt_no: string;
  supplier_name: string | null;
  source_reference: string | null;
  received_at: string;
  inbound_warranty: WarrantyTerms | null;
  inventory_status: InventoryStatus;
  quality_status: QualityStatus;
  inspections: Array<{
    inspection_no: string;
    inspection_type: InspectionKind;
    result: QualityOutcome;
    quality_label_id: string | null;
    quality_label_snapshot: string | null;
    inspected_at: string;
    defect_code: string | null;
    notes: string | null;
  }>;
  movements: Array<{
    movement_id: string;
    movement_type: string;
    source_type: string;
    source_id: string;
    source_reference: string | null;
    occurred_at: string;
  }>;
  outbound: Array<{
    allocation_id: string;
    allocation_status: string;
    allocated_at: string;
    order_id: string;
    order_no: string;
    order_status: string;
    upstream_receiver_name: string;
    shipment_line_id: string | null;
    shipment_id: string | null;
    shipment_no: string | null;
    shipped_at: string | null;
    warranty: WarrantyTerms | null;
    confirmation_code: string | null;
    confirmed_at: string | null;
    delivery_result: string | null;
    return_no: string | null;
    returned_at: string | null;
    return_reason: string | null;
    return_disposition: string | null;
  }>;
  latest_related_order: InventoryTrace["outbound"][number] | null;
}

interface LifecycleEvent {
  key: string;
  occurredAt: string;
  className: string;
  title: string;
  details: string[];
  sequence: number;
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

interface InventorySupplierStockSummary {
  supplier_party_id: string | null;
  supplier_name: string;
  on_hand_units: number;
  inventory: InventoryStatusSummary;
}

interface InventoryProductStockSummary {
  sku_id: string;
  sku_code: string;
  sku_name: string;
  on_hand_units: number;
  inventory: InventoryStatusSummary;
  suppliers: InventorySupplierStockSummary[];
}

interface DashboardDto {
  total_units: number;
  inventory: InventoryStatusSummary;
  quality: QualityStatusSummary;
  products: InventoryProductStockSummary[];
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

interface NetworkCreateOutboundOrderRequest {
  request_id: string;
  idempotency_key: string;
  order_no: string;
  upstream_receiver_name: string;
  sku_code: string;
  sku_name: string;
  required_quantity: number;
  required_at: string | null;
}

interface RenameOutboundOrderRequest {
  request_id: string;
  idempotency_key: string;
  order_id: string;
  upstream_receiver_name: string;
  actor_id: string;
}

interface NetworkRenameOutboundOrderRequest {
  request_id: string;
  idempotency_key: string;
  order_id: string;
  upstream_receiver_name: string;
}

interface NetworkAllocateOutboundRequest {
  request_id: string;
  idempotency_key: string;
  order_id: string;
  order_line_id: string;
  barcodes: string[];
  allow_mixed_skus: boolean;
}

interface NetworkShipOutboundRequest {
  request_id: string;
  idempotency_key: string;
  order_id: string;
  shipment_no: string;
  allocation_ids: string[];
  barcodes: string[];
  shipped_at: string;
  warranty: WarrantyInput | null;
}

interface NetworkConfirmOutboundDeliveryRequest {
  request_id: string;
  idempotency_key: string;
  shipment_id: string;
  confirmation_code: string;
  shipment_line_ids: string[];
  confirmed_at: string;
  notes: string | null;
}

interface NetworkReturnOutboundShipmentRequest {
  request_id: string;
  idempotency_key: string;
  shipment_id: string;
  shipment_line_ids: string[];
  return_no: string;
  returned_at: string;
  reason: string;
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

interface RenameOutboundOrderResponse {
  order_id: string;
  order_no: string;
  receiver_name: string;
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

interface OutboundScannedItem {
  barcode: string;
  skuId: string;
  skuCode: string;
  skuName: string;
}

interface OutboundScanGroup {
  skuId: string;
  skuCode: string;
  skuName: string;
  count: number;
  barcodes: string[];
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

interface ReceiptRecord {
  receipt_id: string;
  receipt_no: string;
  supplier_name: string | null;
  owner_name: string;
  source_reference: string | null;
  received_at: string;
  status: string;
  item_count: number;
  warranty: WarrantyTerms | null;
}

interface OutboundOrderRecord {
  order_id: string;
  order_no: string;
  receiver_name: string;
  status: string;
  created_at: string;
  latest_shipment_no: string | null;
  latest_shipped_at: string | null;
  item_count: number;
  returned_count: number;
}

interface DocumentItem {
  sku_code: string;
  sku_name: string;
  barcode: string;
  inventory_status: InventoryStatus;
  allocation_status: string | null;
  owner_name: string | null;
  shipment_id: string | null;
  shipment_line_id: string | null;
  shipment_no: string | null;
  shipped_at: string | null;
  warranty: WarrantyTerms | null;
  return_no: string | null;
  returned_at: string | null;
  return_reason: string | null;
  return_disposition: string | null;
}

interface DocumentVoidInfo {
  reason: string;
  actor_id: string;
  voided_at: string;
}

interface DocumentVoidEligibility {
  can_void: boolean;
  blockers: string[];
}

interface ReceiptDocument {
  receipt_id: string;
  receipt_no: string;
  supplier_name: string | null;
  owner_name: string;
  source_reference: string | null;
  received_at: string;
  status: string;
  item_count: number;
  warranty: WarrantyTerms | null;
  items: DocumentItem[];
  void_info: DocumentVoidInfo | null;
  void_eligibility: DocumentVoidEligibility;
}

interface OutboundOrderDocument {
  order_id: string;
  order_no: string;
  receiver_name: string;
  status: string;
  created_at: string;
  latest_shipment_no: string | null;
  latest_shipped_at: string | null;
  item_count: number;
  returned_count: number;
  items: DocumentItem[];
  void_info: DocumentVoidInfo | null;
  void_eligibility: DocumentVoidEligibility;
}

interface RenameOutboundDialogState {
  orderId: string;
  orderNo: string;
  currentName: string;
}

interface VoidDocumentResponse {
  document_id: string;
  document_no: string;
  document_kind: "inbound_receipt" | "outbound_order";
  status: "voided";
  voided_at: string;
  voided_inventory_count: number;
  released_inventory_count: number;
  quarantined_inventory_count: number;
  idempotent_replay: boolean;
}

interface CopyDocumentSnResponse {
  document_id: string;
  document_no: string;
  document_kind: "inbound_receipt" | "outbound_order";
  barcodes: string[];
}

interface VoidDialogState {
  kind: "receipt" | "outbound";
  documentId: string;
  documentNo: string;
  itemCount: number;
  blockers: string[];
}

interface SnCopyDialogState {
  kind: "receipt" | "outbound";
  documentId: string;
  documentNo: string;
  itemCount: number;
}

interface ReturnCandidate {
  barcode: string;
  inventory_unit_id: string;
  shipment_id: string;
  shipment_line_id: string;
  shipment_no: string;
  shipped_at: string;
  order_id: string;
  order_no: string;
  receiver_name: string;
  warranty: WarrantyTerms | null;
}

interface NetworkStatus {
  configured: boolean;
  base_url: string | null;
  authenticated: boolean;
  tenant_id: string | null;
  user_id: string | null;
  session_expires_in_seconds: number | null;
}

interface NetworkWarehouse {
  warehouse_id: string;
  warehouse_code: string;
  warehouse_name: string;
  receiving_location_id: string;
  receiving_location_code: string;
  receiving_location_name: string;
}

interface BackupMetadata {
  source_instance_id: string;
  source_workspace_id: string;
  exported_at: string;
  database_bytes: number;
  database_sha256: string;
}

interface RestoreReport {
  status: "restored" | "failed";
  requested_at: string;
  completed_at: string;
  source_workspace_id: string | null;
  backup_exported_at: string | null;
  pre_restore_backup: string | null;
  error: string | null;
}

interface UpgradeExportOutput {
  path: string;
  export_id: string;
  checksum: string;
}

interface UpgradeImportOutput {
  import: {
    status: "imported" | "already_imported";
    export_id: string;
    migration_id: string;
    checksum: string;
    imported_at: string | null;
    entity_counts: Record<string, number>;
  };
  local_archived: boolean;
}

interface NavigationItem {
  id: WorkspacePage;
  label: string;
  description: string;
  icon: LucideIcon;
  group: NavigationGroupId | null;
  mode?: WorkspaceMode;
}

interface NavigationGroup {
  id: NavigationGroupId;
  label: string;
  icon: LucideIcon;
}

const navigationGroups: NavigationGroup[] = [
  { id: "operations", label: "日常作业", icon: ClipboardCheck },
  { id: "inventory_data", label: "库存与数据", icon: Boxes },
  { id: "catalog", label: "基础资料", icon: Tags },
  { id: "system", label: "系统管理", icon: Settings },
];

const navigationItems: NavigationItem[] = [
  { id: "overview", label: "概览", description: "库存与质检态势", icon: Gauge, group: null },
  { id: "receipt", label: "入库", description: "扫码收货", icon: PackagePlus, group: "operations" },
  { id: "quality", label: "质检", description: "初检与复检", icon: ClipboardCheck, group: "operations" },
  { id: "outbound", label: "出库", description: "凑单、交货与退回", icon: Truck, group: "operations" },
  { id: "returns", label: "扫码退货", description: "按批定位原订单", icon: RotateCcw, group: "operations" },
  { id: "inventory", label: "库存查询", description: "单件库存与追溯", icon: Boxes, group: "inventory_data" },
  { id: "lifecycle", label: "生命周期", description: "完整历史与最近订单", icon: History, group: "inventory_data" },
  { id: "records", label: "单据查询", description: "收货单与出库订单", icon: FileSpreadsheet, group: "inventory_data" },
  { id: "legacy-import", label: "Excel 导入", description: "历史数据迁移", icon: FileSpreadsheet, group: "inventory_data", mode: "offline" },
  { id: "catalog", label: "资料维护", description: "商品与往来方", icon: Tags, group: "catalog" },
  { id: "users", label: "用户与角色", description: "账号和权限", icon: Users, group: "system", mode: "network" },
  { id: "settings", label: "数据与设置", description: "备份、恢复和升级", icon: Settings, group: "system" },
];

const defaultOverviewShortcuts: WorkspacePage[] = ["receipt", "quality", "inventory", "outbound"];
const overviewShortcutLimit = 6;
const searchClearPreferencesStorageKey = "inventory-v2-clear-search-after-enter";

const defaultSearchClearPreferences: Record<SearchClearPreferenceKey, boolean> = {
  inventory: false,
  lifecycle: false,
  records: false,
};

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

const catalogPartyRoleLabels: Record<CatalogPartyRole, string> = {
  supplier: "供应商",
  goods_owner: "货主",
  upstream_receiver: "客户",
  carrier: "承运商",
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

function makeDocumentNumber(prefix: "RK" | "ZJ" | "CK" | "TH" | "DD"): string {
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

const warrantyPresetLabels: Record<string, string> = {
  "7": "一个星期",
  "15": "半个月",
  "30": "一个月",
  "365": "一年",
};

function makeWarrantyInput(preset: string, customDays: string, manualStart: boolean, startsAt: string): WarrantyInput | null {
  if (!preset) return null;
  const duration = Number(preset === "custom" ? customDays : preset);
  if (!Number.isInteger(duration) || duration < 1 || duration > 36500) {
    throw new Error("自定义质保期限必须是 1 到 36500 天的整数");
  }
  return {
    duration_days: duration,
    label_snapshot: preset === "custom" ? `自定义 ${duration} 天` : warrantyPresetLabels[preset],
    starts_at: manualStart ? toUtcIso(startsAt) : null,
  };
}

function warrantyStatus(terms: WarrantyTerms | null): { label: string; className: string } {
  if (!terms) return { label: "无质保", className: "none" };
  const remaining = new Date(terms.expires_at).getTime() - Date.now();
  if (remaining < 0) return { label: "已过期", className: "expired" };
  if (remaining <= 30 * 24 * 60 * 60 * 1000) return { label: "即将到期", className: "warning" };
  return { label: "有效", className: "active" };
}

function warrantyDescription(terms: WarrantyTerms | null): string {
  if (!terms) return "无质保";
  return `${terms.label_snapshot} · ${formatDateTime(terms.starts_at)} 至 ${formatDateTime(terms.expires_at)}`;
}

const movementLabels: Record<string, string> = {
  moved: "库位移动",
  reservation_released: "解除订单预留",
  scrapped: "库存报损",
  returned_to_owner: "退还货主",
  voided: "库存作废",
  corrected: "库存更正",
};

const movementSourceLabels: Record<string, string> = {
  inbound_receipt: "收货单",
  outbound_order_line: "出库订单",
  outbound_shipment: "出库单",
  delivery_confirmation: "签收确认",
  outbound_return_batch: "退货单",
  document_void: "单据作废",
};

const documentStatusLabels: Record<string, string> = {
  draft: "草稿",
  posted: "已入库",
  open: "待分配",
  partially_allocated: "部分分配",
  allocated: "已分配",
  partially_shipped: "部分出库",
  shipped: "已出库",
  completed: "已完成",
  voided: "已作废",
};

function documentStatusLabel(status: string): string {
  return documentStatusLabels[status] ?? status;
}

function buildLifecycleEvents(
  trace: InventoryTrace,
  qualityLabels: Record<QualityStatus, string>,
): LifecycleEvent[] {
  const events: LifecycleEvent[] = [{
    key: `receipt-${trace.receipt_id}`,
    occurredAt: trace.received_at,
    className: "received",
    title: `入库 · ${trace.receipt_no}`,
    details: [
      `${trace.supplier_name ?? "供应商未记录"} → ${trace.owner_name}`,
      `来源单号：${trace.source_reference ?? "未记录"}`,
      `供应方质保：${warrantyDescription(trace.inbound_warranty)}`,
    ],
    sequence: 0,
  }];

  trace.inspections.forEach((inspection, index) => {
    events.push({
      key: `inspection-${inspection.inspection_no}-${inspection.inspected_at}`,
      occurredAt: inspection.inspected_at,
      className: inspection.result === "passed" ? "success" : "warning",
      title: `${inspection.inspection_type === "initial" ? "初检" : "复检"} · ${inspection.inspection_no}`,
      details: [
        inspection.quality_label_snapshot ?? qualityLabels[inspection.result],
        ...(inspection.defect_code ? [`缺陷：${inspection.defect_code}`] : []),
        ...(inspection.notes ? [`备注：${inspection.notes}`] : []),
      ],
      sequence: 100 + index,
    });
  });

  trace.outbound.forEach((outbound, index) => {
    const baseDetails = [`客户：${outbound.upstream_receiver_name}`, `订单状态：${outbound.order_status}`];
    events.push({
      key: `allocation-${outbound.allocation_id}`,
      occurredAt: outbound.allocated_at,
      className: "reserved",
      title: `订单分配 · ${outbound.order_no}`,
      details: [...baseDetails, `分配状态：${outbound.allocation_status}`],
      sequence: 200 + index * 10,
    });
    if (outbound.shipment_no && outbound.shipped_at) {
      events.push({
        key: `shipment-${outbound.shipment_line_id ?? outbound.allocation_id}`,
        occurredAt: outbound.shipped_at,
        className: "shipped",
        title: `出库 · ${outbound.shipment_no}`,
        details: [...baseDetails, `客户质保：${warrantyDescription(outbound.warranty)}`],
        sequence: 201 + index * 10,
      });
    }
    if (outbound.confirmation_code && outbound.confirmed_at) {
      events.push({
        key: `confirmation-${outbound.shipment_line_id ?? outbound.allocation_id}`,
        occurredAt: outbound.confirmed_at,
        className: "success",
        title: `签收确认 · ${outbound.confirmation_code}`,
        details: [...baseDetails, `签收结果：${outbound.delivery_result ?? "已确认"}`],
        sequence: 202 + index * 10,
      });
    }
    if (outbound.return_no && outbound.returned_at) {
      events.push({
        key: `return-${outbound.shipment_line_id ?? outbound.allocation_id}`,
        occurredAt: outbound.returned_at,
        className: "returned",
        title: `退货 · ${outbound.return_no}`,
        details: [
          ...baseDetails,
          `退货原因：${outbound.return_reason ?? "未记录"}`,
          `处理方式：${outbound.return_disposition ?? "未记录"}`,
        ],
        sequence: 203 + index * 10,
      });
    }
  });

  const representedMovements = new Set(["received", "reserved", "shipped", "delivered", "returned"]);
  trace.movements.forEach((movement, index) => {
    if (representedMovements.has(movement.movement_type)) return;
    const sourceLabel = movementSourceLabels[movement.source_type] ?? movement.source_type;
    events.push({
      key: `movement-${movement.movement_id}`,
      occurredAt: movement.occurred_at,
      className: ["scrapped", "voided", "returned_to_owner"].includes(movement.movement_type) ? "warning" : "muted",
      title: movementLabels[movement.movement_type] ?? `库存变动 · ${movement.movement_type}`,
      details: [`来源：${sourceLabel}`, `关联记录：${movement.source_reference ?? movement.source_id}`],
      sequence: 300 + index,
    });
  });

  return events.sort((left, right) => {
    const leftTime = Date.parse(left.occurredAt);
    const rightTime = Date.parse(right.occurredAt);
    if (!Number.isNaN(leftTime) && !Number.isNaN(rightTime) && leftTime !== rightTime) {
      return leftTime - rightTime;
    }
    const timestampOrder = left.occurredAt.localeCompare(right.occurredAt);
    return timestampOrder || left.sequence - right.sequence;
  });
}

function getDefaultActorId(): string {
  const storageKey = "inventory-v2-offline-actor-id";
  const existing = window.localStorage.getItem(storageKey);
  if (existing) return existing;
  const created = createId();
  window.localStorage.setItem(storageKey, created);
  return created;
}

function getStoredNetworkValue(key: string): string {
  try {
    return window.localStorage.getItem(key) ?? "";
  } catch {
    return "";
  }
}

function getStoredSearchClearPreferences(): Record<SearchClearPreferenceKey, boolean> {
  try {
    const parsed: unknown = JSON.parse(window.localStorage.getItem(searchClearPreferencesStorageKey) ?? "null");
    if (!parsed || typeof parsed !== "object") return { ...defaultSearchClearPreferences };
    const values = parsed as Partial<Record<SearchClearPreferenceKey, unknown>>;
    return {
      inventory: values.inventory === true,
      lifecycle: values.lifecycle === true,
      records: values.records === true,
    };
  } catch {
    return { ...defaultSearchClearPreferences };
  }
}

function overviewShortcutStorageKey(mode: WorkspaceMode): string {
  return `inventory-v2-overview-shortcuts-${mode}`;
}

function availableOverviewShortcutIds(mode: WorkspaceMode): Set<WorkspacePage> {
  return new Set(
    navigationItems
      .filter((item) => item.id !== "overview" && (!item.mode || item.mode === mode))
      .map((item) => item.id),
  );
}

function getStoredOverviewShortcuts(mode: WorkspaceMode): WorkspacePage[] {
  const available = availableOverviewShortcutIds(mode);
  const fallback = defaultOverviewShortcuts.filter((pageId) => available.has(pageId));
  try {
    const parsed: unknown = JSON.parse(window.localStorage.getItem(overviewShortcutStorageKey(mode)) ?? "null");
    if (!Array.isArray(parsed)) return fallback;
    const unique = parsed.filter((value, index): value is WorkspacePage => (
      typeof value === "string"
      && available.has(value as WorkspacePage)
      && parsed.indexOf(value) === index
    )).slice(0, overviewShortcutLimit);
    return unique.length > 0 ? unique : fallback;
  } catch {
    return fallback;
  }
}

function getNetworkDeviceId(): string {
  const storageKey = "inventory-v2-network-device-id";
  const existing = getStoredNetworkValue(storageKey);
  if (existing) return existing;
  const created = createId();
  try {
    window.localStorage.setItem(storageKey, created);
  } catch {
    // The server accepts a fresh device id when local storage is unavailable.
  }
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

function parseForbiddenSerialTokens(value: string): string[] {
  return value.split(",").flatMap((token) => {
    if (token === " ") return [" "];
    const normalized = token.trim().toUpperCase();
    return normalized ? [normalized] : [];
  });
}

function parseBarcodeLines(value: string): string[] {
  return value
    .split(/\r?\n/)
    .map((barcode) => barcode.trim().toUpperCase())
    .filter(Boolean);
}

function isInspectionEligible(
  item: Pick<InventoryListItem, "inventory_status" | "quality_status">,
  kind: InspectionKind,
): boolean {
  if (kind === "initial") {
    return item.inventory_status === "received" && item.quality_status === "untested";
  }
  return item.inventory_status === "quarantined"
    && (item.quality_status === "failed" || item.quality_status === "passed" || item.quality_status === "waived");
}

export default function InventoryWorkspace({
  onBackToLegacy,
  onRequestActivation,
  offlineActivated = true,
  actorId,
}: InventoryWorkspaceProps) {
  const [page, setPage] = useState<WorkspacePage>("overview");
  const [mode, setMode] = useState<WorkspaceMode>(offlineActivated ? "offline" : "network");
  const [expandedNavGroups, setExpandedNavGroups] = useState<Set<NavigationGroupId>>(() => new Set(["operations"]));
  const [resolvedActorId] = useState(() => actorId?.trim() || getDefaultActorId());
  const workspaceContextRef = useRef(0);

  const [networkStatus, setNetworkStatus] = useState<NetworkStatus | null>(null);
  const [networkBaseUrl, setNetworkBaseUrl] = useState(() => getStoredNetworkValue("inventory-v2-network-url"));
  const [networkTenantId, setNetworkTenantId] = useState(() => getStoredNetworkValue("inventory-v2-network-tenant"));
  const [networkLogin, setNetworkLogin] = useState("");
  const [networkPassword, setNetworkPassword] = useState("");
  const [networkWarehouseId, setNetworkWarehouseId] = useState(() => getStoredNetworkValue("inventory-v2-network-warehouse"));
  const [networkWarehouses, setNetworkWarehouses] = useState<NetworkWarehouse[]>([]);
  const [networkWarehousesLoading, setNetworkWarehousesLoading] = useState(false);
  const [networkWarehousesError, setNetworkWarehousesError] = useState<string | null>(null);
  const [networkAuthLoading, setNetworkAuthLoading] = useState(false);
  const [networkAuthNotice, setNetworkAuthNotice] = useState<Notice | null>(null);

  const [dashboard, setDashboard] = useState<DashboardDto | null>(null);
  const [dashboardLoading, setDashboardLoading] = useState(false);
  const [dashboardError, setDashboardError] = useState<string | null>(null);
  const [selectedOverviewSkuId, setSelectedOverviewSkuId] = useState("");
  const [overviewShortcutPreferences, setOverviewShortcutPreferences] = useState<Record<WorkspaceMode, WorkspacePage[]>>(() => ({
    offline: getStoredOverviewShortcuts("offline"),
    network: getStoredOverviewShortcuts("network"),
  }));
  const [overviewShortcutEditorOpen, setOverviewShortcutEditorOpen] = useState(false);
  const [overviewShortcutDraft, setOverviewShortcutDraft] = useState<WorkspacePage[]>([]);
  const [searchClearPreferences, setSearchClearPreferences] = useState<Record<SearchClearPreferenceKey, boolean>>(() => getStoredSearchClearPreferences());

  const [catalog, setCatalog] = useState<ReferenceCatalog | null>(null);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [catalogNotice, setCatalogNotice] = useState<Notice | null>(null);
  const [catalogTab, setCatalogTab] = useState<CatalogTab>("products");
  const [catalogCreateOpen, setCatalogCreateOpen] = useState(false);
  const [newProductCode, setNewProductCode] = useState("");
  const [newProductName, setNewProductName] = useState("");
  const [newProductSerialPrefix, setNewProductSerialPrefix] = useState("");
  const [newProductForbiddenChars, setNewProductForbiddenChars] = useState("-, ");
  const [editingProductId, setEditingProductId] = useState<string | null>(null);
  const [editingPartyId, setEditingPartyId] = useState<string | null>(null);
  const [newPartyName, setNewPartyName] = useState("");
  const [newPartyRoles, setNewPartyRoles] = useState<Set<CatalogPartyRole>>(() => new Set(["supplier"]));
  const [newPartyContactName, setNewPartyContactName] = useState("");
  const [newPartyPhone, setNewPartyPhone] = useState("");
  const [newPartyWechat, setNewPartyWechat] = useState("");
  const [newPartyEmail, setNewPartyEmail] = useState("");
  const [newPartyAddress, setNewPartyAddress] = useState("");
  const [newPartyNotes, setNewPartyNotes] = useState("");

  const [supplierName, setSupplierName] = useState("");
  const [receiptProductInput, setReceiptProductInput] = useState("");
  const [receiptProductSuggestionsOpen, setReceiptProductSuggestionsOpen] = useState(false);
  const [receiptSupplierSuggestionsOpen, setReceiptSupplierSuggestionsOpen] = useState(false);
  const [selectedProductId, setSelectedProductId] = useState("");
  const [receivedAt, setReceivedAt] = useState(getLocalDateTimeValue);
  const [sourceReference, setSourceReference] = useState("");
  const [scannerInput, setScannerInput] = useState("");
  const [receiptBulkInput, setReceiptBulkInput] = useState("");
  const [scannedBarcodes, setScannedBarcodes] = useState<string[]>([]);
  const scannerInputRef = useRef<HTMLInputElement>(null);
  const scanCheckingRef = useRef(false);
  const [scanChecking, setScanChecking] = useState(false);
  const [receiptLoading, setReceiptLoading] = useState(false);
  const [receiptNotice, setReceiptNotice] = useState<Notice | null>(null);
  const [receiptStep, setReceiptStep] = useState<ReceiptStep>(1);
  const [receiptCompleted, setReceiptCompleted] = useState<PostReceiptResponse | null>(null);
  const [receiptWarrantyPreset, setReceiptWarrantyPreset] = useState("");
  const [receiptWarrantyCustomDays, setReceiptWarrantyCustomDays] = useState("");
  const [receiptWarrantyManualStart, setReceiptWarrantyManualStart] = useState(false);
  const [receiptWarrantyStartsAt, setReceiptWarrantyStartsAt] = useState(getLocalDateTimeValue);

  const [qualityItems, setQualityItems] = useState<InventoryListItem[]>([]);
  const [qualityLoading, setQualityLoading] = useState(false);
  const [qualityNotice, setQualityNotice] = useState<Notice | null>(null);
  const [qualityStep, setQualityStep] = useState<QualityStep>(1);
  const [qualityLabels, setQualityLabels] = useState<QualityLabel[]>([]);
  const [qualityLabelsLoading, setQualityLabelsLoading] = useState(false);
  const [qualityLabelModalOpen, setQualityLabelModalOpen] = useState(false);
  const [qualityLabelNotice, setQualityLabelNotice] = useState<Notice | null>(null);
  const [editingQualityLabelId, setEditingQualityLabelId] = useState<string | null>(null);
  const [qualityLabelName, setQualityLabelName] = useState("");
  const [qualityLabelDisposition, setQualityLabelDisposition] = useState<QualityLabelDisposition>("available");
  const [qualityLabelActive, setQualityLabelActive] = useState(true);
  const [qualityLabelRenameNote, setQualityLabelRenameNote] = useState("");
  const [inspectionQualityLabelId, setInspectionQualityLabelId] = useState("");
  const [selectedBarcodes, setSelectedBarcodes] = useState<Set<string>>(() => new Set());
  const [qualityScannerInput, setQualityScannerInput] = useState("");
  const [qualityBulkInput, setQualityBulkInput] = useState("");
  const [qualityScanNotice, setQualityScanNotice] = useState<Notice | null>(null);
  const [qualityScanChecking, setQualityScanChecking] = useState(false);
  const qualityScannerInputRef = useRef<HTMLInputElement>(null);
  const qualityScanCheckingRef = useRef(false);
  const [inspectionKind, setInspectionKind] = useState<InspectionKind>("initial");
  const [defectCode, setDefectCode] = useState("");
  const [inspectionNotes, setInspectionNotes] = useState("");

  const [inventoryItems, setInventoryItems] = useState<InventoryListItem[]>([]);
  const [inventoryTotal, setInventoryTotal] = useState(0);
  const [inventoryLoading, setInventoryLoading] = useState(false);
  const [inventoryError, setInventoryError] = useState<string | null>(null);
  const [inventoryTrace, setInventoryTrace] = useState<InventoryTrace | null>(null);
  const [inventoryTraceBarcode, setInventoryTraceBarcode] = useState<string | null>(null);
  const [inventoryTraceLoading, setInventoryTraceLoading] = useState(false);
  const [inventoryTraceError, setInventoryTraceError] = useState<string | null>(null);
  const [inventorySearch, setInventorySearch] = useState("");
  const [inventoryStatus, setInventoryStatus] = useState<InventoryStatus | "">("");
  const [qualityStatus, setQualityStatus] = useState<QualityStatus | "">("");
  const [lifecycleSearch, setLifecycleSearch] = useState("");
  const inventorySearchRef = useRef(inventorySearch);
  const inventoryStatusRef = useRef(inventoryStatus);
  const qualityStatusRef = useRef(qualityStatus);
  const lifecycleSearchRef = useRef(lifecycleSearch);
  const returnBarcodeRef = useRef("");
  inventorySearchRef.current = inventorySearch;
  inventoryStatusRef.current = inventoryStatus;
  qualityStatusRef.current = qualityStatus;
  lifecycleSearchRef.current = lifecycleSearch;

  const [outboundReceiver, setOutboundReceiver] = useState("");
  const [outboundReceiverSuggestionsOpen, setOutboundReceiverSuggestionsOpen] = useState(false);
  const [outboundScannerInput, setOutboundScannerInput] = useState("");
  const [outboundBulkInput, setOutboundBulkInput] = useState("");
  const [outboundScannedItems, setOutboundScannedItems] = useState<OutboundScannedItem[]>([]);
  const [outboundScanNotice, setOutboundScanNotice] = useState<Notice | null>(null);
  const outboundScannerInputRef = useRef<HTMLInputElement>(null);
  const outboundScanCheckingRef = useRef(false);
  const [outboundScanChecking, setOutboundScanChecking] = useState(false);
  const [outboundShipmentNo, setOutboundShipmentNo] = useState("");
  const [outboundConfirmationCode, setOutboundConfirmationCode] = useState("");
  const [outboundNotice, setOutboundNotice] = useState<Notice | null>(null);
  const [outboundLoading, setOutboundLoading] = useState(false);
  const [outboundOrder, setOutboundOrder] = useState<CreateOutboundOrderResponse | null>(null);
  const [outboundAllocation, setOutboundAllocation] = useState<AllocateOutboundResponse | null>(null);
  const [outboundShipment, setOutboundShipment] = useState<ShipOutboundResponse | null>(null);
  const [outboundResolved, setOutboundResolved] = useState(false);
  const [outboundStep, setOutboundStep] = useState<OutboundStep>(1);
  const [outboundWarrantyPreset, setOutboundWarrantyPreset] = useState("");
  const [outboundWarrantyCustomDays, setOutboundWarrantyCustomDays] = useState("");
  const [outboundWarrantyManualStart, setOutboundWarrantyManualStart] = useState(false);
  const [outboundWarrantyStartsAt, setOutboundWarrantyStartsAt] = useState(getLocalDateTimeValue);

  const [recordTab, setRecordTab] = useState<"receipt" | "outbound">("outbound");
  const [recordSearch, setRecordSearch] = useState("");
  const recordSearchRef = useRef(recordSearch);
  recordSearchRef.current = recordSearch;
  const [receiptRecords, setReceiptRecords] = useState<ReceiptRecord[]>([]);
  const [outboundRecords, setOutboundRecords] = useState<OutboundOrderRecord[]>([]);
  const [recordLoading, setRecordLoading] = useState(false);
  const [recordNotice, setRecordNotice] = useState<Notice | null>(null);
  const [selectedReceiptDocument, setSelectedReceiptDocument] = useState<ReceiptDocument | null>(null);
  const [selectedOutboundDocument, setSelectedOutboundDocument] = useState<OutboundOrderDocument | null>(null);
  const [renameOutboundDialog, setRenameOutboundDialog] = useState<RenameOutboundDialogState | null>(null);
  const [renameOutboundName, setRenameOutboundName] = useState("");
  const [renameOutboundLoading, setRenameOutboundLoading] = useState(false);
  const [voidDialog, setVoidDialog] = useState<VoidDialogState | null>(null);
  const [voidReason, setVoidReason] = useState("");
  const [voidPassword, setVoidPassword] = useState("");
  const [voidLoading, setVoidLoading] = useState(false);
  const [snCopyDialog, setSnCopyDialog] = useState<SnCopyDialogState | null>(null);
  const [snCopyPassword, setSnCopyPassword] = useState("");
  const [snCopyLoading, setSnCopyLoading] = useState(false);
  const [operationCurrentPassword, setOperationCurrentPassword] = useState("");
  const [operationNewPassword, setOperationNewPassword] = useState("");
  const [operationConfirmPassword, setOperationConfirmPassword] = useState("");
  const [operationPasswordLoading, setOperationPasswordLoading] = useState(false);
  const [operationPasswordNotice, setOperationPasswordNotice] = useState<Notice | null>(null);

  const [returnBarcode, setReturnBarcode] = useState("");
  const [returnCandidates, setReturnCandidates] = useState<ReturnCandidate[]>([]);
  const [returnStep, setReturnStep] = useState<ReturnStep>("scan");
  const [returnReason, setReturnReason] = useState("");
  const [returnLoading, setReturnLoading] = useState(false);
  const [returnNotice, setReturnNotice] = useState<Notice | null>(null);
  const returnScannerRef = useRef<HTMLInputElement>(null);
  returnBarcodeRef.current = returnBarcode;

  const [dataOperationLoading, setDataOperationLoading] = useState(false);
  const dataOperationRef = useRef(false);
  const [childPanelBusy, setChildPanelBusy] = useState(false);
  const childPanelBusyRef = useRef(false);
  const [dataNotice, setDataNotice] = useState<Notice | null>(null);
  const [restoreReport, setRestoreReport] = useState<RestoreReport | null>(null);
  const [upgradePackagePath, setUpgradePackagePath] = useState("");
  const [upgradeTargetWorkspaceId, setUpgradeTargetWorkspaceId] = useState("");
  const [upgradeExport, setUpgradeExport] = useState<UpgradeExportOutput | null>(null);
  const [upgradeImport, setUpgradeImport] = useState<UpgradeImportOutput | null>(null);

  const selectedProduct = useMemo(
    () => catalog?.products.find((product) => product.sku_id === selectedProductId) ?? null,
    [catalog, selectedProductId],
  );
  const selectedSupplier = useMemo(() => {
    const normalized = supplierName.trim().toLocaleLowerCase();
    return catalog?.suppliers.find((party) => party.display_name.toLocaleLowerCase() === normalized) ?? null;
  }, [catalog, supplierName]);
  const receiptProductSuggestions = useMemo(() => {
    const query = receiptProductInput.trim().toLocaleLowerCase();
    const products = catalog?.products ?? [];
    if (!query) return products.slice(0, 8);
    return products
      .filter((product) => `${product.code} ${product.name}`.toLocaleLowerCase().includes(query))
      .slice(0, 8);
  }, [catalog, receiptProductInput]);
  const receiptSupplierSuggestions = useMemo(() => {
    const query = supplierName.trim().toLocaleLowerCase();
    const suppliers = catalog?.suppliers ?? [];
    if (!query) return suppliers.slice(0, 8);
    return suppliers
      .filter((party) => party.display_name.toLocaleLowerCase().includes(query))
      .slice(0, 8);
  }, [catalog, supplierName]);
  const receiptMissingDetails = [
    !selectedProduct ? "商品" : null,
    !selectedSupplier ? "供应商" : null,
    !receivedAt ? "入库时间" : null,
    mode === "network" && !networkWarehouses.some((warehouse) => warehouse.warehouse_id === networkWarehouseId)
      ? "入库仓库"
      : null,
  ].filter((value): value is string => Boolean(value));
  const receiptDetailsReady = receiptMissingDetails.length === 0;
  const barcodes = scannedBarcodes;
  const displayedQualityStatusLabels = qualityStatusLabels;
  const activeQualityLabels = useMemo(
    () => qualityLabels.filter((label) => label.active),
    [qualityLabels],
  );
  const selectedQualityLabel = useMemo(
    () => activeQualityLabels.find((label) => label.quality_label_id === inspectionQualityLabelId) ?? null,
    [activeQualityLabels, inspectionQualityLabelId],
  );
  const editingQualityLabel = useMemo(
    () => qualityLabels.find((label) => label.quality_label_id === editingQualityLabelId) ?? null,
    [editingQualityLabelId, qualityLabels],
  );
  const editingQualityLabelRenamed = Boolean(
    editingQualityLabel && qualityLabelName.trim() && editingQualityLabel.name !== qualityLabelName.trim(),
  );

  const eligibleQualityItems = useMemo(() => {
    return qualityItems.filter((item) => isInspectionEligible(item, inspectionKind));
  }, [inspectionKind, qualityItems]);

  const outboundReceiverSuggestions = useMemo(() => {
    const query = outboundReceiver.trim().toLocaleLowerCase();
    return (catalog?.parties ?? [])
      .filter((party) => party.roles.includes("upstream_receiver"))
      .filter((party) => !query || party.display_name.toLocaleLowerCase().includes(query))
      .sort((left, right) => {
        if (!query) return left.display_name.localeCompare(right.display_name, "zh-CN");
        const leftStarts = left.display_name.toLocaleLowerCase().startsWith(query);
        const rightStarts = right.display_name.toLocaleLowerCase().startsWith(query);
        if (leftStarts !== rightStarts) return leftStarts ? -1 : 1;
        return left.display_name.localeCompare(right.display_name, "zh-CN");
      })
      .slice(0, 8);
  }, [catalog, outboundReceiver]);

  const outboundScanGroups = useMemo(() => {
    const groups = new Map<string, OutboundScanGroup>();
    for (const item of outboundScannedItems) {
      const current = groups.get(item.skuId);
      if (current) {
        current.count += 1;
        current.barcodes.push(item.barcode);
      } else {
        groups.set(item.skuId, {
          skuId: item.skuId,
          skuCode: item.skuCode,
          skuName: item.skuName,
          count: 1,
          barcodes: [item.barcode],
        });
      }
    }
    return Array.from(groups.values());
  }, [outboundScannedItems]);
  const outboundHasScannedItems = outboundScannedItems.length > 0;

  const refreshCatalog = useCallback(async () => {
    const context = workspaceContextRef.current;
    if (mode === "network" && !networkStatus?.authenticated) {
      setCatalog(null);
      return;
    }
    setCatalogLoading(true);
    try {
      const command = mode === "network" ? "v2_network_list_reference_catalog" : "v2_list_reference_catalog";
      const nextCatalog = await invoke<ReferenceCatalog>(command);
      if (context !== workspaceContextRef.current) return;
      setCatalog(nextCatalog);
      setSelectedProductId((current) => nextCatalog.products.some((product) => product.sku_id === current)
        ? current
        : (nextCatalog.products[0]?.sku_id ?? ""));
      setReceiptProductInput((current) => {
        const matched = nextCatalog.products.find((product) => product.code.toLocaleLowerCase() === current.trim().toLocaleLowerCase());
        return matched?.code ?? nextCatalog.products[0]?.code ?? "";
      });
      setSupplierName((current) => {
        const matched = nextCatalog.suppliers.find((party) => party.display_name.toLocaleLowerCase() === current.trim().toLocaleLowerCase());
        return matched?.display_name ?? nextCatalog.suppliers[0]?.display_name ?? "";
      });
    } catch (error) {
      if (context === workspaceContextRef.current) {
        setCatalogNotice({ type: "error", text: `读取基础资料失败：${displayError(error)}` });
      }
    } finally {
      if (context === workspaceContextRef.current) setCatalogLoading(false);
    }
  }, [mode, networkStatus?.authenticated]);

  const refreshDashboard = useCallback(async () => {
    const context = workspaceContextRef.current;
    if (mode === "network" && !networkStatus?.authenticated) return;
    setDashboardLoading(true);
    setDashboardError(null);
    try {
      const command = mode === "network" ? "v2_network_get_dashboard" : "v2_get_dashboard";
      const response = await invoke<DashboardDto>(command, { query: { owner_party_id: null, sku_id: null } });
      if (context === workspaceContextRef.current) {
        const products = response.products ?? [];
        setDashboard({ ...response, products });
        setSelectedOverviewSkuId((current) => (
          products.some((product) => product.sku_id === current)
            ? current
            : (products[0]?.sku_id ?? "")
        ));
      }
    } catch (error) {
      if (context === workspaceContextRef.current) setDashboardError(displayError(error));
    } finally {
      if (context === workspaceContextRef.current) setDashboardLoading(false);
    }
  }, [mode, networkStatus?.authenticated]);

  const refreshQualityLabels = useCallback(async () => {
    const context = workspaceContextRef.current;
    if (mode === "network" && !networkStatus?.authenticated) {
      setQualityLabels([]);
      setInspectionQualityLabelId("");
      return;
    }
    setQualityLabelsLoading(true);
    try {
      const command = mode === "network" ? "v2_network_list_quality_labels" : "v2_list_quality_labels";
      const labels = await invoke<QualityLabel[]>(command);
      if (context !== workspaceContextRef.current) return;
      setQualityLabels(labels);
      setInspectionQualityLabelId((current) => (
        labels.some((label) => label.active && label.quality_label_id === current)
          ? current
          : (labels.find((label) => label.active)?.quality_label_id ?? "")
      ));
    } catch (error) {
      if (context === workspaceContextRef.current) {
        setQualityLabelNotice({ type: "error", text: `读取质检标签失败：${displayError(error)}` });
      }
    } finally {
      if (context === workspaceContextRef.current) setQualityLabelsLoading(false);
    }
  }, [mode, networkStatus?.authenticated]);

  const refreshQualityItems = useCallback(async () => {
    if (qualityScanCheckingRef.current) return;
    const context = workspaceContextRef.current;
    if (mode === "network" && !networkStatus?.authenticated) return;
    setQualityLoading(true);
    setQualityNotice(null);
    try {
      const command = mode === "network" ? "v2_network_list_inventory" : "v2_list_inventory";
      const response = await invoke<InventoryListResponse>(command, { query: emptyInventoryQuery() });
      if (context !== workspaceContextRef.current) return;
      setQualityItems(
        response.items.filter(
          (item) => isInspectionEligible(item, "initial") || isInspectionEligible(item, "retest"),
        ),
      );
    } catch (error) {
      if (context === workspaceContextRef.current) {
        setQualityNotice({ type: "error", text: `读取待检库存失败：${displayError(error)}` });
      }
    } finally {
      if (context === workspaceContextRef.current) setQualityLoading(false);
    }
  }, [mode, networkStatus?.authenticated]);

  const refreshInventory = useCallback(async (): Promise<boolean> => {
    const context = workspaceContextRef.current;
    if (mode === "network" && !networkStatus?.authenticated) return false;
    setInventoryLoading(true);
    setInventoryError(null);
    try {
      const query: InventoryListQuery = {
        ...emptyInventoryQuery(),
        search: inventorySearchRef.current.trim() || null,
        inventory_status: inventoryStatusRef.current || null,
        quality_status: qualityStatusRef.current || null,
      };
      const command = mode === "network" ? "v2_network_list_inventory" : "v2_list_inventory";
      const response = await invoke<InventoryListResponse>(command, { query });
      if (context !== workspaceContextRef.current) return false;
      setInventoryItems(response.items);
      setInventoryTotal(response.total);
      return true;
    } catch (error) {
      if (context === workspaceContextRef.current) setInventoryError(displayError(error));
      return false;
    } finally {
      if (context === workspaceContextRef.current) setInventoryLoading(false);
    }
  }, [mode, networkStatus?.authenticated]);

  async function openInventoryTrace(barcode: string): Promise<boolean> {
    const context = workspaceContextRef.current;
    setInventoryTraceBarcode(barcode);
    setInventoryTrace(null);
    setInventoryTraceError(null);
    setInventoryTraceLoading(true);
    setLifecycleSearch(barcode);
    setPage("lifecycle");
    try {
      const command = mode === "network" ? "v2_network_inventory_trace" : "v2_inventory_trace";
      const trace = await invoke<InventoryTrace>(command, { barcode });
      if (context !== workspaceContextRef.current) return false;
      setInventoryTrace(trace);
      return true;
    } catch (error) {
      if (context === workspaceContextRef.current) setInventoryTraceError(displayError(error));
      return false;
    } finally {
      if (context === workspaceContextRef.current) setInventoryTraceLoading(false);
    }
  }

  async function searchLifecycle(event?: FormEvent<HTMLFormElement>, fromEnter = false) {
    event?.preventDefault();
    const barcode = lifecycleSearch.trim();
    if (!barcode) {
      setInventoryTraceError("请输入或扫描 SN");
      return;
    }
    const loaded = await openInventoryTrace(barcode);
    if (fromEnter && loaded && searchClearPreferences.lifecycle && lifecycleSearchRef.current.trim() === barcode) {
      setLifecycleSearch("");
    }
  }

  async function refreshRecords(): Promise<boolean> {
    if (mode === "network" && !networkStatus?.authenticated) return false;
    setRecordLoading(true);
    setRecordNotice(null);
    setSelectedReceiptDocument(null);
    setSelectedOutboundDocument(null);
    try {
      const query = { search: recordSearchRef.current.trim() || null, limit: 200 };
      if (recordTab === "receipt") {
        const command = mode === "network" ? "v2_network_list_receipt_records" : "v2_list_receipt_records";
        setReceiptRecords(await invoke<ReceiptRecord[]>(command, { query }));
      } else {
        const command = mode === "network" ? "v2_network_list_outbound_order_records" : "v2_list_outbound_order_records";
        setOutboundRecords(await invoke<OutboundOrderRecord[]>(command, { query }));
      }
      return true;
    } catch (error) {
      setRecordNotice({ type: "error", text: `读取单据失败：${displayError(error)}` });
      return false;
    } finally {
      setRecordLoading(false);
    }
  }

  async function submitInventorySearch(fromEnter = false) {
    const submittedSearch = inventorySearchRef.current;
    const loaded = await refreshInventory();
    if (fromEnter && loaded && searchClearPreferences.inventory && inventorySearchRef.current === submittedSearch) {
      setInventorySearch("");
    }
  }

  async function submitRecordsSearch(fromEnter = false) {
    const submittedSearch = recordSearchRef.current;
    const loaded = await refreshRecords();
    if (fromEnter && loaded && searchClearPreferences.records && recordSearchRef.current === submittedSearch) {
      setRecordSearch("");
    }
  }

  async function openReceiptDocument(receiptId: string) {
    setRecordLoading(true);
    setRecordNotice(null);
    try {
      const command = mode === "network" ? "v2_network_receipt_document" : "v2_receipt_document";
      setSelectedReceiptDocument(await invoke<ReceiptDocument>(command, { receiptId }));
      setSelectedOutboundDocument(null);
    } catch (error) {
      setRecordNotice({ type: "error", text: `读取收货单详情失败：${displayError(error)}` });
    } finally {
      setRecordLoading(false);
    }
  }

  async function openOutboundDocument(orderId: string) {
    setRecordLoading(true);
    setRecordNotice(null);
    try {
      const command = mode === "network" ? "v2_network_outbound_order_document" : "v2_outbound_order_document";
      setSelectedOutboundDocument(await invoke<OutboundOrderDocument>(command, { orderId }));
      setSelectedReceiptDocument(null);
    } catch (error) {
      setRecordNotice({ type: "error", text: `读取出库订单详情失败：${displayError(error)}` });
    } finally {
      setRecordLoading(false);
    }
  }

  function openRenameOutboundDialog(document: OutboundOrderDocument) {
    if (document.status === "voided") {
      setRecordNotice({ type: "warning", text: "已作废的出库单不能修改客户名称。" });
      return;
    }
    setRenameOutboundDialog({
      orderId: document.order_id,
      orderNo: document.order_no,
      currentName: document.receiver_name,
    });
    setRenameOutboundName(document.receiver_name);
    setRecordNotice(null);
  }

  function closeRenameOutboundDialog() {
    if (renameOutboundLoading) return;
    setRenameOutboundDialog(null);
    setRenameOutboundName("");
  }

  async function submitRenameOutbound(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!renameOutboundDialog || renameOutboundLoading) return;
    const receiverName = renameOutboundName.trim();
    if (!receiverName) return;
    if (receiverName === renameOutboundDialog.currentName) {
      setRecordNotice({ type: "warning", text: "新客户名称与当前名称相同。" });
      return;
    }
    setRenameOutboundLoading(true);
    setRecordNotice(null);
    try {
      const operationId = createId();
      const common = {
        request_id: operationId,
        idempotency_key: `outbound-rename:${operationId}`,
        order_id: renameOutboundDialog.orderId,
        upstream_receiver_name: receiverName,
      };
      const command = mode === "network" ? "v2_network_rename_outbound_order" : "v2_rename_outbound_order";
      const input = mode === "network"
        ? (common satisfies NetworkRenameOutboundOrderRequest)
        : ({ ...common, actor_id: resolvedActorId } satisfies RenameOutboundOrderRequest);
      const response = await invoke<RenameOutboundOrderResponse>(command, { input });
      setRenameOutboundDialog(null);
      setRenameOutboundName("");
      await refreshRecords();
      await openOutboundDocument(response.order_id);
      setRecordNotice({ type: "success", text: `${response.order_no} 的客户名称已修改为“${response.receiver_name}”。` });
    } catch (error) {
      setRecordNotice({ type: "error", text: `修改客户名称失败：${displayError(error)}` });
    } finally {
      setRenameOutboundLoading(false);
    }
  }

  async function exportBusinessDocument(kind: "receipt" | "outbound", id: string, documentNo: string) {
    const path = await save({
      filters: [{ name: "Excel", extensions: ["xlsx"] }],
      defaultPath: `${documentNo}_${kind === "receipt" ? "收货单" : "出库单"}.xlsx`,
    });
    if (!path) return;
    setRecordLoading(true);
    try {
      const prefix = mode === "network" ? "v2_network" : "v2";
      const command = `${prefix}_export_${kind === "receipt" ? "receipt_document" : "outbound_order_document"}`;
      await invoke(command, kind === "receipt" ? { receiptId: id, path } : { orderId: id, path });
      setRecordNotice({ type: "success", text: `Excel 单据已导出：${path}` });
    } catch (error) {
      setRecordNotice({ type: "error", text: `导出失败：${displayError(error)}` });
    } finally {
      setRecordLoading(false);
    }
  }

  function openSnCopyDialog(
    kind: "receipt" | "outbound",
    documentId: string,
    documentNo: string,
    itemCount: number,
  ) {
    setSnCopyPassword("");
    setSnCopyDialog({ kind, documentId, documentNo, itemCount });
  }

  function closeSnCopyDialog() {
    if (snCopyLoading) return;
    setSnCopyDialog(null);
    setSnCopyPassword("");
  }

  async function submitSnCopy(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!snCopyDialog || !snCopyPassword) return;
    setSnCopyLoading(true);
    setRecordNotice(null);
    try {
      const prefix = mode === "network" ? "v2_network" : "v2";
      const command = `${prefix}_copy_${snCopyDialog.kind === "receipt" ? "receipt_document" : "outbound_order_document"}_sns`;
      const response = await invoke<CopyDocumentSnResponse>(command, {
        input: {
          document_id: snCopyDialog.documentId,
          password: snCopyPassword,
          actor_id: mode === "offline" ? resolvedActorId : null,
          request_id: createId(),
        },
      });
      if (!navigator.clipboard?.writeText) {
        throw new Error("当前环境不支持写入剪贴板");
      }
      await navigator.clipboard.writeText(response.barcodes.join("\n"));
      const count = response.barcodes.length;
      setSnCopyDialog(null);
      setSnCopyPassword("");
      setRecordNotice({
        type: "success",
        text: `已复制 ${response.document_no} 的 ${count} 个 SN（每行一个）`,
      });
    } catch (error) {
      setRecordNotice({ type: "error", text: `复制 SN 失败：${displayError(error)}` });
    } finally {
      setSnCopyLoading(false);
    }
  }

  function openVoidDialog(
    kind: "receipt" | "outbound",
    documentId: string,
    documentNo: string,
    itemCount: number,
    eligibility: DocumentVoidEligibility,
  ) {
    if (!eligibility.can_void) {
      setRecordNotice({
        type: "warning",
        text: eligibility.blockers[0] ?? "该单据当前不能作废",
      });
      return;
    }
    setVoidReason("");
    setVoidPassword("");
    setVoidDialog({ kind, documentId, documentNo, itemCount, blockers: eligibility.blockers });
  }

  function closeVoidDialog() {
    if (voidLoading) return;
    setVoidDialog(null);
    setVoidReason("");
    setVoidPassword("");
  }

  async function submitVoidDocument(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!voidDialog || !voidReason.trim() || !voidPassword) return;
    setVoidLoading(true);
    setRecordNotice(null);
    try {
      const prefix = mode === "network" ? "v2_network" : "v2";
      const command = `${prefix}_void_${voidDialog.kind === "receipt" ? "receipt_document" : "outbound_order_document"}`;
      const response = await invoke<VoidDocumentResponse>(command, {
        input: {
          document_id: voidDialog.documentId,
          reason: voidReason.trim(),
          password: voidPassword,
          actor_id: mode === "offline" ? resolvedActorId : null,
          request_id: createId(),
          idempotency_key: createId(),
        },
      });
      const kind = voidDialog.kind;
      const documentId = voidDialog.documentId;
      setVoidDialog(null);
      setVoidReason("");
      setVoidPassword("");
      await Promise.all([refreshRecords(), refreshDashboard(), refreshInventory()]);
      if (kind === "receipt") await openReceiptDocument(documentId);
      else await openOutboundDocument(documentId);
      setRecordNotice({
        type: "success",
        text: `${response.document_no} 已作废。作废库存 ${response.voided_inventory_count} 件，解除预留 ${response.released_inventory_count} 件，保持隔离 ${response.quarantined_inventory_count} 件。`,
      });
    } catch (error) {
      setRecordNotice({ type: "error", text: `作废失败：${displayError(error)}` });
    } finally {
      setVoidLoading(false);
    }
  }

  async function changeOperationPassword(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setOperationPasswordNotice(null);
    if (operationNewPassword !== operationConfirmPassword) {
      setOperationPasswordNotice({ type: "error", text: "两次输入的新密码不一致" });
      return;
    }
    setOperationPasswordLoading(true);
    try {
      await invoke("v2_change_operation_password", {
        input: {
          current_password: operationCurrentPassword,
          new_password: operationNewPassword,
          actor_id: resolvedActorId,
          request_id: createId(),
        },
      });
      setOperationCurrentPassword("");
      setOperationNewPassword("");
      setOperationConfirmPassword("");
      setOperationPasswordNotice({ type: "success", text: "危险操作密码已更新" });
    } catch (error) {
      setOperationPasswordNotice({ type: "error", text: `修改密码失败：${displayError(error)}` });
    } finally {
      setOperationPasswordLoading(false);
    }
  }

  async function lookupReturnBarcode(event?: FormEvent<HTMLFormElement>, fromEnter = false) {
    event?.preventDefault();
    const barcode = returnBarcode.trim().toUpperCase();
    if (!barcode) return;
    setReturnLoading(true);
    setReturnNotice(null);
    try {
      const command = mode === "network" ? "v2_network_lookup_return_candidate" : "v2_lookup_return_candidate";
      const candidate = await invoke<ReturnCandidate>(command, { barcode });
      const currentBatch = returnCandidates;
      const shouldClearSubmittedBarcode = fromEnter && searchClearPreferences.inventory
        && returnBarcodeRef.current === barcode;
      if (currentBatch.some((item) => item.shipment_line_id === candidate.shipment_line_id)) {
        setReturnNotice({ type: "warning", text: `${candidate.barcode} 已在本批扫描清单中` });
      } else if (currentBatch.length > 0 && currentBatch[0].shipment_id !== candidate.shipment_id) {
        setReturnNotice({ type: "error", text: `本批只能退回同一出库单（当前为 ${currentBatch[0].shipment_no}），${candidate.barcode} 属于 ${candidate.shipment_no}` });
      } else {
        setReturnCandidates((items) => [...items, candidate]);
        setReturnNotice({ type: "success", text: `已加入本批：${candidate.barcode} · ${candidate.order_no}` });
      }
      if (shouldClearSubmittedBarcode) setReturnBarcode("");
    } catch (error) {
      setReturnNotice({ type: "error", text: `退货扫码已拒绝：${displayError(error)}` });
      await playScannerAlert();
    } finally {
      setReturnLoading(false);
      returnScannerRef.current?.focus();
    }
  }

  async function commitScannedReturn() {
    const batch = returnCandidates;
    if (batch.length === 0 || !returnReason.trim()) {
      setReturnNotice({ type: "error", text: "请先扫描本批 SN 并填写统一退货原因" });
      return;
    }
    setReturnLoading(true);
    try {
      const operationId = createId();
      const first = batch[0];
      const common = {
        request_id: operationId,
        idempotency_key: `outbound-return:${operationId}`,
        shipment_id: first.shipment_id,
        shipment_line_ids: batch.map((candidate) => candidate.shipment_line_id),
        return_no: makeDocumentNumber("TH"),
        returned_at: new Date().toISOString(),
        reason: returnReason.trim(),
      };
      const command = mode === "network" ? "v2_network_return_outbound_shipment" : "v2_return_outbound_shipment";
      const input = mode === "network" ? (common satisfies NetworkReturnOutboundShipmentRequest) : { ...common, actor_id: resolvedActorId };
      const response = await invoke<ReturnOutboundShipmentResponse>(command, { input });
      setReturnNotice({ type: "success", text: `${response.return_no} 已批量退回 ${response.quarantined_count} 件，商品已进入隔离区` });
      setReturnCandidates([]);
      setReturnStep("scan");
      setReturnBarcode("");
      setReturnReason("");
      void refreshDashboard();
    } catch (error) {
      setReturnNotice({ type: "error", text: `登记退货失败：${displayError(error)}` });
    } finally {
      setReturnLoading(false);
      if (returnStep === "scan") returnScannerRef.current?.focus();
    }
  }

  const refreshNetworkWarehouses = useCallback(async () => {
    const context = workspaceContextRef.current;
    if (mode !== "network" || !networkStatus?.authenticated) {
      setNetworkWarehouses([]);
      return;
    }
    setNetworkWarehousesLoading(true);
    setNetworkWarehousesError(null);
    try {
      const warehouses = await invoke<NetworkWarehouse[]>("v2_network_list_warehouses");
      if (context !== workspaceContextRef.current) return;
      setNetworkWarehouses(warehouses);
      setNetworkWarehouseId((current) => {
        const stored = getStoredNetworkValue("inventory-v2-network-warehouse");
        const preferred = warehouses.some((warehouse) => warehouse.warehouse_id === current)
          ? current
          : stored;
        const selected = warehouses.some((warehouse) => warehouse.warehouse_id === preferred)
          ? preferred
          : (warehouses[0]?.warehouse_id ?? "");
        try {
          if (selected) window.localStorage.setItem("inventory-v2-network-warehouse", selected);
        } catch {
          // The selected warehouse is still kept in component state.
        }
        return selected;
      });
    } catch (error) {
      if (context === workspaceContextRef.current) {
        setNetworkWarehouses([]);
        setNetworkWarehousesError(displayError(error));
      }
    } finally {
      if (context === workspaceContextRef.current) setNetworkWarehousesLoading(false);
    }
  }, [mode, networkStatus?.authenticated]);

  useEffect(() => {
    let cancelled = false;
    const context = workspaceContextRef.current;
    void invoke<NetworkStatus>("v2_network_status")
      .then((status) => {
        if (!cancelled && context === workspaceContextRef.current) setNetworkStatus(status);
      })
      .catch(() => {
        if (!cancelled && context === workspaceContextRef.current) setNetworkStatus(null);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    void invoke<RestoreReport | null>("v2_offline_restore_report")
      .then(setRestoreReport)
      .catch(() => setRestoreReport(null));
  }, []);

  useEffect(() => {
    void refreshNetworkWarehouses();
  }, [refreshNetworkWarehouses]);

  useEffect(() => {
    if (page === "catalog" || page === "receipt" || page === "outbound") void refreshCatalog();
  }, [page, refreshCatalog]);

  useEffect(() => {
    const activeGroup = navigationItems.find((item) => item.id === page)?.group;
    if (!activeGroup) return;
    setExpandedNavGroups((current) => {
      if (current.has(activeGroup)) return current;
      return new Set([activeGroup]);
    });
  }, [page]);

  useEffect(() => {
    if (page === "overview") void refreshDashboard();
    if (page === "quality") {
      void refreshQualityItems();
      void refreshQualityLabels();
    }
    if (page === "inventory") void refreshInventory();
    if (page === "records") void refreshRecords();
  }, [mode, networkStatus?.authenticated, page, refreshDashboard, refreshInventory, refreshQualityItems, refreshQualityLabels]);

  useEffect(() => {
    if (page === "records") void refreshRecords();
  }, [recordTab]);

  useEffect(() => {
    setSelectedBarcodes(new Set());
    setQualityStep(1);
    setQualityScannerInput("");
    setQualityBulkInput("");
    setQualityScanNotice(null);
    setQualityNotice(null);
    window.requestAnimationFrame(() => qualityScannerInputRef.current?.focus());
  }, [inspectionKind]);

  useEffect(() => {
    let focusFrame: number | null = null;
    if (page === "receipt" && receiptStep === 2 && receiptDetailsReady && !catalogLoading && !receiptLoading && !scanChecking) {
      focusFrame = window.requestAnimationFrame(() => scannerInputRef.current?.focus());
    } else if (page === "quality" && qualityStep === 1 && !qualityLoading && !qualityScanChecking) {
      focusFrame = window.requestAnimationFrame(() => qualityScannerInputRef.current?.focus());
    } else if (page === "outbound" && outboundStep === 2 && !outboundLoading && !outboundScanChecking && !outboundShipment) {
      focusFrame = window.requestAnimationFrame(() => outboundScannerInputRef.current?.focus());
    } else if (page === "returns" && returnStep === "scan" && !returnLoading) {
      focusFrame = window.requestAnimationFrame(() => returnScannerRef.current?.focus());
    }
    return () => {
      if (focusFrame !== null) window.cancelAnimationFrame(focusFrame);
    };
  }, [catalogLoading, page, outboundLoading, outboundScanChecking, outboundShipment, outboundStep, qualityLoading, qualityScanChecking, qualityStep, receiptDetailsReady, receiptLoading, receiptStep, returnLoading, returnStep, scanChecking]);

  function toggleNavGroup(groupId: NavigationGroupId) {
    setExpandedNavGroups((current) => {
      return current.has(groupId) ? new Set<NavigationGroupId>() : new Set([groupId]);
    });
  }

  function resetCatalogDraft() {
    setNewProductCode("");
    setNewProductName("");
    setNewProductSerialPrefix("");
    setNewProductForbiddenChars("-, ");
    setEditingProductId(null);
    setEditingPartyId(null);
    setNewPartyName("");
    setNewPartyRoles(new Set(["supplier"]));
    setNewPartyContactName("");
    setNewPartyPhone("");
    setNewPartyWechat("");
    setNewPartyEmail("");
    setNewPartyAddress("");
    setNewPartyNotes("");
  }

  function openCatalogCreate(tab: CatalogTab = catalogTab, initialRoles: CatalogPartyRole[] = ["supplier"]) {
    resetCatalogDraft();
    setCatalogTab(tab);
    if (tab === "parties") setNewPartyRoles(new Set(initialRoles));
    setCatalogNotice(null);
    setCatalogCreateOpen(true);
  }

  function openCatalogCreateFromReceipt(tab: CatalogTab) {
    if (workspaceOperationInProgress()) return;
    openCatalogCreate(tab, ["supplier"]);
    setPage("catalog");
  }

  function openCatalogPartyEdit(party: CatalogParty) {
    resetCatalogDraft();
    setCatalogTab("parties");
    setEditingPartyId(party.party_id);
    setNewPartyName(party.display_name);
    setNewPartyRoles(new Set(party.roles));
    setNewPartyContactName(party.contact_name ?? "");
    setNewPartyPhone(party.phone ?? "");
    setNewPartyWechat(party.wechat ?? "");
    setNewPartyEmail(party.email ?? "");
    setNewPartyAddress(party.address ?? "");
    setNewPartyNotes(party.notes ?? "");
    setCatalogNotice(null);
    setCatalogCreateOpen(true);
  }

  function openCatalogProductEdit(product: CatalogProduct) {
    resetCatalogDraft();
    setCatalogTab("products");
    setEditingProductId(product.sku_id);
    setNewProductCode(product.code);
    setNewProductName(product.name);
    setNewProductSerialPrefix(product.serial_prefix ?? "");
    setNewProductForbiddenChars(product.serial_forbidden_chars);
    setCatalogNotice(null);
    setCatalogCreateOpen(true);
  }

  function toggleNewPartyRole(role: CatalogPartyRole) {
    setNewPartyRoles((current) => {
      const next = new Set(current);
      if (next.has(role)) next.delete(role);
      else next.add(role);
      return next;
    });
  }

  function closeCatalogCreate() {
    if (catalogLoading) return;
    setCatalogCreateOpen(false);
    resetCatalogDraft();
  }

  function resetOutboundWorkflow(preserveOrderDetails: boolean) {
    setOutboundOrder(null);
    setOutboundAllocation(null);
    setOutboundShipment(null);
    setOutboundResolved(false);
    if (!preserveOrderDetails) {
      setOutboundReceiver("");
    }
    setOutboundReceiverSuggestionsOpen(false);
    setOutboundScannerInput("");
    setOutboundBulkInput("");
    setOutboundScannedItems([]);
    setOutboundShipmentNo("");
    setOutboundConfirmationCode("");
    setOutboundWarrantyPreset("");
    setOutboundWarrantyCustomDays("");
    setOutboundWarrantyManualStart(false);
    setOutboundScanNotice(null);
    setOutboundNotice(null);
    setOutboundStep(1);
  }

  function canOpenOutboundStep(step: OutboundStep): boolean {
    if (step === 1) return true;
    if (step === 2) {
      return Boolean(outboundReceiver.trim());
    }
    return Boolean(outboundShipment);
  }

  function navigateOutboundStep(step: OutboundStep) {
    if (workspaceOperationInProgress() || !canOpenOutboundStep(step)) return;
    setOutboundStep(step);
  }

  function canOpenReceiptStep(step: ReceiptStep): boolean {
    if (receiptCompleted) return step === 3;
    if (step === 1) return true;
    if (step === 2) return receiptDetailsReady;
    return receiptDetailsReady && barcodes.length > 0;
  }

  function navigateReceiptStep(step: ReceiptStep) {
    if (workspaceOperationInProgress() || !canOpenReceiptStep(step)) return;
    setReceiptStep(step);
  }

  function startNextReceiptBatch() {
    if (workspaceOperationInProgress()) return;
    setReceiptCompleted(null);
    setScannedBarcodes([]);
    setScannerInput("");
    setReceiptBulkInput("");
    setReceiptNotice(null);
    setSourceReference("");
    setReceivedAt(getLocalDateTimeValue());
    setReceiptWarrantyPreset("");
    setReceiptWarrantyCustomDays("");
    setReceiptWarrantyManualStart(false);
    setReceiptWarrantyStartsAt(getLocalDateTimeValue());
    setReceiptStep(1);
  }

  function chooseReceiptProduct(product: CatalogProduct) {
    setReceiptProductInput(product.code);
    setSelectedProductId(product.sku_id);
    setReceiptProductSuggestionsOpen(false);
    setScannerInput("");
    setReceiptNotice(null);
  }

  function updateReceiptProductInput(value: string) {
    setReceiptProductInput(value);
    const normalized = value.trim().toLocaleLowerCase();
    const matched = (catalog?.products ?? []).find((product) => (
      product.code.toLocaleLowerCase() === normalized
      || product.name.toLocaleLowerCase() === normalized
    ));
    setSelectedProductId(matched?.sku_id ?? "");
    setReceiptProductSuggestionsOpen(true);
    setScannerInput("");
    setReceiptNotice(null);
  }

  function chooseReceiptSupplier(party: CatalogParty) {
    setSupplierName(party.display_name);
    setReceiptSupplierSuggestionsOpen(false);
  }

  function updateReceiptSupplierInput(value: string) {
    setSupplierName(value);
    setReceiptSupplierSuggestionsOpen(true);
  }

  function canOpenQualityStep(step: QualityStep): boolean {
    return step === 1 || selectedBarcodes.size > 0;
  }

  function navigateQualityStep(step: QualityStep) {
    if (workspaceOperationInProgress() || !canOpenQualityStep(step)) return;
    setQualityStep(step);
  }

  function startNewQualityLabel() {
    setEditingQualityLabelId(null);
    setQualityLabelName("");
    setQualityLabelDisposition("available");
    setQualityLabelActive(true);
    setQualityLabelRenameNote("");
    setQualityLabelNotice(null);
  }

  function editQualityLabel(label: QualityLabel) {
    setEditingQualityLabelId(label.quality_label_id);
    setQualityLabelName(label.name);
    setQualityLabelDisposition(label.disposition);
    setQualityLabelActive(label.active);
    setQualityLabelRenameNote("");
    setQualityLabelNotice(null);
  }

  function openQualityLabelManager() {
    setQualityLabelModalOpen(true);
    startNewQualityLabel();
    void refreshQualityLabels();
  }

  function closeQualityLabelManager() {
    if (qualityLabelsLoading) return;
    setQualityLabelModalOpen(false);
    setQualityLabelNotice(null);
  }

  async function submitQualityLabel(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (qualityLabelsLoading) return;
    const name = qualityLabelName.trim();
    if (!name) {
      setQualityLabelNotice({ type: "error", text: "请输入标签名称。" });
      return;
    }
    setQualityLabelsLoading(true);
    setQualityLabelNotice(null);
    const input: SaveQualityLabelRequest = {
      quality_label_id: editingQualityLabelId,
      name,
      disposition: qualityLabelDisposition,
      active: qualityLabelActive,
      rename_note: editingQualityLabelRenamed ? (qualityLabelRenameNote.trim() || null) : null,
    };
    try {
      const command = mode === "network" ? "v2_network_save_quality_label" : "v2_save_quality_label";
      const saved = await invoke<QualityLabel>(command, { input });
      const listCommand = mode === "network" ? "v2_network_list_quality_labels" : "v2_list_quality_labels";
      const labels = await invoke<QualityLabel[]>(listCommand);
      setQualityLabels(labels);
      setInspectionQualityLabelId((current) => (
        labels.some((label) => label.active && label.quality_label_id === current)
          ? current
          : (labels.find((label) => label.active)?.quality_label_id ?? "")
      ));
      setEditingQualityLabelId(saved.quality_label_id);
      setQualityLabelName(saved.name);
      setQualityLabelDisposition(saved.disposition);
      setQualityLabelActive(saved.active);
      setQualityLabelRenameNote("");
      setQualityLabelNotice({ type: "success", text: `质检标签“${saved.name}”已保存。` });
    } catch (error) {
      setQualityLabelNotice({ type: "error", text: `保存质检标签失败：${displayError(error)}` });
    } finally {
      setQualityLabelsLoading(false);
    }
  }

  function resetWorkspaceTransientState() {
    workspaceContextRef.current += 1;
    scanCheckingRef.current = false;
    qualityScanCheckingRef.current = false;
    outboundScanCheckingRef.current = false;
    setScanChecking(false);
    setQualityScanChecking(false);
    setOutboundScanChecking(false);

    setDashboard(null);
    setDashboardLoading(false);
    setDashboardError(null);
    setSelectedOverviewSkuId("");
    setOverviewShortcutEditorOpen(false);
    setOverviewShortcutDraft([]);

    setCatalog(null);
    setCatalogLoading(false);
    setCatalogNotice(null);
    setCatalogCreateOpen(false);
    resetCatalogDraft();
    setSelectedProductId("");
    setReceiptProductInput("");
    setReceiptProductSuggestionsOpen(false);
    setReceiptSupplierSuggestionsOpen(false);
    setSupplierName("");

    setScannedBarcodes([]);
    setScannerInput("");
    setReceiptBulkInput("");
    setReceiptNotice(null);
    setReceiptLoading(false);
    setReceiptStep(1);
    setReceiptCompleted(null);
    setSourceReference("");
    setReceivedAt(getLocalDateTimeValue());

    setQualityItems([]);
    setQualityLoading(false);
    setQualityStep(1);
    setQualityLabels([]);
    setQualityLabelsLoading(false);
    setQualityLabelModalOpen(false);
    setQualityLabelNotice(null);
    setEditingQualityLabelId(null);
    setQualityLabelName("");
    setQualityLabelDisposition("available");
    setQualityLabelActive(true);
    setQualityLabelRenameNote("");
    setInspectionQualityLabelId("");
    setSelectedBarcodes(new Set());
    setQualityScannerInput("");
    setQualityBulkInput("");
    setQualityScanNotice(null);
    setQualityNotice(null);
    setDefectCode("");
    setInspectionNotes("");

    setInventoryItems([]);
    setInventoryTotal(0);
    setInventoryLoading(false);
    setInventoryError(null);
    setInventoryTrace(null);
    setInventoryTraceBarcode(null);
    setInventoryTraceLoading(false);
    setInventoryTraceError(null);

    resetOutboundWorkflow(false);
    setOutboundLoading(false);
    setReceiptRecords([]);
    setOutboundRecords([]);
    setRecordLoading(false);
    setRecordNotice(null);
    setSelectedReceiptDocument(null);
    setSelectedOutboundDocument(null);
    setRenameOutboundDialog(null);
    setRenameOutboundName("");
    setRenameOutboundLoading(false);
    setVoidDialog(null);
    setVoidReason("");
    setVoidPassword("");
    setVoidLoading(false);
    setOperationCurrentPassword("");
    setOperationNewPassword("");
    setOperationConfirmPassword("");
    setOperationPasswordLoading(false);
    setOperationPasswordNotice(null);

    setNetworkWarehouses([]);
    setNetworkWarehouseId("");
    setNetworkWarehousesLoading(false);
    setNetworkWarehousesError(null);
    dataOperationRef.current = false;
    setDataOperationLoading(false);
    childPanelBusyRef.current = false;
    setChildPanelBusy(false);
    setDataNotice(null);
    setUpgradeTargetWorkspaceId("");
    setUpgradeImport(null);
  }

  function workspaceOperationInProgress(): boolean {
    return scanCheckingRef.current
      || qualityScanCheckingRef.current
      || outboundScanCheckingRef.current
      || receiptLoading
      || qualityLoading
      || outboundLoading
      || catalogLoading
      || recordLoading
      || renameOutboundLoading
      || voidLoading
      || snCopyLoading
      || operationPasswordLoading
      || dataOperationRef.current
      || dataOperationLoading
      || childPanelBusyRef.current
      || childPanelBusy
      || networkAuthLoading;
  }

  function navigateToPage(nextPage: WorkspacePage) {
    if (nextPage === page || workspaceOperationInProgress()) return;
    setPage(nextPage);
  }

  function beginDataOperation(): boolean {
    if (dataOperationRef.current) return false;
    dataOperationRef.current = true;
    setDataOperationLoading(true);
    return true;
  }

  function endDataOperation() {
    dataOperationRef.current = false;
    setDataOperationLoading(false);
  }

  const handleChildPanelBusyChange = useCallback((busy: boolean) => {
    childPanelBusyRef.current = busy;
    setChildPanelBusy(busy);
  }, []);

  async function confirmWorkspaceReset(action: string, title: string): Promise<boolean> {
    const activeScanCount = scannedBarcodes.length + selectedBarcodes.size + outboundScannedItems.length;
    const hasOpenOutboundOrder = Boolean(outboundOrder && !outboundResolved);
    if (activeScanCount === 0 && !hasOpenOutboundOrder) return true;
    const details = [
      activeScanCount > 0 ? `${activeScanCount} 个尚未提交的扫码记录` : null,
      hasOpenOutboundOrder ? `未结束的出库订单 ${outboundOrder?.order_no ?? ""}` : null,
    ].filter(Boolean).join("和");
    return confirm(`${action}会清空${details}，确定继续吗？`, { title, kind: "warning" });
  }

  async function switchMode(nextMode: WorkspaceMode) {
    if (nextMode === mode || workspaceOperationInProgress()) return;
    if (!await confirmWorkspaceReset("切换版本", "切换工作模式")) return;
    if (workspaceOperationInProgress()) return;
    resetWorkspaceTransientState();
    setMode(nextMode);
    if ((page === "legacy-import" && nextMode !== "offline") || (page === "users" && nextMode !== "network")) {
      setPage("overview");
    }
    setNetworkAuthNotice(null);
  }

  async function submitNetworkLogin(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (workspaceOperationInProgress()) return;
    setNetworkAuthNotice(null);
    if (!networkBaseUrl.trim() || !networkTenantId.trim() || !networkLogin.trim() || !networkPassword) {
      setNetworkAuthNotice({ type: "error", text: "请填写 API 地址、租户 ID、账号和密码。" });
      return;
    }
    setNetworkAuthLoading(true);
    try {
      const status = await invoke<NetworkStatus>("v2_network_login", {
        baseUrl: networkBaseUrl.trim(),
        tenantId: networkTenantId.trim(),
        login: networkLogin.trim(),
        password: networkPassword,
        deviceId: getNetworkDeviceId(),
      });
      resetWorkspaceTransientState();
      try {
        window.localStorage.setItem("inventory-v2-network-url", networkBaseUrl.trim().replace(/\/+$/, ""));
        window.localStorage.setItem("inventory-v2-network-tenant", networkTenantId.trim());
      } catch {
        // Endpoint preferences are optional; credentials remain memory-only.
      }
      setNetworkStatus(status);
      setNetworkPassword("");
      setNetworkAuthNotice({ type: "success", text: "网络版登录成功，已使用服务端授权和租户权限。" });
      setPage("overview");
    } catch (error) {
      setNetworkAuthNotice({ type: "error", text: `登录失败：${displayError(error)}` });
    } finally {
      setNetworkAuthLoading(false);
    }
  }

  async function logoutNetwork() {
    if (workspaceOperationInProgress()) return;
    if (!await confirmWorkspaceReset("退出团队版", "退出团队版")) return;
    if (workspaceOperationInProgress()) return;
    setNetworkAuthLoading(true);
    try {
      await invoke<NetworkStatus>("v2_network_logout");
      resetWorkspaceTransientState();
      setNetworkStatus((current) => current ? { ...current, authenticated: false, tenant_id: null, user_id: null } : current);
      setNetworkAuthNotice({ type: "success", text: "已退出网络版。" });
    } catch (error) {
      setNetworkAuthNotice({ type: "error", text: `退出失败：${displayError(error)}` });
    } finally {
      setNetworkAuthLoading(false);
    }
  }

  async function submitCatalogProduct(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setCatalogNotice(null);
    if (!newProductCode.trim() || !newProductName.trim()) {
      setCatalogNotice({ type: "error", text: "请填写商品编码和商品名称。" });
      return;
    }
    setCatalogLoading(true);
    try {
      const input: SaveCatalogProductRequest = {
        sku_id: editingProductId,
        code: newProductCode,
        name: newProductName,
        serial_prefix: newProductSerialPrefix.trim() || null,
        serial_forbidden_chars: newProductForbiddenChars,
      };
      const command = mode === "network" ? "v2_network_save_catalog_product" : "v2_save_catalog_product";
      const product = await invoke<CatalogProduct>(command, { input });
      setSelectedProductId(product.sku_id);
      setReceiptProductInput(product.code);
      setCatalogNotice({ type: "success", text: `已${editingProductId ? "更新" : "创建"}商品 ${product.code}。` });
      await refreshCatalog();
      setCatalogCreateOpen(false);
      resetCatalogDraft();
    } catch (error) {
      setCatalogNotice({ type: "error", text: `保存商品失败：${displayError(error)}` });
    } finally {
      setCatalogLoading(false);
    }
  }

  async function submitCatalogParty(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setCatalogNotice(null);
    if (!newPartyName.trim()) {
      setCatalogNotice({ type: "error", text: "请填写名称。" });
      return;
    }
    if (newPartyRoles.size === 0) {
      setCatalogNotice({ type: "error", text: "请至少选择一个角色。" });
      return;
    }
    setCatalogLoading(true);
    try {
      const optional = (value: string) => value.trim() || null;
      const input: SaveCatalogPartyRequest = {
        party_id: editingPartyId,
        display_name: newPartyName,
        roles: Array.from(newPartyRoles),
        contact_name: optional(newPartyContactName),
        phone: optional(newPartyPhone),
        wechat: optional(newPartyWechat),
        email: optional(newPartyEmail),
        address: optional(newPartyAddress),
        notes: optional(newPartyNotes),
      };
      const command = mode === "network" ? "v2_network_save_catalog_party" : "v2_save_catalog_party";
      const party = await invoke<CatalogParty>(command, { input });
      if (party.roles.includes("supplier")) {
        setSupplierName(party.display_name);
        setReceiptSupplierSuggestionsOpen(false);
      }
      setCatalogNotice({ type: "success", text: `已保存往来方 ${party.display_name}。` });
      await refreshCatalog();
      setCatalogCreateOpen(false);
      resetCatalogDraft();
    } catch (error) {
      setCatalogNotice({ type: "error", text: `保存往来方失败：${displayError(error)}` });
    } finally {
      setCatalogLoading(false);
    }
  }

  async function playScannerAlert() {
    try {
      await invoke("play_beep");
    } catch {
      // A missing output device must not prevent the visual scanner alert.
    }
  }

  async function rejectScan(text: string) {
    setReceiptNotice({ type: "error", text });
    await playScannerAlert();
    setScannerInput("");
    scannerInputRef.current?.focus();
  }

  async function validateReceiptBarcode(value: string, knownBarcodes: Set<string>): Promise<string> {
    const barcode = value.trim().toUpperCase();
    if (!barcode) throw new Error("SN 不能为空。");
    if (!selectedProduct) throw new Error("请先选择商品；未绑定商品的 SN 不允许入库。");
    if (knownBarcodes.has(barcode)) throw new Error(`SN ${barcode} 已在当前入库批次中。`);
    if (selectedProduct.serial_prefix && !barcode.startsWith(selectedProduct.serial_prefix.toUpperCase())) {
      throw new Error(`SN ${barcode} 不符合商品 ${selectedProduct.code} 的前缀规则。`);
    }
    const forbidden = parseForbiddenSerialTokens(selectedProduct.serial_forbidden_chars);
    const forbiddenToken = forbidden.find((token) => barcode.includes(token));
    if (forbiddenToken) {
      throw new Error(`SN ${barcode} 含有商品设置的禁用字符或片段 ${forbiddenToken === " " ? "空格" : forbiddenToken}。`);
    }

    const command = mode === "network"
      ? "v2_network_inventory_barcode_exists"
      : "v2_inventory_barcode_exists";
    const response = await invoke<InventoryBarcodeExistsResponse>(command, { barcode });
    if (response.exists) throw new Error(`SN ${response.barcode} 已存在于库存中，已拒绝重复入库。`);
    return response.barcode;
  }

  async function addScannedBarcode() {
    const rawBarcode = scannerInput;
    if (!receiptDetailsReady || !rawBarcode.trim() || scanCheckingRef.current) return;
    scanCheckingRef.current = true;
    setScanChecking(true);
    try {
      const barcode = await validateReceiptBarcode(rawBarcode, new Set(scannedBarcodes));
      setScannedBarcodes((current) => [...current, barcode]);
      setScannerInput("");
      setReceiptNotice({ type: "success", text: `已采集 SN ${barcode}。` });
    } catch (error) {
      await rejectScan(displayError(error));
    } finally {
      scanCheckingRef.current = false;
      setScanChecking(false);
      scannerInputRef.current?.focus();
    }
  }

  async function importReceiptBarcodes() {
    const candidates = parseBarcodeLines(receiptBulkInput);
    if (!receiptDetailsReady || candidates.length === 0 || scanCheckingRef.current) return;
    scanCheckingRef.current = true;
    setScanChecking(true);
    try {
      const known = new Set(scannedBarcodes);
      const validated: string[] = [];
      for (const candidate of candidates) {
        const barcode = await validateReceiptBarcode(candidate, known);
        known.add(barcode);
        validated.push(barcode);
      }
      setScannedBarcodes((current) => [...current, ...validated]);
      setReceiptBulkInput("");
      setReceiptNotice({ type: "success", text: `备用批量录入已校验并加入 ${validated.length} 个 SN。` });
    } catch (error) {
      await rejectScan(`备用批量录入已取消：${displayError(error)}`);
    } finally {
      scanCheckingRef.current = false;
      setScanChecking(false);
      scannerInputRef.current?.focus();
    }
  }

  function removeScannedBarcode(barcode: string) {
    setScannedBarcodes((current) => current.filter((item) => item !== barcode));
    setReceiptNotice({ type: "success", text: `已从当前批次移除 SN ${barcode}。` });
    scannerInputRef.current?.focus();
  }

  async function clearScannedBatch() {
    if (scannedBarcodes.length === 0 || receiptLoading) return;
    const approved = await confirm(
      `确定清空当前批次已采集的 ${scannedBarcodes.length} 个 SN 吗？`,
      { title: "清空入库批次", kind: "warning" },
    );
    if (!approved) return;
    setScannedBarcodes([]);
    setScannerInput("");
    setReceiptNotice({ type: "success", text: "当前入库批次已清空。" });
    scannerInputRef.current?.focus();
  }

  async function submitReceipt(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setReceiptNotice(null);
    if (receiptCompleted) return;
    if (!receiptDetailsReady || !selectedProduct) {
      setReceiptNotice({ type: "error", text: `入库资料不完整：请补充${receiptMissingDetails.join("、")}。` });
      return;
    }
    if (barcodes.length === 0) {
      setReceiptNotice({ type: "error", text: "请至少扫描一个 SN。" });
      return;
    }

    setReceiptLoading(true);
    try {
      const operationId = createId();
      const warranty = makeWarrantyInput(
        receiptWarrantyPreset,
        receiptWarrantyCustomDays,
        receiptWarrantyManualStart,
        receiptWarrantyStartsAt,
      );
      const common = {
        request_id: operationId,
        idempotency_key: `receipt:${operationId}`,
        receipt_no: makeDocumentNumber("RK"),
        owner_name: supplierName.trim(),
        supplier_name: supplierName.trim(),
        sku_code: selectedProduct.code,
        sku_name: selectedProduct.name,
        source_reference: sourceReference.trim() || null,
        received_at: toUtcIso(receivedAt),
        barcodes,
        notes: null,
        warranty,
      };
      let response: PostReceiptResponse;
      if (mode === "network") {
        if (!networkWarehouses.some((warehouse) => warehouse.warehouse_id === networkWarehouseId)) {
          throw new Error("请选择当前团队工作区中可用的入库仓库");
        }
        response = await invoke<PostReceiptResponse>("v2_network_post_receipt", {
          input: { ...common, warehouse_id: networkWarehouseId.trim() } satisfies NetworkPostReceiptRequest,
        });
      } else {
        response = await invoke<PostReceiptResponse>("v2_post_receipt", {
          input: { ...common, actor_id: resolvedActorId } satisfies PostReceiptRequest,
        });
      }
      setReceiptNotice({
        type: "success",
        text: `${response.receipt_no} 已原子入库 ${response.received_count} 件${
          response.idempotent_replay ? "（幂等回放）" : ""
        }。新入库单件默认标记为未测试。`,
      });
      setReceiptCompleted(response);
      setScannerInput("");
      setSourceReference("");
      setReceivedAt(getLocalDateTimeValue());
      setReceiptWarrantyPreset("");
      setReceiptWarrantyCustomDays("");
      setReceiptWarrantyManualStart(false);
      setReceiptStep(3);
      void refreshDashboard();
    } catch (error) {
      await rejectScan(`入库失败：${displayError(error)}`);
    } finally {
      setReceiptLoading(false);
      window.requestAnimationFrame(() => scannerInputRef.current?.focus());
    }
  }

  function toggleQualityBarcode(barcode: string) {
    if (qualityLoading || qualityScanCheckingRef.current) return;
    setSelectedBarcodes((current) => {
      const next = new Set(current);
      if (next.has(barcode)) next.delete(barcode);
      else next.add(barcode);
      return next;
    });
    setQualityScanNotice({ type: "success", text: `已通过备用列表更新 SN ${barcode}。` });
    qualityScannerInputRef.current?.focus();
  }

  async function changeInspectionKind(nextKind: InspectionKind) {
    if (nextKind === inspectionKind || qualityLoading || qualityScanCheckingRef.current) return;
    if (selectedBarcodes.size > 0) {
      const approved = await confirm(
        `切换质检类型会清空当前已扫描的 ${selectedBarcodes.size} 个 SN，确定继续吗？`,
        { title: "切换质检类型", kind: "warning" },
      );
      if (!approved) return;
    }
    setInspectionKind(nextKind);
  }

  async function rejectQualityScan(text: string) {
    setQualityScanNotice({ type: "error", text });
    await playScannerAlert();
    setQualityScannerInput("");
    qualityScannerInputRef.current?.focus();
  }

  async function validateQualityBarcode(value: string, knownBarcodes: Set<string>): Promise<InventoryTrace> {
    const barcode = value.trim().toUpperCase();
    if (!barcode) throw new Error("SN 不能为空。");
    if (knownBarcodes.has(barcode)) throw new Error(`SN ${barcode} 已在当前质检批次中。`);
    const command = mode === "network" ? "v2_network_inventory_trace" : "v2_inventory_trace";
    const trace = await invoke<InventoryTrace>(command, { barcode });
    if (!isInspectionEligible(trace, inspectionKind)) {
      const required = inspectionKind === "initial" ? "待检入库的未测试库存" : "隔离区内的待复检库存";
      throw new Error(`SN ${trace.barcode} 当前为“${inventoryStatusLabels[trace.inventory_status]} / ${displayedQualityStatusLabels[trace.quality_status]}”，不属于${required}。`);
    }
    return trace;
  }

  async function addQualityScannedBarcode() {
    const rawBarcode = qualityScannerInput;
    if (!rawBarcode.trim() || qualityScanCheckingRef.current) return;
    qualityScanCheckingRef.current = true;
    setQualityScanChecking(true);
    try {
      const trace = await validateQualityBarcode(rawBarcode, selectedBarcodes);
      setSelectedBarcodes((current) => new Set([...current, trace.barcode.toUpperCase()]));
      setQualityScannerInput("");
      setQualityScanNotice({
        type: "success",
        text: `已加入 ${trace.barcode} · ${trace.sku_code} ${trace.sku_name} · ${trace.owner_name}。`,
      });
    } catch (error) {
      await rejectQualityScan(`质检扫码已拒绝：${displayError(error)}`);
    } finally {
      qualityScanCheckingRef.current = false;
      setQualityScanChecking(false);
      qualityScannerInputRef.current?.focus();
    }
  }

  async function importQualityBarcodes() {
    const candidates = parseBarcodeLines(qualityBulkInput);
    if (candidates.length === 0 || qualityScanCheckingRef.current) return;
    qualityScanCheckingRef.current = true;
    setQualityScanChecking(true);
    try {
      const known = new Set(selectedBarcodes);
      const validated: string[] = [];
      for (const candidate of candidates) {
        const trace = await validateQualityBarcode(candidate, known);
        const barcode = trace.barcode.toUpperCase();
        known.add(barcode);
        validated.push(barcode);
      }
      setSelectedBarcodes(known);
      setQualityBulkInput("");
      setQualityScanNotice({ type: "success", text: `备用批量录入已校验并加入 ${validated.length} 个 SN。` });
    } catch (error) {
      await rejectQualityScan(`备用批量录入已取消：${displayError(error)}`);
    } finally {
      qualityScanCheckingRef.current = false;
      setQualityScanChecking(false);
      qualityScannerInputRef.current?.focus();
    }
  }

  function removeQualityBarcode(barcode: string) {
    if (qualityLoading || qualityScanCheckingRef.current) return;
    setSelectedBarcodes((current) => {
      const next = new Set(current);
      next.delete(barcode);
      return next;
    });
    setQualityScanNotice({ type: "success", text: `已从质检批次移除 SN ${barcode}。` });
    qualityScannerInputRef.current?.focus();
  }

  async function clearQualityBatch() {
    if (selectedBarcodes.size === 0 || qualityLoading || qualityScanCheckingRef.current) return;
    const approved = await confirm(
      `确定清空当前质检批次的 ${selectedBarcodes.size} 个 SN 吗？`,
      { title: "清空质检批次", kind: "warning" },
    );
    if (!approved) return;
    setSelectedBarcodes(new Set());
    setQualityScanNotice({ type: "success", text: "当前质检扫码批次已清空。" });
    qualityScannerInputRef.current?.focus();
  }

  async function submitInspection(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (qualityLoading || qualityScanCheckingRef.current) return;
    setQualityNotice(null);
    if (selectedBarcodes.size === 0) {
      setQualityNotice({ type: "error", text: "请至少选择一件待检库存。" });
      return;
    }
    if (!selectedQualityLabel) {
      setQualityNotice({ type: "error", text: "请先选择质检标签；没有可用标签时请打开标签管理窗口创建。" });
      return;
    }
    const inspectionOutcome: QualityOutcome = selectedQualityLabel.disposition === "available" ? "passed" : "failed";
    if (selectedQualityLabel.disposition === "quarantine" && !defectCode.trim() && !inspectionNotes.trim()) {
      setQualityNotice({ type: "error", text: `标签“${selectedQualityLabel.name}”会进入隔离区，请填写缺陷代码或备注。` });
      return;
    }

    const operationId = createId();
    const common = {
      request_id: operationId,
      idempotency_key: `inspection:${operationId}`,
      inspection_no: makeDocumentNumber("ZJ"),
      inspection_kind: inspectionKind,
      inspected_at: new Date().toISOString(),
      results: Array.from(selectedBarcodes).map((barcode) => ({
        barcode,
        outcome: inspectionOutcome,
        quality_label_id: selectedQualityLabel.quality_label_id,
        defect_code: defectCode.trim() || null,
        measurements: {},
        notes: inspectionNotes.trim() || null,
      })),
    };
    let response: CompleteInspectionResponse;
    setQualityLoading(true);
    try {
      if (mode === "network") {
        response = await invoke<CompleteInspectionResponse>("v2_network_complete_inspection", {
          input: common satisfies NetworkCompleteInspectionRequest,
        });
      } else {
        response = await invoke<CompleteInspectionResponse>("v2_complete_inspection", {
          input: { ...common, inspector_id: resolvedActorId } satisfies CompleteInspectionRequest,
        });
      }
    } catch (error) {
      const message = `质检提交失败：${displayError(error)}`;
      setQualityNotice({ type: "error", text: message });
      await rejectQualityScan(message);
      setQualityLoading(false);
      return;
    }

    const completionText = `${response.inspection_no} 已完成：${selectedQualityLabel.name} ${response.inspected_count} 件${
      response.idempotent_replay ? "（幂等回放）" : ""
    }。`;
    setDefectCode("");
    setInspectionNotes("");
    setSelectedBarcodes(new Set());
    setQualityStep(1);
    setQualityScannerInput("");
    setQualityBulkInput("");
    setQualityScanNotice(null);
    try {
      const listCommand = mode === "network" ? "v2_network_list_inventory" : "v2_list_inventory";
      const listResponse = await invoke<InventoryListResponse>(listCommand, { query: emptyInventoryQuery() });
      setQualityItems(
        listResponse.items.filter(
          (item) => isInspectionEligible(item, "initial") || isInspectionEligible(item, "retest"),
        ),
      );
      setQualityNotice({ type: "success", text: completionText });
    } catch (error) {
      setQualityNotice({
        type: "warning",
        text: `${completionText} 但待检列表刷新失败：${displayError(error)}。请手动刷新列表。`,
      });
    }
    setQualityLoading(false);
    void refreshDashboard();
  }

  function beginOutboundScan(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setOutboundNotice(null);
    if (!outboundReceiver.trim()) {
      setOutboundNotice({ type: "error", text: "请填写上游收货方。" });
      return;
    }
    setOutboundOrder(null);
    setOutboundAllocation(null);
    setOutboundShipment(null);
    setOutboundResolved(false);
    setOutboundScannedItems([]);
    setOutboundReceiverSuggestionsOpen(false);
    setOutboundScannerInput("");
    setOutboundBulkInput("");
    setOutboundShipmentNo("");
    setOutboundScanNotice({ type: "success", text: "已进入扫码出库，请扫描实际出货单件；结束扫码时以实际件数确定需求数量。" });
    setOutboundStep(2);
  }

  async function loadOutboundScanItem(barcode: string): Promise<OutboundScannedItem> {
    const command = mode === "network" ? "v2_network_inventory_trace" : "v2_inventory_trace";
    const trace = await invoke<InventoryTrace>(command, { barcode });
    if (trace.inventory_status !== "available" || (trace.quality_status !== "passed" && trace.quality_status !== "waived")) {
      throw new Error(`SN ${trace.barcode} 当前为“${inventoryStatusLabels[trace.inventory_status]} / ${displayedQualityStatusLabels[trace.quality_status]}”，不可出库。`);
    }
    return {
      barcode: trace.barcode.toUpperCase(),
      skuId: trace.sku_id,
      skuCode: trace.sku_code,
      skuName: trace.sku_name,
    };
  }

  async function completeOutboundScanAndShip(items: OutboundScannedItem[] = outboundScannedItems) {
    if (outboundLoading || outboundShipment) return;
    const quantity = items.length;
    if (quantity === 0) {
      setOutboundNotice({ type: "error", text: "请至少扫描一个实际出货 SN，再结束扫码。" });
      return;
    }
    if (!outboundReceiver.trim()) {
      setOutboundNotice({ type: "error", text: "请先填写上游收货方。" });
      return;
    }
    setOutboundLoading(true);
    setOutboundNotice(null);
    try {
      let order = outboundOrder;
      if (!order) {
        const first = items[0];
        const operationId = createId();
        const common = {
          request_id: operationId,
          idempotency_key: `outbound-order:${operationId}`,
          order_no: makeDocumentNumber("DD"),
          upstream_receiver_name: outboundReceiver.trim(),
          sku_code: first.skuCode,
          sku_name: first.skuName,
          required_quantity: quantity,
          required_at: null,
        };
        const command = mode === "network" ? "v2_network_create_outbound_order" : "v2_create_outbound_order";
        const input = mode === "network"
          ? (common satisfies NetworkCreateOutboundOrderRequest)
          : ({ ...common, actor_id: resolvedActorId } satisfies CreateOutboundOrderRequest);
        order = await invoke<CreateOutboundOrderResponse>(command, { input });
        setOutboundOrder(order);
        void refreshCatalog();
      }

      let allocation = outboundAllocation;
      if (!allocation) {
        const operationId = createId();
        const common = {
          request_id: operationId,
          idempotency_key: `outbound-allocation:${operationId}`,
          order_id: order.order_id,
          order_line_id: order.order_line_id,
          barcodes: items.map((item) => item.barcode),
          allow_mixed_skus: true,
        };
        const command = mode === "network" ? "v2_network_allocate_outbound_order" : "v2_allocate_outbound_order";
        const input = mode === "network"
          ? (common satisfies NetworkAllocateOutboundRequest)
          : ({ ...common, actor_id: resolvedActorId });
        allocation = await invoke<AllocateOutboundResponse>(command, { input });
        setOutboundAllocation(allocation);
      }

      const operationId = createId();
      const warranty = makeWarrantyInput(
        outboundWarrantyPreset,
        outboundWarrantyCustomDays,
        outboundWarrantyManualStart,
        outboundWarrantyStartsAt,
      );
      const common = {
        request_id: operationId,
        idempotency_key: `outbound-shipment:${operationId}`,
        order_id: order.order_id,
        shipment_no: outboundShipmentNo.trim() || makeDocumentNumber("CK"),
        allocation_ids: [],
        barcodes: items.map((item) => item.barcode),
        shipped_at: new Date().toISOString(),
        warranty,
      };
      const command = mode === "network" ? "v2_network_ship_outbound_order" : "v2_ship_outbound_order";
      const input = mode === "network"
        ? (common satisfies NetworkShipOutboundRequest)
        : ({ ...common, actor_id: resolvedActorId });
      const shipment = await invoke<ShipOutboundResponse>(command, { input });
      setOutboundShipment(shipment);
      setOutboundStep(3);
      setOutboundShipmentNo(shipment.shipment_no);
      const groupCount = new Set(items.map((item) => item.skuId)).size;
      setOutboundNotice({ type: "success", text: `${shipment.shipment_no} 已按扫码结果自动归类 ${groupCount} 个品牌/型号，并成功出库 ${shipment.shipped_count} 件。` });
      setOutboundScanNotice({ type: "success", text: `服务端已按实际扫描的 ${shipment.shipped_count} 个 SN 完成原子出库。` });
      void refreshDashboard();
    } catch (error) {
      const message = `扫码确认后自动出库失败：${displayError(error)}`;
      setOutboundNotice({ type: "error", text: message });
      await playScannerAlert();
      outboundScannerInputRef.current?.focus();
    } finally {
      setOutboundLoading(false);
    }
  }

  async function rejectOutboundScan(text: string) {
    setOutboundScanNotice({ type: "error", text });
    setOutboundScannerInput("");
    await playScannerAlert();
    outboundScannerInputRef.current?.focus();
  }

  async function addOutboundScannedBarcode() {
    const rawBarcode = outboundScannerInput;
    if (!rawBarcode.trim() || outboundScanCheckingRef.current || outboundLoading || outboundShipment) return;
    outboundScanCheckingRef.current = true;
    setOutboundScanChecking(true);
    setOutboundScannerInput("");
    try {
      const candidates = parseBarcodeLines(rawBarcode);
      if (candidates.length !== 1) throw new Error("主扫描区每次只接收一个 SN；多件录入请使用备用录入。");
      const barcode = candidates[0].toUpperCase();
      if (outboundScannedItems.some((item) => item.barcode === barcode)) throw new Error(`SN ${barcode} 已经扫描过，请勿重复扫描。`);
      const item = await loadOutboundScanItem(barcode);
      const nextItems = [...outboundScannedItems, item];
      setOutboundScannedItems(nextItems);
      setOutboundScanNotice({ type: "success", text: `SN ${barcode} 核验通过，当前已扫描 ${nextItems.length} 件；确认结束扫码后按此数量出库。` });
    } catch (error) {
      await rejectOutboundScan(`出库扫码已拒绝：${displayError(error)}`);
    } finally {
      outboundScanCheckingRef.current = false;
      setOutboundScanChecking(false);
    }
  }

  async function importOutboundBarcodes() {
    const candidates = parseBarcodeLines(outboundBulkInput);
    if (candidates.length === 0 || outboundScanCheckingRef.current || outboundLoading || outboundShipment) return;
    outboundScanCheckingRef.current = true;
    setOutboundScanChecking(true);
    setOutboundBulkInput("");
    try {
      const known = new Set(outboundScannedItems.map((item) => item.barcode));
      const nextItems = [...outboundScannedItems];
      for (const value of candidates) {
        const barcode = value.toUpperCase();
        if (known.has(barcode)) throw new Error(`SN ${barcode} 已经扫描过，请勿重复扫描。`);
        const item = await loadOutboundScanItem(barcode);
        known.add(barcode);
        nextItems.push(item);
      }
      setOutboundScannedItems(nextItems);
      setOutboundScanNotice({ type: "success", text: `备用录入已核验并加入 ${candidates.length} 个 SN，当前共 ${nextItems.length} 件；确认结束扫码后按此数量出库。` });
    } catch (error) {
      await rejectOutboundScan(`备用录入已拒绝：${displayError(error)}`);
    } finally {
      outboundScanCheckingRef.current = false;
      setOutboundScanChecking(false);
    }
  }

  function removeOutboundScannedBarcode(barcode: string) {
    if (outboundScanCheckingRef.current || outboundLoading || outboundShipment) return;
    setOutboundScannedItems((current) => current.filter((item) => item.barcode !== barcode));
    setOutboundScanNotice({ type: "success", text: `已从出库扫码批次移除 SN ${barcode}。` });
    outboundScannerInputRef.current?.focus();
  }

  async function clearOutboundScanBatch() {
    if (outboundScannedItems.length === 0 || outboundScanCheckingRef.current || outboundLoading || outboundShipment) return;
    const approved = await confirm(
      `确定清空已核验的 ${outboundScannedItems.length} 个出库 SN 吗？`,
      { title: "清空出库扫码", kind: "warning" },
    );
    if (!approved) return;
    setOutboundScannedItems([]);
    setOutboundScanNotice({ type: "success", text: "当前出库扫码批次已清空。" });
    outboundScannerInputRef.current?.focus();
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
      const common = {
        request_id: operationId,
        idempotency_key: `outbound-delivery:${operationId}`,
        shipment_id: outboundShipment.shipment_id,
        confirmation_code: outboundConfirmationCode.trim(),
        shipment_line_ids: [],
        confirmed_at: new Date().toISOString(),
        notes: null,
      };
      const command = mode === "network" ? "v2_network_confirm_outbound_delivery" : "v2_confirm_outbound_delivery";
      const input = mode === "network"
        ? (common satisfies NetworkConfirmOutboundDeliveryRequest)
        : ({ ...common, confirmed_by: resolvedActorId });
      const response = await invoke<ConfirmOutboundDeliveryResponse>(command, { input });
      setOutboundNotice({ type: "success", text: `已确认交货 ${response.delivered_count} 件，批次状态：${response.shipment_status}。` });
      setOutboundResolved(true);
      void refreshDashboard();
    } catch (error) {
      setOutboundNotice({ type: "error", text: `交货确认失败：${displayError(error)}` });
    } finally {
      setOutboundLoading(false);
    }
  }

  function startNextOutboundOrder() {
    setOutboundOrder(null);
    setOutboundAllocation(null);
    setOutboundShipment(null);
    setOutboundResolved(false);
    setOutboundReceiver("");
    setOutboundReceiverSuggestionsOpen(false);
    setOutboundScannerInput("");
    setOutboundBulkInput("");
    setOutboundScannedItems([]);
    setOutboundShipmentNo("");
    setOutboundConfirmationCode("");
    setOutboundWarrantyPreset("");
    setOutboundWarrantyCustomDays("");
    setOutboundWarrantyManualStart(false);
    setOutboundWarrantyStartsAt(getLocalDateTimeValue());
    setOutboundScanNotice(null);
    setOutboundNotice(null);
    setOutboundStep(1);
    void refreshCatalog();
  }

  async function createOfflineBackup() {
    if (!beginDataOperation()) return;
    setDataNotice(null);
    try {
      const stamp = new Date().toISOString().replace(/[-:TZ.]/g, "").slice(0, 14);
      const destination = await save({
        title: "保存离线备份",
        defaultPath: `inventory-backup-${stamp}.invbackup`,
      });
      if (!destination) return;
      const metadata = await invoke<BackupMetadata>("v2_create_offline_backup", { destination });
      setDataNotice({
        type: "success",
        text: `备份已完成：${destination}（${metadata.database_bytes.toLocaleString()} 字节，SHA-256 ${metadata.database_sha256.slice(0, 12)}…）`,
      });
    } catch (error) {
      setDataNotice({ type: "error", text: `备份失败：${displayError(error)}` });
    } finally {
      endDataOperation();
    }
  }

  async function restoreOfflineBackup() {
    if (!beginDataOperation()) return;
    setDataNotice(null);
    try {
      const selected = await open({
        title: "选择离线备份包",
        directory: true,
        multiple: false,
      });
      if (!selected || Array.isArray(selected)) return;
      const metadata = await invoke<BackupMetadata>("v2_verify_offline_backup", {
        packagePath: selected,
      });
      const approved = await confirm(
        `将当前离线库恢复到 ${formatDateTime(metadata.exported_at)} 的状态。应用会重启，当前数据库会先生成保护性备份。`,
        { title: "确认恢复离线数据", kind: "warning" },
      );
      if (!approved) return;
      setDataNotice({ type: "success", text: "备份校验通过，正在重启并原子恢复…" });
      await invoke("v2_restore_offline_backup", { packagePath: selected });
    } catch (error) {
      setDataNotice({ type: "error", text: `恢复失败：${displayError(error)}` });
    } finally {
      endDataOperation();
    }
  }

  async function exportUpgradePackage() {
    if (!beginDataOperation()) return;
    setDataNotice(null);
    try {
      const stamp = new Date().toISOString().replace(/[-:TZ.]/g, "").slice(0, 14);
      const destination = await save({
        title: "保存一次性升级包",
        defaultPath: `inventory-upgrade-${stamp}.invpack`,
      });
      if (!destination) return;
      const output = await invoke<UpgradeExportOutput>("v2_export_upgrade_package", {
        input: {
          destination,
          export_id: createId(),
          exported_at: new Date().toISOString(),
        },
      });
      setUpgradeExport(output);
      setUpgradePackagePath(output.path);
      setDataNotice({
        type: "success",
        text: `升级包已验证：${output.path}（checksum ${output.checksum.slice(0, 12)}…）`,
      });
    } catch (error) {
      setDataNotice({ type: "error", text: `升级包导出失败：${displayError(error)}` });
    } finally {
      endDataOperation();
    }
  }

  async function chooseUpgradePackage() {
    if (!beginDataOperation()) return;
    try {
      const selected = await open({
        title: "选择一次性升级包",
        directory: true,
        multiple: false,
      });
      if (selected && !Array.isArray(selected)) setUpgradePackagePath(selected);
    } catch (error) {
      setDataNotice({ type: "error", text: `选择升级包失败：${displayError(error)}` });
    } finally {
      endDataOperation();
    }
  }

  async function upgradeOfflineToNetwork() {
    setDataNotice(null);
    if (!networkStatus?.authenticated) {
      setDataNotice({ type: "error", text: "请先切换到网络版并登录有升级权限的账号。" });
      return;
    }
    if (!upgradePackagePath.trim() || !upgradeTargetWorkspaceId.trim()) {
      setDataNotice({ type: "error", text: "请选择升级包并填写目标工作区 ID。" });
      return;
    }
    if (!beginDataOperation()) return;
    try {
      const approved = await confirm(
        "导入成功后，当前离线工作区将永久冻结为只读，网络 PostgreSQL 成为唯一事实源。该操作不会启用双向同步。",
        { title: "确认一次性升级", kind: "warning" },
      );
      if (!approved) return;
      const output = await invoke<UpgradeImportOutput>("v2_upgrade_offline_to_network", {
        input: {
          package_path: upgradePackagePath.trim(),
          target_workspace_id: upgradeTargetWorkspaceId.trim(),
        },
      });
      setUpgradeImport(output);
      setDataNotice({
        type: "success",
        text: `网络导入${output.import.status === "already_imported" ? "已幂等确认" : "成功"}，本地工作区${output.local_archived ? "已冻结为只读" : "尚未冻结"}。`,
      });
    } catch (error) {
      setDataNotice({ type: "error", text: `一次性升级失败：${displayError(error)}` });
    } finally {
      endDataOperation();
    }
  }

  function openOverviewShortcutEditor() {
    const available = availableOverviewShortcutIds(mode);
    const current = overviewShortcutPreferences[mode].filter((pageId) => available.has(pageId));
    setOverviewShortcutDraft(current.length > 0 ? current : defaultOverviewShortcuts.filter((pageId) => available.has(pageId)));
    setOverviewShortcutEditorOpen(true);
  }

  function toggleOverviewShortcut(pageId: WorkspacePage, checked: boolean) {
    setOverviewShortcutDraft((current) => {
      if (!checked) return current.filter((item) => item !== pageId);
      if (current.includes(pageId) || current.length >= overviewShortcutLimit) return current;
      return [...current, pageId];
    });
  }

  function moveOverviewShortcut(pageId: WorkspacePage, direction: -1 | 1) {
    setOverviewShortcutDraft((current) => {
      const index = current.indexOf(pageId);
      const target = index + direction;
      if (index < 0 || target < 0 || target >= current.length) return current;
      const next = [...current];
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
  }

  function resetOverviewShortcutDraft() {
    const available = availableOverviewShortcutIds(mode);
    setOverviewShortcutDraft(defaultOverviewShortcuts.filter((pageId) => available.has(pageId)));
  }

  function saveOverviewShortcuts() {
    if (overviewShortcutDraft.length === 0) return;
    const saved = [...overviewShortcutDraft];
    setOverviewShortcutPreferences((current) => ({ ...current, [mode]: saved }));
    try {
      window.localStorage.setItem(overviewShortcutStorageKey(mode), JSON.stringify(saved));
    } catch {
      // The current session still keeps the preference when local storage is unavailable.
    }
    setOverviewShortcutEditorOpen(false);
  }

  function updateSearchClearPreference(key: SearchClearPreferenceKey, checked: boolean) {
    const next = { ...searchClearPreferences, [key]: checked };
    setSearchClearPreferences(next);
    try {
      window.localStorage.setItem(searchClearPreferencesStorageKey, JSON.stringify(next));
    } catch {
      // Keep the preference for this session when local storage is unavailable.
    }
  }

  function resetSearchClearPreferences() {
    const next = { ...defaultSearchClearPreferences };
    setSearchClearPreferences(next);
    try {
      window.localStorage.setItem(searchClearPreferencesStorageKey, JSON.stringify(next));
    } catch {
      // Keep the preference for this session when local storage is unavailable.
    }
  }

  function overviewShortcutDescription(pageId: WorkspacePage, onHandUnits: number): string {
    if (pageId === "quality") return `${dashboard?.quality.untested ?? 0} 件待检`;
    if (pageId === "inventory") return `${onHandUnits} 件当前在库`;
    if (pageId === "receipt") return "新建扫码批次";
    if (pageId === "outbound") return "凑单与交货";
    return navigationItems.find((item) => item.id === pageId)?.description ?? "";
  }

  function renderNetworkLogin() {
    return (
      <section className="v2-page v2-network-gate" aria-labelledby="v2-network-login-title">
        <div className="v2-page-heading">
          <div>
            <span className="v2-eyebrow">团队版</span>
            <h2 id="v2-network-login-title">连接库存服务</h2>
            <p>登录由服务端验证账号、租户、角色和有效授权；桌面端不会保存 PostgreSQL 凭据。</p>
          </div>
        </div>
        <form className="v2-panel v2-network-login-form" onSubmit={submitNetworkLogin}>
          <label><span>API 地址 *</span><input value={networkBaseUrl} onChange={(event) => setNetworkBaseUrl(event.target.value)} placeholder="https://inventory.example" autoComplete="url" /></label>
          <label><span>租户 ID *</span><input value={networkTenantId} onChange={(event) => setNetworkTenantId(event.target.value)} placeholder="UUID" autoComplete="organization" /></label>
          <label><span>账号 *</span><input value={networkLogin} onChange={(event) => setNetworkLogin(event.target.value)} autoComplete="username" /></label>
          <label><span>密码 *</span><input type="password" value={networkPassword} onChange={(event) => setNetworkPassword(event.target.value)} autoComplete="current-password" /></label>
          {networkAuthNotice && <div className={`v2-notice ${networkAuthNotice.type}`}>{networkAuthNotice.text}</div>}
          <div className="v2-form-actions">
          <button className="v2-button" type="button" onClick={() => void switchMode("offline")} disabled={modeSwitchDisabled}>{offlineActivated ? "切换本机版" : "切换本机只读"}</button>
            <button className="v2-button primary" type="submit" disabled={networkAuthLoading}>{networkAuthLoading ? "正在登录…" : "登录团队版"}</button>
          </div>
        </form>
      </section>
    );
  }

  function renderOverview() {
    const inventory = dashboard?.inventory;
    const quality = dashboard?.quality;
    const products = dashboard?.products ?? [];
    const onHandUnits = products.reduce((total, product) => total + product.on_hand_units, 0);
    const selectedProduct = products.find((product) => product.sku_id === selectedOverviewSkuId) ?? products[0] ?? null;
    const availableShortcutIds = availableOverviewShortcutIds(mode);
    const shortcutItems = overviewShortcutPreferences[mode]
      .filter((pageId) => availableShortcutIds.has(pageId))
      .flatMap((pageId) => {
        const item = navigationItems.find((candidate) => candidate.id === pageId);
        return item ? [item] : [];
      });
    const shortcutChoices = navigationItems.filter((item) => (
      item.id !== "overview" && (!item.mode || item.mode === mode)
    ));
    const orderedShortcutChoices = [
      ...overviewShortcutDraft.flatMap((pageId) => {
        const item = shortcutChoices.find((candidate) => candidate.id === pageId);
        return item ? [item] : [];
      }),
      ...shortcutChoices.filter((item) => !overviewShortcutDraft.includes(item.id)),
    ];
    return (
      <section className="v2-page" aria-labelledby="v2-overview-title">
        <div className="v2-page-heading">
          <div>
            <span className="v2-eyebrow">库存工作台</span>
            <h2 id="v2-overview-title">业务概览</h2>
            <p>查看在库商品、供应商分布和作业状态。</p>
          </div>
          <button className="v2-button" type="button" onClick={() => void refreshDashboard()} disabled={dashboardLoading}>
            <RefreshCw size={16} className={dashboardLoading ? "v2-spin" : ""} /> 刷新
          </button>
        </div>
        {dashboardError && <div className="v2-notice error">读取概览失败：{dashboardError}</div>}
        <section className="v2-overview-shortcuts" aria-label="常用作业">
          <header><strong>常用作业</strong><button className="v2-icon-button" type="button" onClick={openOverviewShortcutEditor} aria-label="自定义常用作业" title="自定义常用作业"><SlidersHorizontal size={16} /></button></header>
          <div className="v2-overview-shortcut-list">
            {shortcutItems.map((item) => {
              const ShortcutIcon = item.icon;
              return <button key={item.id} type="button" onClick={() => navigateToPage(item.id)} disabled={modeSwitchDisabled}><ShortcutIcon size={18} /><span><b>{item.label}</b><small>{overviewShortcutDescription(item.id, onHandUnits)}</small></span></button>;
            })}
          </div>
        </section>
        <div className="v2-metric-grid" aria-busy={dashboardLoading}>
          <article className="v2-metric-card primary"><span>当前在库</span><strong>{dashboard ? onHandUnits : "—"}</strong><small>{dashboard ? `${products.length} 种商品，系统累计追踪 ${dashboard.total_units} 件` : "待检、可用、预留与隔离库存"}</small></article>
          <article className="v2-metric-card"><span>可用库存</span><strong>{inventory?.available ?? "—"}</strong><small>{displayedQualityStatusLabels.passed}，可参与凑单</small></article>
          <article className="v2-metric-card warning"><span>待检库存</span><strong>{quality?.untested ?? "—"}</strong><small>入库后尚未完成初检</small></article>
          <article className="v2-metric-card danger"><span>隔离库存</span><strong>{inventory?.quarantined ?? "—"}</strong><small>{displayedQualityStatusLabels.failed}或退回待复检</small></article>
        </div>
        <section className="v2-panel v2-overview-stock" aria-labelledby="v2-overview-stock-title">
          <header className="v2-overview-stock-heading">
            <div><h3 id="v2-overview-stock-title">在库商品</h3><small>待检、可用、预留和隔离中的实物</small></div>
            <span>{products.length} 种 · {onHandUnits} 件</span>
          </header>
          <div className="v2-overview-stock-layout">
            <div className="v2-overview-product-table-wrap">
              <table className="v2-overview-product-table">
                <thead><tr><th>商品</th><th>在库</th><th>可用</th><th>待检</th><th>预留</th><th>隔离</th></tr></thead>
                <tbody>
                  {!dashboardLoading && products.length === 0 && <tr><td className="v2-table-empty" colSpan={6}>当前没有在库商品</td></tr>}
                  {products.map((product) => <tr className={selectedProduct?.sku_id === product.sku_id ? "selected" : ""} key={product.sku_id}>
                    <td><button className="v2-overview-product-select" type="button" onClick={() => setSelectedOverviewSkuId(product.sku_id)} aria-pressed={selectedProduct?.sku_id === product.sku_id}><strong>{product.sku_code}</strong><span>{product.sku_name}</span></button></td>
                    <td><strong>{product.on_hand_units}</strong></td>
                    <td>{product.inventory.available}</td>
                    <td>{product.inventory.received}</td>
                    <td>{product.inventory.reserved}</td>
                    <td>{product.inventory.quarantined}</td>
                  </tr>)}
                </tbody>
              </table>
            </div>
            <aside className="v2-overview-supplier-detail" aria-live="polite">
              {selectedProduct ? <>
                <header><div><span>供应商分布</span><h4>{selectedProduct.sku_code} · {selectedProduct.sku_name}</h4></div><strong>{selectedProduct.on_hand_units} 件</strong></header>
                <div className="v2-overview-supplier-table-wrap"><table><thead><tr><th>供应商</th><th>在库</th><th>可用</th><th>待检</th><th>预留</th><th>隔离</th></tr></thead><tbody>
                  {selectedProduct.suppliers.map((supplier) => <tr key={supplier.supplier_party_id ?? supplier.supplier_name}><td>{supplier.supplier_name}</td><td><strong>{supplier.on_hand_units}</strong></td><td>{supplier.inventory.available}</td><td>{supplier.inventory.received}</td><td>{supplier.inventory.reserved}</td><td>{supplier.inventory.quarantined}</td></tr>)}
                </tbody></table></div>
              </> : <div className="v2-overview-stock-empty"><Boxes size={28} /><span>没有可显示的供应商库存</span></div>}
            </aside>
          </div>
        </section>
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
              <span>{displayedQualityStatusLabels.passed} <strong>{quality?.passed ?? 0}</strong></span>
              <span>{displayedQualityStatusLabels.failed} <strong>{quality?.failed ?? 0}</strong></span>
              <span>测试中 <strong>{quality?.testing ?? 0}</strong></span>
              <span>例外放行 <strong>{quality?.waived ?? 0}</strong></span>
            </div>
          </article>
        </div>
        {overviewShortcutEditorOpen && <div className="v2-catalog-modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) setOverviewShortcutEditorOpen(false); }} onKeyDown={(event) => { if (event.key === "Escape") setOverviewShortcutEditorOpen(false); }}>
          <section className="v2-catalog-modal v2-shortcut-modal" role="dialog" aria-modal="true" aria-labelledby="v2-shortcut-modal-title" tabIndex={-1}>
            <header><div><span>概览设置</span><h3 id="v2-shortcut-modal-title">自定义常用作业</h3></div><button className="v2-icon-button" type="button" onClick={() => setOverviewShortcutEditorOpen(false)} aria-label="关闭" title="关闭"><X size={18} /></button></header>
            <div className="v2-shortcut-editor-body">
              <div className="v2-shortcut-editor-heading"><strong>{mode === "network" ? "团队版" : "本机版"}</strong><span>已选择 {overviewShortcutDraft.length} / {overviewShortcutLimit}</span></div>
              <div className="v2-shortcut-editor-list">
                {orderedShortcutChoices.map((item) => {
                  const ItemIcon = item.icon;
                  const selectedIndex = overviewShortcutDraft.indexOf(item.id);
                  const selected = selectedIndex >= 0;
                  return <div className={`v2-shortcut-editor-row ${selected ? "selected" : ""}`} key={item.id}>
                    <label><input type="checkbox" checked={selected} disabled={!selected && overviewShortcutDraft.length >= overviewShortcutLimit} onChange={(event) => toggleOverviewShortcut(item.id, event.target.checked)} /><ItemIcon size={17} /><span><strong>{item.label}</strong><small>{item.description}</small></span></label>
                    {selected && <div><button className="v2-icon-button" type="button" onClick={() => moveOverviewShortcut(item.id, -1)} disabled={selectedIndex === 0} aria-label={`上移${item.label}`} title="上移"><ArrowUp size={15} /></button><button className="v2-icon-button" type="button" onClick={() => moveOverviewShortcut(item.id, 1)} disabled={selectedIndex === overviewShortcutDraft.length - 1} aria-label={`下移${item.label}`} title="下移"><ArrowDown size={15} /></button></div>}
                  </div>;
                })}
              </div>
            </div>
            <footer className="v2-shortcut-editor-actions"><button className="v2-button" type="button" onClick={resetOverviewShortcutDraft}><RotateCcw size={16} /> 恢复默认</button><div><button className="v2-button" type="button" onClick={() => setOverviewShortcutEditorOpen(false)}>取消</button><button className="v2-button primary" type="button" onClick={saveOverviewShortcuts} disabled={overviewShortcutDraft.length === 0}>保存</button></div></footer>
          </section>
        </div>}
      </section>
    );
  }

  function renderCatalog() {
    const products = catalog?.products ?? [];
    const parties = catalog?.parties ?? [];
    const mutationDisabled = mode === "offline" && !offlineActivated;
    const activeTabLabel = catalogTab === "products" ? "商品" : "往来方";
    const createButtonLabel = catalogTab === "products" ? "新增商品" : "新增往来方";

    return (
      <section className="v2-page" aria-labelledby="v2-catalog-title">
        <div className="v2-page-heading">
          <div>
            <span className="v2-eyebrow">基础资料</span>
            <h2 id="v2-catalog-title">商品与往来方</h2>
            <p>维护商品和业务往来方档案。</p>
          </div>
          <button className="v2-button" type="button" onClick={() => void refreshCatalog()} disabled={catalogLoading}>
            <RefreshCw size={16} className={catalogLoading ? "v2-spin" : ""} /> 刷新
          </button>
        </div>

        {catalogNotice && <div className={`v2-notice v2-catalog-notice ${catalogNotice.type}`}>{catalogNotice.text}</div>}

        <div className="v2-catalog-toolbar">
          <div className="v2-catalog-tabs" role="tablist" aria-label="基础资料类型">
            <button id="v2-catalog-tab-products" type="button" role="tab" aria-selected={catalogTab === "products"} className={catalogTab === "products" ? "active" : ""} onClick={() => setCatalogTab("products")}><Tags size={16} /> 商品 <span>{products.length}</span></button>
            <button id="v2-catalog-tab-parties" type="button" role="tab" aria-selected={catalogTab === "parties"} className={catalogTab === "parties" ? "active" : ""} onClick={() => setCatalogTab("parties")}><Users size={16} /> 往来方 <span>{parties.length}</span></button>
          </div>
          <button className="v2-button primary" type="button" onClick={() => openCatalogCreate()} disabled={catalogLoading || mutationDisabled}><Plus size={16} /> {createButtonLabel}</button>
        </div>

        <section className="v2-panel v2-catalog-directory" role="tabpanel" aria-labelledby={catalogTab === "products" ? "v2-catalog-tab-products" : "v2-catalog-tab-parties"} aria-busy={catalogLoading}>
          {catalogTab === "products" ? <>
            <div className="v2-section-heading compact">
              <div><h3>商品目录</h3><small>{products.length} 个商品</small></div>
            </div>
            <div className="v2-table-wrap">
              <table className="v2-catalog-table">
                <thead><tr><th>商品编码</th><th>商品名称</th><th>SN 前缀</th><th>禁用字符或片段</th><th aria-label="操作" /></tr></thead>
                <tbody>
                  {!catalogLoading && products.length === 0 && <tr><td className="v2-table-empty" colSpan={5}><div className="v2-catalog-empty"><Tags size={28} /><strong>暂无商品</strong><button className="v2-button primary" type="button" onClick={() => openCatalogCreate("products")} disabled={mutationDisabled}><Plus size={16} /> 新增商品</button></div></td></tr>}
                  {products.map((product) => {
                    const forbiddenTokens = parseForbiddenSerialTokens(product.serial_forbidden_chars);
                    return (
                      <tr key={product.sku_id}>
                        <td><strong className="v2-mono">{product.code}</strong></td>
                        <td>{product.name}</td>
                        <td>{product.serial_prefix ? <code className="v2-rule-token">{product.serial_prefix}</code> : <span className="v2-muted-value">不限</span>}</td>
                        <td>{forbiddenTokens.length > 0 ? <div className="v2-token-list">{forbiddenTokens.map((token, index) => <code className="v2-rule-token danger" key={`${token}-${index}`}>{token === " " ? "空格" : token}</code>)}</div> : <span className="v2-muted-value">无</span>}</td>
                        <td><button className="v2-icon-button" type="button" onClick={() => openCatalogProductEdit(product)} disabled={mutationDisabled || catalogLoading} aria-label={`编辑商品 ${product.code}`} title="编辑商品"><Pencil size={16} /></button></td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </> : <>
            <div className="v2-section-heading compact">
              <div><h3>往来方目录</h3><small>{parties.length} 个档案</small></div>
            </div>
            <div className="v2-table-wrap">
              <table className="v2-catalog-party-table">
                <thead><tr><th>名称</th><th>角色</th><th>联系方式</th><th>地址</th><th aria-label="操作" /></tr></thead>
                <tbody>
                  {!catalogLoading && parties.length === 0 && <tr><td className="v2-table-empty" colSpan={5}><div className="v2-catalog-empty"><Users size={28} /><strong>暂无往来方</strong><button className="v2-button primary" type="button" onClick={() => openCatalogCreate("parties")} disabled={mutationDisabled}><Plus size={16} /> 新增往来方</button></div></td></tr>}
                  {parties.map((party) => <tr key={party.party_id}>
                    <td><strong>{party.display_name}</strong>{party.contact_name && <small>{party.contact_name}</small>}</td>
                    <td><div className="v2-party-role-list">{party.roles.map((role) => <span key={role}>{catalogPartyRoleLabels[role]}</span>)}</div></td>
                    <td><div className="v2-party-contact">{party.phone && <span>{party.phone}</span>}{party.wechat && <span>微信：{party.wechat}</span>}{party.email && <span>{party.email}</span>}{!party.phone && !party.wechat && !party.email && <span className="v2-muted-value">未填写</span>}</div></td>
                    <td>{party.address || <span className="v2-muted-value">未填写</span>}</td>
                    <td><button className="v2-icon-button" type="button" onClick={() => openCatalogPartyEdit(party)} disabled={mutationDisabled || catalogLoading} aria-label={`编辑往来方 ${party.display_name}`} title="编辑往来方"><Pencil size={16} /></button></td>
                  </tr>)}
                </tbody>
              </table>
            </div>
          </>}
        </section>

        {catalogCreateOpen && <div className="v2-catalog-modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) closeCatalogCreate(); }} onKeyDown={(event) => { if (event.key === "Escape") closeCatalogCreate(); }}>
          <section className="v2-catalog-modal" role="dialog" aria-modal="true" aria-labelledby="v2-catalog-create-title">
            <header>
              <div><span>{activeTabLabel}</span><h3 id="v2-catalog-create-title">{catalogTab === "products" && editingProductId ? "编辑商品" : catalogTab === "parties" && editingPartyId ? "编辑往来方" : createButtonLabel}</h3></div>
              <button className="v2-icon-button" type="button" onClick={closeCatalogCreate} disabled={catalogLoading} aria-label="关闭商品或往来方窗口" title="关闭"><X size={17} /></button>
            </header>
            {catalogTab === "products" ? <form className="v2-form v2-catalog-modal-form" onSubmit={submitCatalogProduct}>
              <div className="v2-form-grid">
                <label><span>商品编码 *</span><input value={newProductCode} onChange={(event) => setNewProductCode(event.target.value)} placeholder="例如 DDR4-32G-3200" autoComplete="off" autoFocus required disabled={catalogLoading || mutationDisabled} /></label>
                <label><span>商品名称 *</span><input value={newProductName} onChange={(event) => setNewProductName(event.target.value)} placeholder="例如 32G 3200 内存" autoComplete="off" required disabled={catalogLoading || mutationDisabled} /></label>
                <label><span>SN 前缀</span><input value={newProductSerialPrefix} onChange={(event) => setNewProductSerialPrefix(event.target.value)} placeholder="可选，例如 RAM" autoComplete="off" disabled={catalogLoading || mutationDisabled} /></label>
                <label><span>SN 禁用字符或片段</span><input value={newProductForbiddenChars} onChange={(event) => setNewProductForbiddenChars(event.target.value)} placeholder="使用英文逗号分隔" autoComplete="off" disabled={catalogLoading || mutationDisabled} /><small>英文逗号分隔；默认禁止连字符和空格。</small></label>
              </div>
              {editingProductId && <div className="v2-rule-hint"><ShieldAlert size={17} /><span>商品会保持原 SKU 身份，已有入库、订单和生命周期关联不会改变；扫码规则只约束后续入库。</span></div>}
              {catalogNotice?.type === "error" && <div className="v2-notice error">{catalogNotice.text}</div>}
              <div className="v2-form-actions"><button className="v2-button" type="button" onClick={closeCatalogCreate} disabled={catalogLoading}>取消</button><button className="v2-button primary" type="submit" disabled={catalogLoading || mutationDisabled}>{editingProductId ? <CheckCircle2 size={16} /> : <Plus size={16} />} 保存商品</button></div>
            </form> : <form className="v2-form v2-catalog-modal-form" onSubmit={submitCatalogParty}>
              <div className="v2-form-grid">
                <label><span>名称 *</span><input value={newPartyName} onChange={(event) => setNewPartyName(event.target.value)} placeholder="例如 深圳某某科技" autoComplete="organization" autoFocus required disabled={catalogLoading || mutationDisabled} /></label>
                <label><span>联系人</span><input value={newPartyContactName} onChange={(event) => setNewPartyContactName(event.target.value)} placeholder="姓名" autoComplete="name" disabled={catalogLoading || mutationDisabled} /></label>
                <label><span>电话</span><input type="tel" value={newPartyPhone} onChange={(event) => setNewPartyPhone(event.target.value)} placeholder="手机号或座机" autoComplete="tel" disabled={catalogLoading || mutationDisabled} /></label>
                <label><span>微信号</span><input value={newPartyWechat} onChange={(event) => setNewPartyWechat(event.target.value)} placeholder="微信号" autoComplete="off" disabled={catalogLoading || mutationDisabled} /></label>
                <label className="v2-span-two"><span>电子邮箱</span><input type="email" value={newPartyEmail} onChange={(event) => setNewPartyEmail(event.target.value)} placeholder="name@example.com" autoComplete="email" disabled={catalogLoading || mutationDisabled} /></label>
                <label className="v2-span-two"><span>地址</span><input value={newPartyAddress} onChange={(event) => setNewPartyAddress(event.target.value)} placeholder="省市区及详细地址" autoComplete="street-address" disabled={catalogLoading || mutationDisabled} /></label>
                <label className="v2-span-two"><span>备注</span><textarea value={newPartyNotes} onChange={(event) => setNewPartyNotes(event.target.value)} rows={3} disabled={catalogLoading || mutationDisabled} /></label>
              </div>
              <fieldset className="v2-role-checklist v2-party-role-checklist">
                <legend>业务角色 *</legend>
                {(Object.entries(catalogPartyRoleLabels) as [CatalogPartyRole, string][]).map(([role, label]) => <label key={role}><input type="checkbox" checked={newPartyRoles.has(role)} onChange={() => toggleNewPartyRole(role)} disabled={catalogLoading || mutationDisabled} /><span><strong>{label}</strong></span></label>)}
              </fieldset>
              {catalogNotice?.type === "error" && <div className="v2-notice error">{catalogNotice.text}</div>}
              <div className="v2-form-actions"><button className="v2-button" type="button" onClick={closeCatalogCreate} disabled={catalogLoading}>取消</button><button className="v2-button primary" type="submit" disabled={catalogLoading || mutationDisabled}>{editingPartyId ? <CheckCircle2 size={16} /> : <Plus size={16} />} 保存往来方</button></div>
            </form>}
          </section>
        </div>}
      </section>
    );
  }

  function renderReceipt() {
    const products = catalog?.products ?? [];
    const suppliers = catalog?.suppliers ?? [];
    const productLocked = scannedBarcodes.length > 0;
    const mutationDisabled = mode === "offline" && !offlineActivated;
    const missingCatalogEntries = [
      products.length === 0 ? "商品" : null,
      suppliers.length === 0 ? "供应商" : null,
    ].filter((value): value is string => Boolean(value));
    const firstMissingCatalogTab: CatalogTab = products.length === 0
      ? "products"
      : "parties";
    const receiptReady = receiptDetailsReady && barcodes.length > 0;
    const stepOneState = receiptStep === 1 ? "active" : receiptDetailsReady ? "complete" : "pending";
    const stepTwoState = receiptStep === 2 ? "active" : barcodes.length > 0 ? "complete" : "pending";
    const stepThreeState = receiptStep === 3 ? "active" : "pending";
    const forbiddenTokens = selectedProduct ? parseForbiddenSerialTokens(selectedProduct.serial_forbidden_chars) : [];

    return (
      <section className="v2-page" aria-labelledby="v2-receipt-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">入库管理</span><h2 id="v2-receipt-title">新建入库批次</h2></div>
          <button className="v2-button" type="button" onClick={() => navigateToPage("catalog")} disabled={modeSwitchDisabled}><Tags size={16} /> 管理基础资料</button>
        </div>

        {catalogNotice?.type === "error" && <div className="v2-notice error">{catalogNotice.text}</div>}

        <form className="v2-panel v2-form v2-receipt-form" onSubmit={submitReceipt}>
          <ol className="v2-receipt-progress" aria-label="入库步骤">
            <li className={stepOneState}><span>1</span><div><strong>选择资料</strong><small>{receiptDetailsReady ? "已就绪" : "待选择"}</small></div></li>
            <li className={stepTwoState}><span>2</span><div><strong>扫描 SN</strong><small>{barcodes.length > 0 ? `${barcodes.length} 件` : "待扫描"}</small></div></li>
            <li className={stepThreeState}><span>3</span><div><strong>确认入库</strong><small>{receiptCompleted ? "已完成" : receiptReady ? "可提交" : "待完成"}</small></div></li>
          </ol>

          {receiptStep === 1 && <section className="v2-receipt-details-step" aria-labelledby="v2-receipt-details-title">
            <div className="v2-receipt-section-heading"><span>1</span><div><h3 id="v2-receipt-details-title">选择资料</h3><small>{receiptDetailsReady ? "资料已完整" : "完成必填项"}</small></div></div>
            {!catalogLoading && catalog && missingCatalogEntries.length > 0 && <div className="v2-notice warning v2-receipt-prerequisite" role="alert"><span>缺少基础资料：{missingCatalogEntries.join("、")}。请先新增后再扫码。</span><button className="v2-button" type="button" onClick={() => openCatalogCreateFromReceipt(firstMissingCatalogTab)}><Plus size={16} /> 新增{missingCatalogEntries[0]}</button></div>}
            <div className="v2-form-grid">
            <label className="v2-receipt-autocomplete"><span>商品 *</span><div className="v2-receipt-autocomplete-control">
              <input value={receiptProductInput} onChange={(event) => updateReceiptProductInput(event.target.value)} onFocus={() => setReceiptProductSuggestionsOpen(true)} onBlur={() => window.setTimeout(() => setReceiptProductSuggestionsOpen(false), 120)} onKeyDown={(event) => {
                if (event.key === "Escape") setReceiptProductSuggestionsOpen(false);
                if (event.key === "Enter" && receiptProductSuggestionsOpen && receiptProductSuggestions.length > 0) {
                  event.preventDefault();
                  chooseReceiptProduct(receiptProductSuggestions[0]);
                }
              }} placeholder={catalogLoading ? "正在读取商品…" : "输入编码或名称查找"} required disabled={catalogLoading || scanChecking || productLocked || products.length === 0} autoComplete="off" role="combobox" aria-autocomplete="list" aria-expanded={receiptProductSuggestionsOpen} aria-controls="v2-receipt-product-suggestions" />
              {receiptProductSuggestionsOpen && !productLocked && <div id="v2-receipt-product-suggestions" className="v2-receipt-autocomplete-suggestions" role="listbox">
                {receiptProductSuggestions.map((product) => <button key={product.sku_id} type="button" role="option" onMouseDown={(event) => event.preventDefault()} onClick={() => chooseReceiptProduct(product)}><strong>{product.code}</strong><small>{product.name}</small></button>)}
                {!catalogLoading && receiptProductSuggestions.length === 0 && <div className="v2-receipt-autocomplete-empty">没有匹配的商品</div>}
              </div>}
            </div>{productLocked && <small>当前批次已有 SN，商品已锁定。</small>}{!productLocked && selectedProduct && <small>已绑定目录商品：{selectedProduct.code} · {selectedProduct.name}</small>}</label>
            <label className="v2-receipt-autocomplete"><span>供应商 *</span><div className="v2-receipt-autocomplete-control">
              <input value={supplierName} onChange={(event) => updateReceiptSupplierInput(event.target.value)} onFocus={() => setReceiptSupplierSuggestionsOpen(true)} onBlur={() => window.setTimeout(() => setReceiptSupplierSuggestionsOpen(false), 120)} onKeyDown={(event) => {
                if (event.key === "Escape") setReceiptSupplierSuggestionsOpen(false);
                if (event.key === "Enter" && receiptSupplierSuggestionsOpen && receiptSupplierSuggestions.length > 0) {
                  event.preventDefault();
                  chooseReceiptSupplier(receiptSupplierSuggestions[0]);
                }
              }} placeholder={catalogLoading ? "正在读取供应商…" : "输入供应商名称查找"} required disabled={catalogLoading || suppliers.length === 0} autoComplete="off" role="combobox" aria-autocomplete="list" aria-expanded={receiptSupplierSuggestionsOpen} aria-controls="v2-receipt-supplier-suggestions" />
              {receiptSupplierSuggestionsOpen && <div id="v2-receipt-supplier-suggestions" className="v2-receipt-autocomplete-suggestions" role="listbox">
                {receiptSupplierSuggestions.map((party) => <button key={party.party_id} type="button" role="option" onMouseDown={(event) => event.preventDefault()} onClick={() => chooseReceiptSupplier(party)}><strong>{party.display_name}</strong><small>{party.contact_name || "历史供应商"}</small></button>)}
                {!catalogLoading && receiptSupplierSuggestions.length === 0 && <div className="v2-receipt-autocomplete-empty">没有匹配的供应商</div>}
              </div>}
            </div>{selectedSupplier && <small>已绑定供应商档案：{selectedSupplier.display_name}</small>}</label>
            <label><span>入库时间 *</span><input type="datetime-local" step="1" value={receivedAt} onChange={(event) => setReceivedAt(event.target.value)} required /></label>
            {mode === "network" && <label><span>入库仓库 *</span><select value={networkWarehouseId} onChange={(event) => {
              const value = event.target.value;
              setNetworkWarehouseId(value);
              try { window.localStorage.setItem("inventory-v2-network-warehouse", value); } catch { /* optional preference */ }
            }} required disabled={networkWarehousesLoading || networkWarehouses.length === 0}>
              {networkWarehouses.length === 0 && <option value="">{networkWarehousesLoading ? "正在读取仓库…" : "没有可用收货仓库"}</option>}
              {networkWarehouses.map((warehouse) => <option key={warehouse.warehouse_id} value={warehouse.warehouse_id}>{warehouse.warehouse_name}（{warehouse.warehouse_code}）</option>)}
            </select>{networkWarehousesError && <small className="v2-field-error">仓库读取失败：{networkWarehousesError}</small>}</label>}
            <label className="v2-span-two"><span>来源单号 / 备注</span><input value={sourceReference} onChange={(event) => setSourceReference(event.target.value)} placeholder="可选，例如供应商送货单号" autoComplete="off" /></label>
            <div className="v2-warranty-editor v2-span-two">
              <div className="v2-warranty-heading"><span>供应方质保（可选）</span><small>保存到本批次所有 SN 的来源记录</small></div>
              <div className="v2-warranty-controls">
                <select value={receiptWarrantyPreset} onChange={(event) => setReceiptWarrantyPreset(event.target.value)} aria-label="供应方质保期限">
                  <option value="">无质保</option><option value="7">一个星期（7天）</option><option value="15">半个月（15天）</option><option value="30">一个月（30天）</option><option value="365">一年（365天）</option><option value="custom">自定义天数</option>
                </select>
                {receiptWarrantyPreset === "custom" && <input type="number" min="1" max="36500" value={receiptWarrantyCustomDays} onChange={(event) => setReceiptWarrantyCustomDays(event.target.value)} placeholder="天数" aria-label="自定义质保天数" />}
                <label className="v2-inline-check"><input type="checkbox" checked={receiptWarrantyManualStart} onChange={(event) => setReceiptWarrantyManualStart(event.target.checked)} /><span>手动指定起算</span></label>
                {receiptWarrantyManualStart && <input type="datetime-local" step="1" value={receiptWarrantyStartsAt} onChange={(event) => setReceiptWarrantyStartsAt(event.target.value)} aria-label="供应方质保起算时间" />}
              </div>
            </div>
            </div>
            <div className="v2-workflow-actions"><button className="v2-button primary" type="button" onClick={() => navigateReceiptStep(2)} disabled={!receiptDetailsReady || catalogLoading || scanChecking}>下一步：扫描 SN <ArrowRight size={16} /></button></div>
          </section>}

          {receiptStep === 2 && <section className="v2-scanner-section" aria-labelledby="v2-scanner-title">
            <div className="v2-scanner-heading">
              <div className="v2-receipt-section-heading"><span>2</span><div><h3 id="v2-scanner-title">扫描 SN</h3><small>{receiptDetailsReady ? "逐件校验" : "等待资料完整"}</small></div></div>
              <strong>{barcodes.length}<small>件</small></strong>
            </div>

            <div className="v2-scan-rule-bar">
              <span><strong>商品</strong>{selectedProduct ? `${selectedProduct.code} · ${selectedProduct.name}` : "未选择"}</span>
              <span><strong>前缀</strong>{selectedProduct?.serial_prefix || "不限"}</span>
              <span><strong>禁用</strong>{forbiddenTokens.length > 0 ? forbiddenTokens.map((token) => token === " " ? "空格" : token).join("、") : "无"}</span>
            </div>

            <label className="v2-scan-field">
              <span>扫码枪输入 *</span>
              <div className="v2-scanner-control">
                <Bell size={21} aria-hidden="true" />
                <input ref={scannerInputRef} value={scannerInput} onChange={(event) => setScannerInput(event.target.value)} onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    void addScannedBarcode();
                  }
                }} placeholder={receiptDetailsReady ? "请扫描 SN（扫码枪自动回车）" : "请先完成入库资料"} autoFocus autoComplete="off" autoCapitalize="characters" spellCheck={false} disabled={!receiptDetailsReady || scanChecking || receiptLoading || mutationDisabled || catalogLoading} />
                <button className="v2-button" type="button" onClick={() => void addScannedBarcode()} disabled={!receiptDetailsReady || !scannerInput.trim() || scanChecking || receiptLoading || mutationDisabled}>{scanChecking ? "正在校验…" : "手动加入"}</button>
              </div>
              <small>扫描后会自动回到输入框；每个 SN 即时精确查重，提交时数据库再次校验唯一性。</small>
            </label>

            {receiptNotice && <div className={`v2-scanner-feedback ${receiptNotice.type}`} role={receiptNotice.type === "error" ? "alert" : "status"} aria-live={receiptNotice.type === "error" ? "assertive" : "polite"}>{receiptNotice.type === "error" ? <Bell size={19} /> : <CheckCircle2 size={19} />}<span>{receiptNotice.text}</span></div>}

            <div className="v2-scanned-heading">
              <div><strong>当前批次</strong><span>已采集 {barcodes.length} 个 SN</span></div>
              <button className="v2-button" type="button" onClick={() => void clearScannedBatch()} disabled={scanChecking || receiptLoading || barcodes.length === 0}>清空批次</button>
            </div>
            <div className="v2-scanned-list" aria-label="已采集 SN">
              {barcodes.length === 0 && <div className="v2-scanned-empty"><PackagePlus size={26} /><span>等待扫描</span></div>}
              {barcodes.map((barcode, index) => (
                <div className="v2-scanned-row" key={barcode}>
                  <span>{String(index + 1).padStart(2, "0")}</span>
                  <strong>{barcode}</strong>
                  <button className="v2-icon-button danger" type="button" onClick={() => removeScannedBarcode(barcode)} disabled={scanChecking || receiptLoading} aria-label={`移除 SN ${barcode}`} title="从当前批次移除"><X size={16} /></button>
                </div>
              ))}
            </div>
            <details className="v2-alternative-entry">
              <summary><span>备用录入</span><small>批量粘贴 SN，仅在扫码枪不可用时使用</small><ChevronDown size={16} /></summary>
              <div className="v2-alternative-content">
                <label><span>每行一个 SN</span><textarea value={receiptBulkInput} onChange={(event) => setReceiptBulkInput(event.target.value)} placeholder={"SN0001\nSN0002\nSN0003"} disabled={!receiptDetailsReady || scanChecking || receiptLoading || mutationDisabled} /></label>
                <button className="v2-button" type="button" onClick={() => void importReceiptBarcodes()} disabled={!receiptDetailsReady || !receiptBulkInput.trim() || scanChecking || receiptLoading || mutationDisabled}>校验并加入批次</button>
              </div>
            </details>
            <div className="v2-workflow-actions">
              <button className="v2-button" type="button" onClick={() => navigateReceiptStep(1)} disabled={scanChecking || receiptLoading}>上一步</button>
              <button className="v2-button primary" type="button" onClick={() => navigateReceiptStep(3)} disabled={!receiptReady || scanChecking || receiptLoading}>下一步：确认入库 <ArrowRight size={16} /></button>
            </div>
          </section>}

          {receiptStep === 3 && <section className="v2-receipt-confirm-step" aria-labelledby="v2-receipt-confirm-title">
            <div className="v2-receipt-section-heading"><span>3</span><div><h3 id="v2-receipt-confirm-title">确认入库</h3><small>{receiptCompleted ? "已完成" : receiptReady ? "核对后提交" : "完成前两步后提交"}</small></div></div>
            {!receiptCompleted ? <>
              <div className="v2-receipt-confirm-summary">
                <span><small>商品</small><strong>{selectedProduct?.code ?? "—"}</strong></span>
                <span><small>供应商</small><strong>{supplierName || "—"}</strong></span>
                <span><small>数量</small><strong>{barcodes.length} 件</strong></span>
              </div>
              <div className="v2-workflow-actions">
                <button className="v2-button" type="button" onClick={() => navigateReceiptStep(2)} disabled={receiptLoading || scanChecking}>上一步</button>
                <button className="v2-button primary v2-receipt-submit" type="submit" disabled={receiptLoading || scanChecking || mutationDisabled || !receiptReady}>{receiptLoading ? "正在原子入库…" : `确认入库 ${barcodes.length} 件`}</button>
              </div>
            </> : <>
              <div className="v2-inline-success"><CheckCircle2 size={18} /><span>{receiptCompleted.receipt_no} 已完成入库 {receiptCompleted.received_count} 件。新入库单件默认标记为未测试。</span></div>
              <div className="v2-workflow-actions"><button className="v2-button primary" type="button" onClick={startNextReceiptBatch}>开始下一批 <ArrowRight size={16} /></button></div>
            </>}
          </section>}
        </form>
      </section>
    );
  }

  function renderQuality() {
    const mutationDisabled = mode === "offline" && !offlineActivated;
    const stepOneState = qualityStep === 1 ? "active" : selectedBarcodes.size > 0 ? "complete" : "pending";
    const stepTwoState = qualityStep === 2 ? "active" : "pending";
    return (
      <section className="v2-page" aria-labelledby="v2-quality-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">质量作业 · 第 {qualityStep} / 2 步</span><h2 id="v2-quality-title">扫码质检</h2><p>先逐件采集待检 SN，再填写结果并提交。</p></div>
          <div className="v2-page-heading-actions">
            <button className="v2-button" type="button" onClick={openQualityLabelManager} disabled={qualityLoading || qualityScanChecking}><Tags size={16} /> 管理质检标签</button>
            <button className="v2-button" type="button" onClick={() => { void refreshQualityItems(); void refreshQualityLabels(); }} disabled={qualityLoading || qualityScanChecking || qualityLabelsLoading}><RefreshCw size={16} className={qualityLoading || qualityLabelsLoading ? "v2-spin" : ""} /> 刷新</button>
          </div>
        </div>
        <div className="v2-panel v2-quality-workbench">
          <ol className="v2-workflow-progress v2-workflow-progress-two" aria-label="质检步骤">
            <li className={stepOneState} aria-current={qualityStep === 1 ? "step" : undefined}><span>1</span><div><strong>扫描 SN</strong><small>{selectedBarcodes.size > 0 ? `${selectedBarcodes.size} 件` : "待扫描"}</small></div></li>
            <li className={stepTwoState} aria-current={qualityStep === 2 ? "step" : undefined}><span>2</span><div><strong>填写结果</strong><small>{selectedBarcodes.size > 0 ? "待提交" : "待选择"}</small></div></li>
          </ol>
          <form className="v2-quality-step-form" onSubmit={submitInspection}>
            {qualityStep === 1 && <div className="v2-quality-step-panel v2-quality-scanner-panel">
              <div className="v2-workbench-toolbar">
                <div>
                  <span>质检类型</span>
                  <div className="v2-segmented" aria-label="质检类型">
                    <button type="button" className={inspectionKind === "initial" ? "active" : ""} onClick={() => void changeInspectionKind("initial")} disabled={qualityLoading || qualityScanChecking}>初检</button>
                    <button type="button" className={inspectionKind === "retest" ? "active" : ""} onClick={() => void changeInspectionKind("retest")} disabled={qualityLoading || qualityScanChecking}>复检</button>
                  </div>
                </div>
                <strong>{selectedBarcodes.size}<small>件已扫描</small></strong>
              </div>

              <div className="v2-scan-context">
                <span><strong>当前范围</strong>{inspectionKind === "initial" ? "待检入库 · 未测试" : "隔离区 · 待复检"}</span>
                <span><strong>下一步</strong>填写本批质检结果</span>
              </div>

              <label className="v2-scan-field">
                <span>扫码枪输入 *</span>
                <div className="v2-scanner-control">
                  <Bell size={21} aria-hidden="true" />
                  <input ref={qualityScannerInputRef} value={qualityScannerInput} onChange={(event) => setQualityScannerInput(event.target.value)} onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      void addQualityScannedBarcode();
                    }
                  }} placeholder="请扫描待检 SN（扫码枪自动回车）" autoFocus autoComplete="off" autoCapitalize="characters" spellCheck={false} disabled={qualityScanChecking || qualityLoading || mutationDisabled} />
                  <button className="v2-button" type="button" onClick={() => void addQualityScannedBarcode()} disabled={!qualityScannerInput.trim() || qualityScanChecking || qualityLoading || mutationDisabled}>{qualityScanChecking ? "正在核对…" : "手动加入"}</button>
                </div>
                <small>系统会精确读取单件状态；扫到错误类型、重复 SN 或不存在的 SN 会立即报警。</small>
              </label>

              {qualityScanNotice && <div className={`v2-scanner-feedback ${qualityScanNotice.type}`} role={qualityScanNotice.type === "error" ? "alert" : "status"} aria-live={qualityScanNotice.type === "error" ? "assertive" : "polite"}>{qualityScanNotice.type === "error" ? <Bell size={19} /> : <CheckCircle2 size={19} />}<span>{qualityScanNotice.text}</span></div>}

              <div className="v2-scanned-heading">
                <div><strong>当前质检批次</strong><span>已采集 {selectedBarcodes.size} 个 SN</span></div>
                <button className="v2-button" type="button" onClick={() => void clearQualityBatch()} disabled={qualityLoading || qualityScanChecking || selectedBarcodes.size === 0}>清空批次</button>
              </div>
              <div className="v2-scanned-list" aria-label="当前质检批次 SN">
                {selectedBarcodes.size === 0 && <div className="v2-scanned-empty"><ClipboardCheck size={26} /><span>等待扫描待检 SN</span></div>}
                {Array.from(selectedBarcodes).map((barcode, index) => (
                  <div className="v2-scanned-row" key={barcode}>
                    <span>{String(index + 1).padStart(2, "0")}</span>
                    <strong>{barcode}</strong>
                    <button className="v2-icon-button danger" type="button" onClick={() => removeQualityBarcode(barcode)} disabled={qualityLoading || qualityScanChecking} aria-label={`移除质检 SN ${barcode}`} title="从当前批次移除"><X size={16} /></button>
                  </div>
                ))}
              </div>

              <details className="v2-alternative-entry">
                <summary><span>备用录入</span><small>批量粘贴或从待检列表勾选</small><ChevronDown size={16} /></summary>
                <div className="v2-alternative-content v2-quality-alternatives">
                  <div className="v2-bulk-entry">
                    <label><span>批量粘贴 SN（每行一个）</span><textarea value={qualityBulkInput} onChange={(event) => setQualityBulkInput(event.target.value)} placeholder={"SN0001\nSN0002\nSN0003"} disabled={qualityScanChecking || qualityLoading || mutationDisabled} /></label>
                    <button className="v2-button" type="button" onClick={() => void importQualityBarcodes()} disabled={!qualityBulkInput.trim() || qualityScanChecking || qualityLoading || mutationDisabled}>校验并加入批次</button>
                  </div>
                  <div>
                    <div className="v2-selection-heading"><strong>待检列表</strong><span>显示 {eligibleQualityItems.length} 件</span></div>
                    <div className="v2-select-list" aria-busy={qualityLoading}>
                      {!qualityLoading && eligibleQualityItems.length === 0 && <div className="v2-empty"><CheckCircle2 size={28} /> 当前没有符合条件的待检库存</div>}
                      {eligibleQualityItems.map((item) => (
                        <label className="v2-select-item" key={item.inventory_unit_id}>
                          <input type="checkbox" checked={selectedBarcodes.has(item.barcode)} onChange={() => toggleQualityBarcode(item.barcode)} disabled={qualityLoading || qualityScanChecking || mutationDisabled} />
                          <span className="v2-item-main"><strong>{item.barcode}</strong><small>{item.owner_name} · {item.sku_code} / {item.sku_name}</small></span>
                          <span className={`v2-badge quality-${item.quality_status}`}>{displayedQualityStatusLabels[item.quality_status]}</span>
                        </label>
                      ))}
                    </div>
                  </div>
                </div>
              </details>
              <div className="v2-workflow-actions"><button className="v2-button primary" type="button" onClick={() => navigateQualityStep(2)} disabled={selectedBarcodes.size === 0 || qualityLoading || qualityScanChecking}>下一步：填写质检结果 <ArrowRight size={16} /></button></div>
            </div>}

            {qualityStep === 2 && <div className="v2-quality-step-panel v2-inspection-form">
              <div className="v2-section-heading compact"><div><h3>本批质检结果</h3><small>应用到本批所有 SN</small></div><ClipboardCheck size={20} /></div>
              <div className="v2-scan-context">
                <span><strong>质检类型</strong>{inspectionKind === "initial" ? "初检" : "复检"}</span>
                <span><strong>已选单件</strong>{selectedBarcodes.size} 件</span>
              </div>
              <div className="v2-code-list"><strong>本批 SN</strong>{Array.from(selectedBarcodes).map((barcode) => <span key={barcode}><b>{barcode}</b><small>待提交</small></span>)}</div>
              {activeQualityLabels.length === 0 && <div className="v2-notice warning v2-quality-label-required"><span>当前没有可用的质检标签。</span><button className="v2-button" type="button" onClick={openQualityLabelManager}><Plus size={16} /> 创建标签</button></div>}
              <label><span>质检标签 *</span><select value={inspectionQualityLabelId} onChange={(event) => setInspectionQualityLabelId(event.target.value)} disabled={qualityLoading || qualityScanChecking || qualityLabelsLoading || mutationDisabled}><option value="">请选择质检标签</option><optgroup label="进入可用库存">{activeQualityLabels.filter((label) => label.disposition === "available").map((label) => <option key={label.quality_label_id} value={label.quality_label_id}>{label.name}</option>)}</optgroup><optgroup label="进入隔离区">{activeQualityLabels.filter((label) => label.disposition === "quarantine").map((label) => <option key={label.quality_label_id} value={label.quality_label_id}>{label.name}</option>)}</optgroup></select></label>
              <label><span>缺陷代码</span><input value={defectCode} onChange={(event) => setDefectCode(event.target.value)} placeholder={selectedQualityLabel?.disposition === "quarantine" ? "隔离类标签建议填写" : "可选"} disabled={qualityLoading || qualityScanChecking || mutationDisabled} /></label>
              <label><span>质检备注</span><textarea value={inspectionNotes} onChange={(event) => setInspectionNotes(event.target.value)} placeholder="记录现象、测试方法或复检说明" disabled={qualityLoading || qualityScanChecking || mutationDisabled} /></label>
              <div className={`v2-rule-hint ${selectedQualityLabel?.disposition === "quarantine" ? "danger" : ""}`}><ShieldAlert size={17} /><span>{selectedQualityLabel ? `“${selectedQualityLabel.name}”提交后将${selectedQualityLabel.disposition === "available" ? "进入可用库存并可参与出库" : "进入隔离区等待后续处理"}。` : "标签决定质检后进入可用库存或隔离区。"}</span></div>
              <div className="v2-workflow-actions">
                <button className="v2-button" type="button" onClick={() => navigateQualityStep(1)} disabled={qualityLoading || qualityScanChecking}>上一步</button>
                <button className="v2-button primary" type="submit" disabled={qualityLoading || qualityScanChecking || selectedBarcodes.size === 0 || !selectedQualityLabel || mutationDisabled}>{qualityLoading ? "正在提交…" : `确认质检 ${selectedBarcodes.size} 件`}</button>
              </div>
            </div>}
          </form>
        </div>
        {qualityNotice && <div className={`v2-notice ${qualityNotice.type}`}>{qualityNotice.text}</div>}
        {qualityLabelModalOpen && <div className="v2-catalog-modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) closeQualityLabelManager(); }} onKeyDown={(event) => { if (event.key === "Escape") closeQualityLabelManager(); }}>
          <section className="v2-catalog-modal v2-quality-label-modal" role="dialog" aria-modal="true" aria-labelledby="v2-quality-label-modal-title">
            <header><div><span>质检设置</span><h3 id="v2-quality-label-modal-title">质检标签管理</h3></div><button className="v2-icon-button" type="button" onClick={closeQualityLabelManager} disabled={qualityLabelsLoading} aria-label="关闭质检标签管理" title="关闭"><X size={18} /></button></header>
            <div className="v2-quality-label-modal-body">
              <section className="v2-quality-label-directory" aria-label="已有质检标签">
                <div className="v2-quality-label-directory-heading"><div><strong>已有标签</strong><small>{qualityLabels.filter((label) => label.active).length} 个启用</small></div><button className="v2-button" type="button" onClick={startNewQualityLabel} disabled={qualityLabelsLoading || mutationDisabled}><Plus size={16} /> 新建</button></div>
                <div className="v2-quality-label-list" aria-busy={qualityLabelsLoading}>
                  {!qualityLabelsLoading && qualityLabels.length === 0 && <div className="v2-catalog-empty"><Tags size={28} /><strong>还没有质检标签</strong></div>}
                  {qualityLabels.map((label) => <div className={`v2-quality-label-row ${editingQualityLabelId === label.quality_label_id ? "selected" : ""} ${label.active ? "" : "inactive"}`} key={label.quality_label_id}><div><strong>{label.name}</strong><span className={`v2-quality-label-disposition ${label.disposition}`}>{label.disposition === "available" ? "进入可用库存" : "进入隔离区"}</span><small>{label.active ? "已启用" : "已停用"} · 已用于 {label.usage_count} 件 · 改名 {label.name_history.length} 次</small></div><button className="v2-icon-button" type="button" onClick={() => editQualityLabel(label)} disabled={qualityLabelsLoading} aria-label={`编辑质检标签 ${label.name}`} title="编辑标签"><Pencil size={16} /></button></div>)}
                </div>
              </section>
              <form className="v2-form v2-quality-label-editor" onSubmit={submitQualityLabel}>
                <div className="v2-section-heading compact"><div><h4>{editingQualityLabelId ? "编辑标签" : "创建标签"}</h4><small>名称可自定义，处理方式决定库存去向</small></div></div>
                <label><span>标签名称 *</span><input value={qualityLabelName} onChange={(event) => setQualityLabelName(event.target.value.slice(0, 40))} maxLength={40} placeholder="例如：外观完好、屏幕异常" disabled={qualityLabelsLoading || mutationDisabled} autoFocus /></label>
                <fieldset className="v2-quality-disposition-options" disabled={qualityLabelsLoading || mutationDisabled || (editingQualityLabel?.usage_count ?? 0) > 0}><legend>库存处理方式 *</legend><label className={qualityLabelDisposition === "available" ? "selected" : ""}><input type="radio" name="quality-label-disposition" value="available" checked={qualityLabelDisposition === "available"} onChange={() => setQualityLabelDisposition("available")} /><span><strong>进入可用库存</strong><small>该单件可参与扫码出库</small></span></label><label className={qualityLabelDisposition === "quarantine" ? "selected danger" : ""}><input type="radio" name="quality-label-disposition" value="quarantine" checked={qualityLabelDisposition === "quarantine"} onChange={() => setQualityLabelDisposition("quarantine")} /><span><strong>进入隔离区</strong><small>该单件不可参与正常出库</small></span></label></fieldset>
                {editingQualityLabel && editingQualityLabel.usage_count > 0 && <div className="v2-quality-label-lock">已用于 {editingQualityLabel.usage_count} 件，库存处理方式不可修改。</div>}
                {editingQualityLabelRenamed && <label><span>改名说明</span><textarea value={qualityLabelRenameNote} onChange={(event) => setQualityLabelRenameNote(event.target.value.slice(0, 200))} maxLength={200} placeholder="可选" disabled={qualityLabelsLoading || mutationDisabled} /></label>}
                <label className="v2-quality-label-active"><input type="checkbox" checked={qualityLabelActive} onChange={(event) => setQualityLabelActive(event.target.checked)} disabled={qualityLabelsLoading || mutationDisabled} /><span><strong>启用此标签</strong><small>停用后不再出现在质检选择中，历史记录仍保留</small></span></label>
                {editingQualityLabel && <section className="v2-quality-label-history" aria-label="标签名称变更记录"><header><strong>名称变更记录</strong><small>{editingQualityLabel.name_history.length} 条</small></header>{editingQualityLabel.name_history.length === 0 ? <p>暂无改名记录</p> : <ol>{editingQualityLabel.name_history.map((history) => <li key={history.history_id}><div><strong>{history.old_name} 改为 {history.new_name}</strong><span>{formatDateTime(history.changed_at)} · {history.changed_by}</span></div>{history.change_note && <small>{history.change_note}</small>}</li>)}</ol>}</section>}
                {qualityLabelNotice && <div className={`v2-notice ${qualityLabelNotice.type}`}>{qualityLabelNotice.text}</div>}
                <div className="v2-workflow-actions"><button className="v2-button primary" type="submit" disabled={qualityLabelsLoading || mutationDisabled || !qualityLabelName.trim()}>{qualityLabelsLoading ? "正在保存…" : "保存标签"}</button></div>
              </form>
            </div>
          </section>
        </div>}
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
        <form className="v2-panel v2-filters" onSubmit={(event) => { event.preventDefault(); void submitInventorySearch(); }}>
          <label className="v2-search"><Search size={17} /><input value={inventorySearch} onChange={(event) => setInventorySearch(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter" && !event.nativeEvent.isComposing) { event.preventDefault(); if (!inventoryLoading) void submitInventorySearch(true); } }} placeholder="条码、货主、型号、入库单号" /></label>
          <select aria-label="库存状态" value={inventoryStatus} onChange={(event) => setInventoryStatus(event.target.value as InventoryStatus | "")}><option value="">全部库存状态</option>{Object.entries(inventoryStatusLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select>
          <select aria-label="质检状态" value={qualityStatus} onChange={(event) => setQualityStatus(event.target.value as QualityStatus | "")}><option value="">全部质检状态</option>{Object.entries(displayedQualityStatusLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select>
          <button className="v2-button primary" type="submit" disabled={inventoryLoading}>查询</button>
        </form>
        {inventoryError && <div className="v2-notice error">读取库存失败：{inventoryError}</div>}
        <div className="v2-panel v2-table-panel">
          <div className="v2-table-meta">共 {inventoryTotal} 件{inventoryTotal > inventoryItems.length ? `，当前显示前 ${inventoryItems.length} 件` : ""}</div>
          <div className="v2-table-wrap">
            <table>
              <thead><tr><th>条码 / SN</th><th>货主</th><th>产品型号</th><th>入库时间</th><th>库存状态</th><th>质检状态</th><th aria-label="操作" /></tr></thead>
              <tbody>
                {!inventoryLoading && inventoryItems.length === 0 && <tr><td className="v2-table-empty" colSpan={7}>没有匹配的库存记录</td></tr>}
                {inventoryItems.map((item) => (
                  <tr key={item.inventory_unit_id}>
                    <td><strong className="v2-mono">{item.barcode}</strong><small>{item.receipt_no}</small></td>
                    <td>{item.owner_name}</td>
                    <td><strong>{item.sku_code}</strong><small>{item.sku_name}</small></td>
                    <td>{formatDateTime(item.received_at)}</td>
                    <td><span className={`v2-badge inventory-${item.inventory_status}`}>{inventoryStatusLabels[item.inventory_status]}</span></td>
                    <td><span className={`v2-badge quality-${item.quality_status}`}>{displayedQualityStatusLabels[item.quality_status]}</span></td>
                    <td><button className="v2-icon-button" type="button" onClick={() => void openInventoryTrace(item.barcode)} aria-label={`查看 ${item.barcode} 完整追溯`} title="查看完整追溯"><Search size={16} /></button></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
        {inventoryTraceBarcode && <div className="v2-trace-backdrop" role="presentation" onMouseDown={(event) => {
          if (event.target === event.currentTarget) setInventoryTraceBarcode(null);
        }}>
          <section className="v2-trace-drawer" role="dialog" aria-modal="true" aria-labelledby="v2-trace-title">
            <header><div><span>单件完整追溯</span><h3 id="v2-trace-title">{inventoryTraceBarcode}</h3></div><button className="v2-icon-button" type="button" onClick={() => setInventoryTraceBarcode(null)} aria-label="关闭追溯详情" title="关闭"><X size={18} /></button></header>
            {inventoryTraceLoading && <div className="v2-empty"><RefreshCw className="v2-spin" size={24} /> 正在读取业务事实</div>}
            {inventoryTraceError && <div className="v2-notice error">读取追溯失败：{inventoryTraceError}</div>}
            {inventoryTrace && <div className="v2-trace-content">
              <dl className="v2-trace-summary">
                <div><dt>货主</dt><dd>{inventoryTrace.owner_name}</dd></div>
                <div><dt>产品型号</dt><dd>{inventoryTrace.sku_code} · {inventoryTrace.sku_name}</dd></div>
                <div><dt>入库单</dt><dd>{inventoryTrace.receipt_no}</dd></div>
                <div><dt>入库时间</dt><dd>{formatDateTime(inventoryTrace.received_at)}</dd></div>
                <div><dt>库存状态</dt><dd>{inventoryStatusLabels[inventoryTrace.inventory_status]}</dd></div>
                <div><dt>质检状态</dt><dd>{displayedQualityStatusLabels[inventoryTrace.quality_status]}</dd></div>
              </dl>
              <section className="v2-trace-section">
                <h4>完整生命周期</h4>
                <ol className="v2-lifecycle-events compact">
                  {buildLifecycleEvents(inventoryTrace, displayedQualityStatusLabels).map((event) => <li className={event.className} key={event.key}><span className="v2-event-dot" /><div><strong>{event.title}</strong><time>{formatDateTime(event.occurredAt)}</time>{event.details.map((detail) => <p key={detail}>{detail}</p>)}</div></li>)}
                </ol>
              </section>
            </div>}
          </section>
        </div>}
      </section>
    );
  }

  function renderLifecycle() {
    const trace = inventoryTrace;
    const latest = trace?.latest_related_order ?? null;
    const latestWarranty = latest?.warranty ? warrantyStatus(latest.warranty) : null;
    const lifecycleEvents = trace ? buildLifecycleEvents(trace, displayedQualityStatusLabels) : [];
    return (
      <section className="v2-page v2-lifecycle-page" aria-labelledby="v2-lifecycle-title">
        <div className="v2-page-heading"><div><span className="v2-eyebrow">库存与数据</span><h2 id="v2-lifecycle-title">单件生命周期</h2><p>按 SN 查看来源、质检、销售和售后全部事实。</p></div></div>
        <form className="v2-panel v2-lifecycle-search" onSubmit={(event) => void searchLifecycle(event)}>
          <label className="v2-search"><Search size={19} /><input value={lifecycleSearch} onChange={(event) => setLifecycleSearch(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter" && !event.nativeEvent.isComposing) { event.preventDefault(); if (!inventoryTraceLoading) void searchLifecycle(undefined, true); } }} placeholder="扫描或输入唯一 SN" autoComplete="off" autoCapitalize="characters" spellCheck={false} /></label>
          <button className="v2-button primary" type="submit" disabled={inventoryTraceLoading}><Search size={16} /> 查询生命周期</button>
        </form>
        {inventoryTraceError && <div className="v2-notice error">读取生命周期失败：{inventoryTraceError}</div>}
        {inventoryTraceLoading && <div className="v2-panel v2-empty"><RefreshCw className="v2-spin" size={24} /> 正在读取完整业务事实</div>}
        {trace && <div className="v2-lifecycle-layout">
          <section className="v2-panel v2-lifecycle-summary">
            <div className="v2-lifecycle-identity"><span className="v2-eyebrow">唯一库存单件</span><strong className="v2-mono">{trace.barcode}</strong><span>{trace.sku_code} · {trace.sku_name}</span></div>
            <div className="v2-lifecycle-badges"><span className={`v2-badge inventory-${trace.inventory_status}`}>{inventoryStatusLabels[trace.inventory_status]}</span><span className={`v2-badge quality-${trace.quality_status}`}>{displayedQualityStatusLabels[trace.quality_status]}</span></div>
            <dl className="v2-trace-summary"><div><dt>货主</dt><dd>{trace.owner_name}</dd></div><div><dt>供应商</dt><dd>{trace.supplier_name ?? "未记录"}</dd></div><div><dt>来源单号</dt><dd>{trace.source_reference ?? "未记录"}</dd></div><div><dt>入库单</dt><dd>{trace.receipt_no}</dd></div><div><dt>入库时间</dt><dd>{formatDateTime(trace.received_at)}</dd></div><div><dt>供应方质保</dt><dd>{warrantyDescription(trace.inbound_warranty)}</dd></div></dl>
          </section>
          <section className="v2-panel v2-latest-order">
            <div className="v2-section-heading compact"><div><span className="v2-eyebrow">最近一次关联订单</span><h3>{latest ? latest.order_no : "尚未关联订单"}</h3></div>{latest && <span className="v2-badge">{latest.order_status}</span>}</div>
            {latest ? <div className="v2-latest-order-grid"><div><small>客户</small><strong>{latest.upstream_receiver_name}</strong></div><div><small>分配时间</small><strong>{formatDateTime(latest.allocated_at)}</strong></div><div><small>出库单</small><strong>{latest.shipment_no ?? "未出库"}</strong></div><div><small>出库时间</small><strong>{latest.shipped_at ? formatDateTime(latest.shipped_at) : "未出库"}</strong></div><div><small>客户质保</small><strong>{latest.warranty ? latest.warranty.label_snapshot : "无质保"}</strong></div><div><small>售后</small><strong>{latest.return_no ? `已退货 · ${latest.return_no}` : "暂无退货"}</strong></div></div> : <p className="v2-muted-value">该 SN 尚未进入任何出库订单。</p>}
            {latestWarranty && <span className={`v2-warranty-status ${latestWarranty.className}`}><Clock3 size={15} /> {latestWarranty.label} · {warrantyDescription(latest?.warranty ?? null)}</span>}
          </section>
          <section className="v2-panel v2-lifecycle-timeline"><div className="v2-section-heading"><div><span className="v2-eyebrow">完整事实链</span><h3>生命周期时间线</h3></div><span>{lifecycleEvents.length} 个节点</span></div>
            <ol className="v2-lifecycle-events">
              {lifecycleEvents.map((event) => <li className={event.className} key={event.key}><span className="v2-event-dot" /><div><strong>{event.title}</strong><time>{formatDateTime(event.occurredAt)}</time>{event.details.map((detail) => <p key={detail}>{detail}</p>)}</div></li>)}
            </ol>
          </section>
        </div>}
        {!trace && !inventoryTraceLoading && <div className="v2-panel v2-empty"><History size={32} /><strong>扫描一个 SN，查看它从哪里来、卖给谁以及是否退过货</strong></div>}
      </section>
    );
  }

  function renderRecords() {
    const mutationDisabled = mode === "offline" && !offlineActivated;
    return (
      <section className="v2-page" aria-labelledby="v2-records-title">
        <div className="v2-page-heading"><div><span className="v2-eyebrow">库存与数据</span><h2 id="v2-records-title">单据查询</h2><p>按订单号、客户、供应商或 SN 查询历史单据。</p></div><button className="v2-button" type="button" onClick={() => void refreshRecords()} disabled={recordLoading}><RefreshCw size={16} className={recordLoading ? "v2-spin" : ""} /> 刷新</button></div>
        <form className="v2-panel v2-filters" onSubmit={(event) => { event.preventDefault(); void submitRecordsSearch(); }}><label className="v2-search"><Search size={17} /><input value={recordSearch} onChange={(event) => setRecordSearch(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter" && !event.nativeEvent.isComposing) { event.preventDefault(); if (!recordLoading) void submitRecordsSearch(true); } }} placeholder="订单号、出库单号、客户、供应商或 SN" /></label><button className="v2-button primary" type="submit" disabled={recordLoading}>查询</button></form>
        {recordNotice && <div className={`v2-notice ${recordNotice.type}`}>{recordNotice.text}</div>}
        <div className="v2-record-tabs" role="tablist"><button type="button" className={recordTab === "outbound" ? "active" : ""} onClick={() => setRecordTab("outbound")}><Truck size={16} /> 出库订单 <span>{outboundRecords.length}</span></button><button type="button" className={recordTab === "receipt" ? "active" : ""} onClick={() => setRecordTab("receipt")}><PackagePlus size={16} /> 收货单 <span>{receiptRecords.length}</span></button></div>
        {recordTab === "outbound" ? (
          <div className="v2-panel v2-table-panel"><div className="v2-table-wrap"><table><thead><tr><th>订单编号</th><th>客户</th><th>最近出库</th><th>数量</th><th>售后</th><th>状态</th><th /></tr></thead><tbody>
            {outboundRecords.length === 0 && !recordLoading && <tr><td colSpan={7} className="v2-table-empty">暂无匹配出库订单</td></tr>}
            {outboundRecords.map((record) => <tr key={record.order_id}><td><strong className="v2-mono">{record.order_no}</strong><small>{formatDateTime(record.created_at)}</small></td><td>{record.receiver_name}</td><td><strong>{record.latest_shipment_no ?? "未出库"}</strong><small>{record.latest_shipped_at ? formatDateTime(record.latest_shipped_at) : ""}</small></td><td>{record.item_count} 件</td><td>{record.returned_count > 0 ? <span className="v2-badge inventory-quarantined">退货 {record.returned_count}</span> : "—"}</td><td><span className={`v2-badge ${record.status === "voided" ? "inventory-voided" : ""}`}>{documentStatusLabel(record.status)}</span></td><td><button className="v2-icon-button" type="button" onClick={() => void openOutboundDocument(record.order_id)} title="查看订单详情" aria-label="查看订单详情"><Search size={16} /></button></td></tr>)}
          </tbody></table></div></div>
        ) : (
          <div className="v2-panel v2-table-panel"><div className="v2-table-wrap"><table><thead><tr><th>收货单号</th><th>供应商</th><th>货主</th><th>入库时间</th><th>数量</th><th>质保</th><th>状态</th><th /></tr></thead><tbody>
            {receiptRecords.length === 0 && !recordLoading && <tr><td colSpan={8} className="v2-table-empty">暂无匹配收货单</td></tr>}
            {receiptRecords.map((record) => <tr key={record.receipt_id}><td><strong className="v2-mono">{record.receipt_no}</strong><small>{record.source_reference ?? "无来源单号"}</small></td><td>{record.supplier_name ?? "未记录"}</td><td>{record.owner_name}</td><td>{formatDateTime(record.received_at)}</td><td>{record.item_count} 件</td><td>{record.warranty ? record.warranty.label_snapshot : "无质保"}</td><td><span className={`v2-badge ${record.status === "voided" ? "inventory-voided" : ""}`}>{documentStatusLabel(record.status)}</span></td><td><button className="v2-icon-button" type="button" onClick={() => void openReceiptDocument(record.receipt_id)} title="查看收货单详情" aria-label="查看收货单详情"><Search size={16} /></button></td></tr>)}
          </tbody></table></div></div>
        )}
        {selectedOutboundDocument && <section className="v2-panel v2-record-detail">
          <header><div><span className="v2-eyebrow">出库订单详情</span><h3>{selectedOutboundDocument.order_no} · {selectedOutboundDocument.receiver_name}</h3></div><div className="v2-detail-actions">
            {selectedOutboundDocument.status !== "voided" && <button className="v2-button" type="button" onClick={() => openRenameOutboundDialog(selectedOutboundDocument)} disabled={recordLoading || mutationDisabled}><Pencil size={16} /> 修改客户名称</button>}
            <button className="v2-button danger" type="button" disabled={!selectedOutboundDocument.void_eligibility.can_void || recordLoading} title={selectedOutboundDocument.void_eligibility.blockers.join("；") || "作废出库订单"} onClick={() => openVoidDialog("outbound", selectedOutboundDocument.order_id, selectedOutboundDocument.order_no, selectedOutboundDocument.items.length, selectedOutboundDocument.void_eligibility)}><Ban size={16} /> 作废单据</button>
            <button className="v2-button" type="button" onClick={() => openSnCopyDialog("outbound", selectedOutboundDocument.order_id, selectedOutboundDocument.order_no, selectedOutboundDocument.items.length)} disabled={recordLoading}><Copy size={16} /> 复制整单 SN</button>
            <button className="v2-button" type="button" onClick={() => void exportBusinessDocument("outbound", selectedOutboundDocument.order_id, selectedOutboundDocument.order_no)}><Download size={16} /> 导出出库单</button>
            <button className="v2-icon-button" type="button" onClick={() => setSelectedOutboundDocument(null)} aria-label="关闭详情" title="关闭"><X size={18} /></button>
          </div></header>
          <div className="v2-record-detail-meta"><span>订单状态 <strong>{documentStatusLabel(selectedOutboundDocument.status)}</strong></span><span>最近出库 <strong>{selectedOutboundDocument.latest_shipment_no ?? "未出库"}</strong></span><span>客户质保 <strong>{selectedOutboundDocument.items.find((item) => item.warranty)?.warranty?.label_snapshot ?? "无质保"}</strong></span></div>
          {selectedOutboundDocument.void_info && <div className="v2-void-fact"><Ban size={18} /><div><strong>该出库订单已作废</strong><span>{formatDateTime(selectedOutboundDocument.void_info.voided_at)} · {selectedOutboundDocument.void_info.actor_id}</span><p>{selectedOutboundDocument.void_info.reason}</p></div></div>}
          {!selectedOutboundDocument.void_eligibility.can_void && !selectedOutboundDocument.void_info && <div className="v2-void-blockers"><ShieldAlert size={18} /><div><strong>当前不能作废</strong>{selectedOutboundDocument.void_eligibility.blockers.map((blocker) => <span key={blocker}>{blocker}</span>)}</div></div>}
          <div className="v2-table-wrap"><table><thead><tr><th>SKU</th><th>商品名称</th><th>SN</th><th>库存状态</th><th>出库单</th><th>质保</th><th>售后</th></tr></thead><tbody>{selectedOutboundDocument.items.map((item) => <tr key={`${item.barcode}-${item.shipment_line_id ?? "allocation"}`}><td>{item.sku_code}</td><td>{item.sku_name}</td><td className="v2-mono">{item.barcode}</td><td><span className={`v2-badge inventory-${item.inventory_status}`}>{inventoryStatusLabels[item.inventory_status]}</span></td><td>{item.shipment_no ?? "未出库"}</td><td>{item.warranty ? item.warranty.label_snapshot : "无质保"}</td><td>{item.return_no ? `${item.return_no} · ${formatDateTime(item.returned_at ?? "")}` : "—"}</td></tr>)}</tbody></table></div>
        </section>}
        {selectedReceiptDocument && <section className="v2-panel v2-record-detail">
          <header><div><span className="v2-eyebrow">收货单详情</span><h3>{selectedReceiptDocument.receipt_no} · {selectedReceiptDocument.supplier_name ?? "未记录供应商"}</h3></div><div className="v2-detail-actions">
            <button className="v2-button danger" type="button" disabled={!selectedReceiptDocument.void_eligibility.can_void || recordLoading} title={selectedReceiptDocument.void_eligibility.blockers.join("；") || "作废收货单"} onClick={() => openVoidDialog("receipt", selectedReceiptDocument.receipt_id, selectedReceiptDocument.receipt_no, selectedReceiptDocument.items.length, selectedReceiptDocument.void_eligibility)}><Ban size={16} /> 作废单据</button>
            <button className="v2-button" type="button" onClick={() => openSnCopyDialog("receipt", selectedReceiptDocument.receipt_id, selectedReceiptDocument.receipt_no, selectedReceiptDocument.items.length)} disabled={recordLoading}><Copy size={16} /> 复制整单 SN</button>
            <button className="v2-button" type="button" onClick={() => void exportBusinessDocument("receipt", selectedReceiptDocument.receipt_id, selectedReceiptDocument.receipt_no)}><Download size={16} /> 导出收货单</button>
            <button className="v2-icon-button" type="button" onClick={() => setSelectedReceiptDocument(null)} aria-label="关闭详情" title="关闭"><X size={18} /></button>
          </div></header>
          <div className="v2-record-detail-meta"><span>单据状态 <strong>{documentStatusLabel(selectedReceiptDocument.status)}</strong></span><span>货主 <strong>{selectedReceiptDocument.owner_name}</strong></span><span>入库时间 <strong>{formatDateTime(selectedReceiptDocument.received_at)}</strong></span><span>质保 <strong>{selectedReceiptDocument.warranty ? selectedReceiptDocument.warranty.label_snapshot : "无质保"}</strong></span></div>
          {selectedReceiptDocument.void_info && <div className="v2-void-fact"><Ban size={18} /><div><strong>该收货单已作废</strong><span>{formatDateTime(selectedReceiptDocument.void_info.voided_at)} · {selectedReceiptDocument.void_info.actor_id}</span><p>{selectedReceiptDocument.void_info.reason}</p></div></div>}
          {!selectedReceiptDocument.void_eligibility.can_void && !selectedReceiptDocument.void_info && <div className="v2-void-blockers"><ShieldAlert size={18} /><div><strong>当前不能作废</strong>{selectedReceiptDocument.void_eligibility.blockers.map((blocker) => <span key={blocker}>{blocker}</span>)}</div></div>}
          <div className="v2-table-wrap"><table><thead><tr><th>SKU</th><th>商品名称</th><th>SN</th><th>库存状态</th><th>货主</th></tr></thead><tbody>{selectedReceiptDocument.items.map((item) => <tr key={item.barcode}><td>{item.sku_code}</td><td>{item.sku_name}</td><td className="v2-mono">{item.barcode}</td><td><span className={`v2-badge inventory-${item.inventory_status}`}>{inventoryStatusLabels[item.inventory_status]}</span></td><td>{item.owner_name}</td></tr>)}</tbody></table></div>
        </section>}
        {renameOutboundDialog && <div className="v2-catalog-modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) closeRenameOutboundDialog(); }} onKeyDown={(event) => { if (event.key === "Escape") closeRenameOutboundDialog(); }}>
          <section className="v2-catalog-modal v2-rename-outbound-modal" role="dialog" aria-modal="true" aria-labelledby="v2-rename-outbound-modal-title">
            <header><div><span>单据更正</span><h3 id="v2-rename-outbound-modal-title">修改 {renameOutboundDialog.orderNo} 的客户名称</h3></div><button className="v2-icon-button" type="button" onClick={closeRenameOutboundDialog} disabled={renameOutboundLoading} aria-label="关闭" title="关闭"><X size={18} /></button></header>
            <form className="v2-void-modal-form" onSubmit={(event) => void submitRenameOutbound(event)}>
              <div className="v2-void-impact"><Pencil size={21} /><div><strong>仅更正本张出库单</strong><span>不会重命名历史客户，也不会改变商品、SN、出库、交货或售后记录。</span></div></div>
              <label><span>客户名称 *</span><input value={renameOutboundName} onChange={(event) => setRenameOutboundName(event.target.value)} maxLength={200} autoFocus disabled={renameOutboundLoading} /></label>
              <div className="v2-form-actions"><button className="v2-button" type="button" onClick={closeRenameOutboundDialog} disabled={renameOutboundLoading}>取消</button><button className="v2-button primary" type="submit" disabled={renameOutboundLoading || !renameOutboundName.trim() || renameOutboundName.trim() === renameOutboundDialog.currentName}><Pencil size={16} />{renameOutboundLoading ? "正在修改…" : "确认修改"}</button></div>
            </form>
          </section>
        </div>}
        {voidDialog && <div className="v2-catalog-modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) closeVoidDialog(); }} onKeyDown={(event) => { if (event.key === "Escape") closeVoidDialog(); }}>
          <section className="v2-catalog-modal v2-void-modal" role="dialog" aria-modal="true" aria-labelledby="v2-void-modal-title">
            <header><div><span>危险操作</span><h3 id="v2-void-modal-title">作废 {voidDialog.documentNo}</h3></div><button className="v2-icon-button" type="button" onClick={closeVoidDialog} disabled={voidLoading} aria-label="关闭" title="关闭"><X size={18} /></button></header>
            <form className="v2-void-modal-form" onSubmit={(event) => void submitVoidDocument(event)}>
              <div className="v2-void-impact"><Ban size={21} /><div><strong>此操作不可撤销</strong><span>{voidDialog.kind === "receipt" ? `关联的 ${voidDialog.itemCount} 件库存将标记为已作废。` : "未发货商品会解除预留；已经退回的商品继续保持隔离。"}</span></div></div>
              <label><span>作废原因 *</span><textarea value={voidReason} onChange={(event) => setVoidReason(event.target.value)} placeholder="填写可供日后审计的具体原因" disabled={voidLoading} autoFocus /></label>
              <label><span>{mode === "network" ? "当前账号密码" : "危险操作密码"} *</span><input type="password" value={voidPassword} onChange={(event) => setVoidPassword(event.target.value)} autoComplete="current-password" disabled={voidLoading} /></label>
              <div className="v2-form-actions"><button className="v2-button" type="button" onClick={closeVoidDialog} disabled={voidLoading}>取消</button><button className="v2-button danger" type="submit" disabled={voidLoading || !voidReason.trim() || !voidPassword}><Ban size={16} />{voidLoading ? "正在作废…" : "确认作废"}</button></div>
            </form>
          </section>
        </div>}
        {snCopyDialog && <div className="v2-catalog-modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) closeSnCopyDialog(); }} onKeyDown={(event) => { if (event.key === "Escape") closeSnCopyDialog(); }}>
          <section className="v2-catalog-modal v2-sn-copy-modal" role="dialog" aria-modal="true" aria-labelledby="v2-sn-copy-modal-title">
            <header><div><span>敏感信息操作</span><h3 id="v2-sn-copy-modal-title">复制整单 SN</h3></div><button className="v2-icon-button" type="button" onClick={closeSnCopyDialog} disabled={snCopyLoading} aria-label="关闭" title="关闭"><X size={18} /></button></header>
            <form className="v2-void-modal-form" onSubmit={(event) => void submitSnCopy(event)}>
              <div className="v2-void-impact"><Copy size={21} /><div><strong>需要密码授权</strong><span>将复制 {snCopyDialog.documentNo} 的 {snCopyDialog.itemCount} 件单品 SN，每行一个。复制内容会进入系统剪贴板，请注意粘贴位置。</span></div></div>
              <label><span>{mode === "network" ? "当前账号密码" : "危险操作密码"} *</span><input type="password" value={snCopyPassword} onChange={(event) => setSnCopyPassword(event.target.value)} autoComplete="current-password" autoFocus disabled={snCopyLoading} /></label>
              <div className="v2-form-actions"><button className="v2-button" type="button" onClick={closeSnCopyDialog} disabled={snCopyLoading}>取消</button><button className="v2-button primary" type="submit" disabled={snCopyLoading || !snCopyPassword}><Copy size={16} />{snCopyLoading ? "正在授权…" : "授权并复制"}</button></div>
            </form>
          </section>
        </div>}
      </section>
    );
  }

  function renderReturns() {
    const firstCandidate = returnCandidates[0] ?? null;
    const status = firstCandidate?.warranty ? warrantyStatus(firstCandidate.warranty) : null;
    return (
      <section className="v2-page" aria-labelledby="v2-returns-title">
        <div className="v2-page-heading"><div><span className="v2-eyebrow">售后处理</span><h2 id="v2-returns-title">扫码退货</h2><p>先连续扫描同一出库单的退货 SN，结束扫描后为整批填写一次原因。</p></div></div>
        {returnStep === "scan" && <>
          <form className="v2-panel v2-return-scanner" onSubmit={(event) => void lookupReturnBarcode(event)}><label className="v2-search"><RotateCcw size={19} /><input ref={returnScannerRef} value={returnBarcode} onChange={(event) => setReturnBarcode(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); if (!returnLoading && returnBarcode.trim()) void lookupReturnBarcode(undefined, true); } }} placeholder="请扫描退货 SN（扫码枪自动回车）" autoComplete="off" autoCapitalize="characters" spellCheck={false} /></label><button className="v2-button primary" type="submit" disabled={returnLoading || !returnBarcode.trim()}>{returnLoading ? "正在定位…" : "加入本批"}</button></form>
          {returnCandidates.length > 0 && <section className="v2-panel v2-return-batch"><header className="v2-return-batch-heading"><div><span className="v2-eyebrow">当前退货批次</span><h3>{firstCandidate?.shipment_no}</h3></div><strong>{returnCandidates.length}<small> 件</small></strong></header><div className="v2-return-batch-meta"><span><small>客户</small><strong>{firstCandidate?.receiver_name}</strong></span><span><small>订单编号</small><strong>{firstCandidate?.order_no}</strong></span><span><small>本批规则</small><strong>同一出库单</strong></span></div><div className="v2-return-batch-items">{returnCandidates.map((candidate, index) => <div className="v2-return-batch-item" key={candidate.shipment_line_id}><span>{index + 1}</span><strong className="v2-mono">{candidate.barcode}</strong><small>{formatDateTime(candidate.shipped_at)}</small><button className="v2-icon-button" type="button" onClick={() => setReturnCandidates((items) => items.filter((item) => item.shipment_line_id !== candidate.shipment_line_id))} disabled={returnLoading} aria-label={`移除 ${candidate.barcode}`} title="从本批移除"><X size={16} /></button></div>)}</div><div className="v2-workflow-actions"><button className="v2-button primary" type="button" onClick={() => { setReturnNotice(null); setReturnStep("confirm"); }} disabled={returnLoading || returnCandidates.length === 0}>结束扫描，填写统一原因 <ArrowRight size={16} /></button><button className="v2-button" type="button" onClick={() => { setReturnCandidates([]); setReturnBarcode(""); setReturnNotice(null); }} disabled={returnLoading}>清空本批</button></div></section>}
        </>}
        {returnNotice && <div className={`v2-notice ${returnNotice.type}`}>{returnNotice.text}</div>}
        {returnStep === "confirm" && firstCandidate && <section className="v2-panel v2-return-confirm"><header><div><span className="v2-eyebrow">批量退货确认</span><h3>{firstCandidate.shipment_no} · {returnCandidates.length} 件</h3></div>{status && <span className={`v2-warranty-status ${status.className}`}>{status.label}</span>}</header><div className="v2-return-context"><span><small>客户</small><strong>{firstCandidate.receiver_name}</strong></span><span><small>订单编号</small><strong>{firstCandidate.order_no}</strong></span><span><small>出库单号</small><strong>{firstCandidate.shipment_no}</strong></span><span><small>出库时间</small><strong>{formatDateTime(firstCandidate.shipped_at)}</strong></span><span><small>客户质保</small><strong>{warrantyDescription(firstCandidate.warranty)}</strong></span></div><div className="v2-return-batch-items compact">{returnCandidates.map((candidate, index) => <div className="v2-return-batch-item" key={candidate.shipment_line_id}><span>{index + 1}</span><strong className="v2-mono">{candidate.barcode}</strong></div>)}</div><label><span>本批统一退货原因 *</span><textarea value={returnReason} onChange={(event) => setReturnReason(event.target.value)} placeholder="例如：客户检测后无法点亮" disabled={returnLoading} autoFocus /></label><div className="v2-workflow-actions"><button className="v2-button primary" type="button" onClick={() => void commitScannedReturn()} disabled={returnLoading || !returnReason.trim()}>{returnLoading ? "正在登记…" : `确认退回并隔离 ${returnCandidates.length} 件`}</button><button className="v2-button" type="button" onClick={() => { setReturnStep("scan"); setReturnReason(""); setReturnNotice(null); }} disabled={returnLoading}>返回继续扫描</button></div></section>}
      </section>
    );
  }

  function renderOutbound() {
    const mutationDisabled = mode === "offline" && !offlineActivated;
    const orderStepState = outboundStep === 1 ? "active" : canOpenOutboundStep(2) ? "complete" : "pending";
    const scanStepState = outboundStep === 2 ? "active" : outboundShipment ? "complete" : "pending";
    const deliveryStepState = outboundStep === 3 ? "active" : outboundResolved ? "complete" : "pending";
    const finishOutboundLabel = outboundOrder || outboundAllocation || outboundNotice?.type === "error"
      ? "重试归类并出库"
      : `确认 ${outboundScannedItems.length} 件并出库`;
    return (
      <section className="v2-page" aria-labelledby="v2-outbound-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">出库作业 · 第 {outboundStep} / 3 步</span><h2 id="v2-outbound-title">扫码出库</h2><p>先扫描实际出货单件，结束扫码时确定数量，再按品牌和型号自动归类并完成出库。</p></div>
        </div>
        <div className="v2-panel v2-outbound-workbench">
          <ol className="v2-workflow-progress v2-workflow-progress-three" aria-label="出库进度">
            <li className={orderStepState} aria-current={outboundStep === 1 ? "step" : undefined}><span>1</span><div><strong>选择客户</strong><small>{outboundOrder?.order_no ?? (outboundReceiver || "待填写")}</small></div></li>
            <li className={scanStepState} aria-current={outboundStep === 2 ? "step" : undefined}><span>2</span><div><strong>优先扫码</strong><small>{outboundHasScannedItems ? `${outboundScannedItems.length} 件已扫描` : "待扫码"}</small></div></li>
            <li className={deliveryStepState} aria-current={outboundStep === 3 ? "step" : undefined}><span>3</span><div><strong>交货处理</strong><small>{outboundShipment?.shipment_no ?? "待出库"}</small></div></li>
          </ol>

          {outboundStep === 1 && <form className="v2-outbound-step" onSubmit={beginOutboundScan}>
            <div className="v2-receipt-section-heading"><span>1</span><div><h3>选择上游客户</h3><small>输入历史客户名称可直接选择，新客户会在确认出库时自动创建</small></div></div>
            <div>
              <div className="v2-form-grid">
                <label className="v2-span-two v2-outbound-receiver-field"><span>上游收货方 *</span><div className="v2-outbound-receiver-input-wrap">
                  <input value={outboundReceiver} onChange={(event) => { setOutboundReceiver(event.target.value); setOutboundReceiverSuggestionsOpen(true); }} onFocus={() => setOutboundReceiverSuggestionsOpen(true)} onBlur={() => window.setTimeout(() => setOutboundReceiverSuggestionsOpen(false), 120)} placeholder="例如：张三" autoComplete="off" role="combobox" aria-autocomplete="list" aria-expanded={outboundReceiverSuggestionsOpen} disabled={outboundLoading || mutationDisabled} />
                  {outboundReceiverSuggestionsOpen && !mutationDisabled && <div className="v2-outbound-receiver-suggestions" role="listbox">
                    {catalogLoading && <div className="v2-outbound-receiver-suggestion-empty">正在读取历史客户…</div>}
                    {!catalogLoading && outboundReceiverSuggestions.map((party) => <button key={party.party_id} type="button" role="option" onMouseDown={(event) => event.preventDefault()} onClick={() => { setOutboundReceiver(party.display_name); setOutboundReceiverSuggestionsOpen(false); }}>{party.display_name}<small>历史客户</small></button>)}
                    {!catalogLoading && outboundReceiverSuggestions.length === 0 && <div className="v2-outbound-receiver-suggestion-empty">暂无匹配客户；确认后会自动创建“{outboundReceiver.trim() || "新客户"}”</div>}
                  </div>}
                </div><small>输入关键字会筛选历史上曾经出货过的客户。</small></label>
              </div>
              <div className="v2-rule-hint"><ShieldAlert size={17} /><span>订单号由系统自动生成；需求数量以第二步结束时实际扫描的件数确定。本单可以包含不同品牌、不同型号。</span></div>
              <div className="v2-form-actions"><button className="v2-button primary" type="submit" disabled={outboundLoading || mutationDisabled}>进入扫码出库 <ArrowRight size={16} /></button></div>
            </div>
          </form>}

          {outboundStep === 2 && <section className="v2-outbound-step v2-outbound-scan-step">
            <div className="v2-scanner-heading">
              <div className="v2-receipt-section-heading"><span>2</span><div><h3>优先扫码实际出货单件</h3><small>{outboundReceiver} · 订单号确认出库时自动生成</small></div></div>
              <strong>{outboundScannedItems.length}<small> 件已扫描</small></strong>
            </div>
            <div className="v2-scan-context"><span><strong>扫描原则</strong>只按实际 SN 出货</span><span><strong>数量确定</strong>结束扫码时以当前件数为准</span></div>
            {!outboundShipment && <label className="v2-scan-field">
                <span>扫码枪输入 *</span>
                <div className="v2-scanner-control">
                  <Bell size={21} aria-hidden="true" />
                  <input ref={outboundScannerInputRef} value={outboundScannerInput} onChange={(event) => setOutboundScannerInput(event.target.value)} onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      void addOutboundScannedBarcode();
                    }
                  }} placeholder="请扫描实际出货 SN（扫码枪自动回车）" autoComplete="off" autoCapitalize="characters" spellCheck={false} disabled={outboundScanChecking || outboundLoading || mutationDisabled} />
                  <button className="v2-button" type="button" onClick={() => void addOutboundScannedBarcode()} disabled={!outboundScannerInput.trim() || outboundScanChecking || outboundLoading || mutationDisabled}>{outboundScanChecking ? "正在核对…" : "手动加入"}</button>
                </div>
                <small>系统会实时核验 SN 是否存在、是否已质检且当前可用；不同品牌和型号可以混扫。</small>
              </label>}

              {outboundScanNotice && <div className={`v2-scanner-feedback ${outboundScanNotice.type}`} role={outboundScanNotice.type === "error" ? "alert" : "status"} aria-live={outboundScanNotice.type === "error" ? "assertive" : "polite"}>{outboundScanNotice.type === "error" ? <Bell size={19} /> : <CheckCircle2 size={19} />}<span>{outboundScanNotice.text}</span></div>}

              <div className="v2-scanned-heading">
                <div><strong>已扫描清单</strong><span>{outboundHasScannedItems ? "确认结束扫码后按当前件数出库" : "请扫描实际出货 SN"}</span></div>
                <button className="v2-button" type="button" onClick={() => void clearOutboundScanBatch()} disabled={Boolean(outboundShipment) || outboundScanChecking || outboundLoading || outboundScannedItems.length === 0}>清空扫码</button>
              </div>
              <div className="v2-outbound-checklist" aria-label="实际出货扫码清单">
                {outboundScannedItems.length === 0 && <div className="v2-scanned-empty"><Truck size={26} /><span>等待扫描实际出货 SN</span></div>}
                {outboundScannedItems.map((item) => <div className="scanned" key={item.barcode}>
                  <span><CheckCircle2 size={17} /></span>
                  <strong>{item.barcode}</strong>
                  <small>{item.skuCode} · {item.skuName}</small>
                  {!outboundShipment && <button className="v2-icon-button danger" type="button" onClick={() => removeOutboundScannedBarcode(item.barcode)} disabled={outboundScanChecking || outboundLoading} aria-label={`移除出库 SN ${item.barcode}`} title="移除后重新扫描"><X size={16} /></button>}
                </div>)}
              </div>

              {outboundScannedItems.length > 0 && <div className="v2-outbound-group-summary"><strong>自动归类预览</strong>{outboundScanGroups.map((group) => <span key={group.skuId}><b>{group.skuCode}</b><small>{group.skuName} · {group.count} 件</small></span>)}</div>}

              {!outboundShipment && outboundScannedItems.length > 0 && <div className="v2-warranty-editor">
                <div className="v2-warranty-heading"><span>客户质保（可选）</span><small>保存到本次出库的全部 SN</small></div>
                <div className="v2-warranty-controls">
                  <select value={outboundWarrantyPreset} onChange={(event) => setOutboundWarrantyPreset(event.target.value)} aria-label="客户质保期限">
                    <option value="">无质保</option><option value="7">一个星期（7天）</option><option value="15">半个月（15天）</option><option value="30">一个月（30天）</option><option value="365">一年（365天）</option><option value="custom">自定义天数</option>
                  </select>
                  {outboundWarrantyPreset === "custom" && <input type="number" min="1" max="36500" value={outboundWarrantyCustomDays} onChange={(event) => setOutboundWarrantyCustomDays(event.target.value)} placeholder="天数" aria-label="自定义客户质保天数" />}
                  <label className="v2-inline-check"><input type="checkbox" checked={outboundWarrantyManualStart} onChange={(event) => setOutboundWarrantyManualStart(event.target.checked)} /><span>手动指定起算</span></label>
                  {outboundWarrantyManualStart && <input type="datetime-local" step="1" value={outboundWarrantyStartsAt} onChange={(event) => setOutboundWarrantyStartsAt(event.target.value)} aria-label="客户质保起算时间" />}
                </div>
              </div>}

              {!outboundShipment && <>
                <details className="v2-alternative-entry">
                  <summary><span>备用录入</span><small>批量粘贴实际出货 SN</small><ChevronDown size={16} /></summary>
                  <div className="v2-alternative-content">
                    <label><span>每行一个 SN</span><textarea value={outboundBulkInput} onChange={(event) => setOutboundBulkInput(event.target.value)} placeholder={"SN0001\nSN0002"} disabled={outboundScanChecking || outboundLoading || mutationDisabled} /></label>
                    <button className="v2-button" type="button" onClick={() => void importOutboundBarcodes()} disabled={!outboundBulkInput.trim() || outboundScanChecking || outboundLoading || mutationDisabled}>核验并加入批次</button>
                  </div>
                </details>

                <div className="v2-outbound-submit-row">
                  <label><span>出库批次号（可选）</span><input value={outboundShipmentNo} onChange={(event) => setOutboundShipmentNo(event.target.value)} placeholder="留空自动生成" disabled={outboundScanChecking || outboundLoading || mutationDisabled} /></label>
                  {outboundHasScannedItems && !outboundLoading && <button className="v2-button primary" type="button" onClick={() => void completeOutboundScanAndShip()} disabled={outboundScanChecking || mutationDisabled}>{finishOutboundLabel}</button>}
                </div>
              </>}
              <div className="v2-workflow-actions">
                <button className="v2-button" type="button" onClick={() => navigateOutboundStep(1)} disabled={outboundLoading || outboundScanChecking}>上一步</button>
              </div>
          </section>}

          {outboundStep === 3 && <section className="v2-outbound-step">
            <div className="v2-receipt-section-heading"><span>3</span><div><h3>交货确认或退回</h3><small>{outboundShipment ? outboundShipment.shipment_no : "完成自动出库后可用"}</small></div></div>
            {!outboundShipment && <div className="v2-step-blocked">请先完成第 2 步，扫码并自动出库。</div>}
            {outboundShipment && <>
              <div className="v2-step-summary"><span><small>出库批次</small><strong>{outboundShipment.shipment_no}</strong></span><span><small>出库数量</small><strong>{outboundShipment.shipped_count} 件</strong></span><span><small>订单</small><strong>{outboundOrder?.order_no ?? "—"}</strong></span><span><small>品牌/型号</small><strong>{outboundScanGroups.length} 组</strong></span></div>
              <div className="v2-outbound-group-summary"><strong>本单自动归类结果</strong>{outboundScanGroups.map((group) => <span key={group.skuId}><b>{group.skuCode}</b><small>{group.skuName} · {group.count} 件</small></span>)}</div>
              {!outboundResolved && <div className="v2-delivery-actions">
                <div>
                  <label><span>上游确认码</span><input value={outboundConfirmationCode} onChange={(event) => setOutboundConfirmationCode(event.target.value)} placeholder="签收单号 / 确认码" disabled={outboundLoading || mutationDisabled} /></label>
                  <button className="v2-button primary" type="button" onClick={() => void confirmOutboundDelivery()} disabled={outboundLoading || mutationDisabled}>确认已交货</button>
                </div>
                <div>
                  <span>后续发生退货时按实际 SN 逐件处理</span>
                  <button className="v2-button" type="button" onClick={() => navigateToPage("returns")} disabled={outboundLoading || mutationDisabled}><RotateCcw size={16} /> 前往扫码退货</button>
                </div>
              </div>}
              {outboundResolved && <div className="v2-inline-success"><CheckCircle2 size={18} /><span>本单已完成交货或退回处理，可以开始下一单。</span></div>}
              <div className="v2-workflow-actions">
                <button className="v2-button" type="button" onClick={() => navigateOutboundStep(2)} disabled={outboundLoading}>上一步</button>
                {outboundResolved && <button className="v2-button primary" type="button" onClick={startNextOutboundOrder}>开始下一单 <ArrowRight size={16} /></button>}
              </div>
            </>}
          </section>}
        </div>
        {outboundNotice && <div className={`v2-notice ${outboundNotice.type}`}>{outboundNotice.text}</div>}
      </section>
    );
  }

  function renderSettings() {
    return (
      <section className="v2-page" aria-labelledby="v2-settings-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">数据安全</span><h2 id="v2-settings-title">备份、恢复与版本升级</h2><p>离线数据恢复和一次性网络升级。</p></div>
        </div>
        <section className="v2-panel v2-query-settings-panel">
          <div className="v2-settings-heading"><div><h3>查询输入行为</h3><small>设置查询页面和退货录入按回车查询后是否清空输入框，扫码枪连续查询时更方便。</small></div><SlidersHorizontal size={20} /></div>
          <div className="v2-query-settings-list">
            <label className="v2-settings-toggle"><input type="checkbox" checked={searchClearPreferences.inventory} onChange={(event) => updateSearchClearPreference("inventory", event.target.checked)} /><span><strong>库存查询与退货录入</strong><small>回车查询完成后清空库存搜索词或退货 SN</small></span></label>
            <label className="v2-settings-toggle"><input type="checkbox" checked={searchClearPreferences.lifecycle} onChange={(event) => updateSearchClearPreference("lifecycle", event.target.checked)} /><span><strong>生命周期</strong><small>回车查询完成后清空 SN 输入框</small></span></label>
            <label className="v2-settings-toggle"><input type="checkbox" checked={searchClearPreferences.records} onChange={(event) => updateSearchClearPreference("records", event.target.checked)} /><span><strong>单据查询</strong><small>回车查询完成后清空单据搜索词</small></span></label>
          </div>
          <div className="v2-query-settings-actions"><span>默认关闭，避免查询后失去当前检索条件。</span><button className="v2-button" type="button" onClick={resetSearchClearPreferences}>恢复默认</button></div>
        </section>
        <div className="v2-settings-grid">
          {mode === "offline" && <form className="v2-panel v2-settings-panel v2-operation-password-panel" onSubmit={(event) => void changeOperationPassword(event)}>
            <div className="v2-settings-heading"><div><h3>危险操作密码</h3><small>用于作废入库单和出库单</small></div><KeyRound size={20} /></div>
            <div className="v2-rule-hint"><ShieldAlert size={17} /><span>初始密码为 admin。密码只以 Argon2id 哈希保存在本机数据库中。</span></div>
            <label className="v2-settings-field"><span>当前密码</span><input type="password" value={operationCurrentPassword} onChange={(event) => setOperationCurrentPassword(event.target.value)} autoComplete="current-password" disabled={operationPasswordLoading} /></label>
            <label className="v2-settings-field"><span>新密码</span><input type="password" value={operationNewPassword} onChange={(event) => setOperationNewPassword(event.target.value)} autoComplete="new-password" minLength={5} maxLength={128} disabled={operationPasswordLoading} /></label>
            <label className="v2-settings-field"><span>确认新密码</span><input type="password" value={operationConfirmPassword} onChange={(event) => setOperationConfirmPassword(event.target.value)} autoComplete="new-password" minLength={5} maxLength={128} disabled={operationPasswordLoading} /></label>
            <button className="v2-button primary wide" type="submit" disabled={operationPasswordLoading || !operationCurrentPassword || operationNewPassword.length < 5 || !operationConfirmPassword}>{operationPasswordLoading ? "正在更新…" : "更新操作密码"}</button>
            {operationPasswordNotice && <div className={`v2-notice ${operationPasswordNotice.type}`}>{operationPasswordNotice.text}</div>}
          </form>}
          <section className="v2-panel v2-settings-panel">
            <div className="v2-settings-heading"><div><h3>离线 SQLite 备份</h3><small>工作区一致性快照</small></div><Warehouse size={20} /></div>
            <button className="v2-button primary wide" type="button" onClick={() => void createOfflineBackup()} disabled={dataOperationLoading}>创建备份</button>
            <button className="v2-button wide" type="button" onClick={() => void restoreOfflineBackup()} disabled={dataOperationLoading}>验证并恢复备份</button>
            {restoreReport && <div className={`v2-restore-report ${restoreReport.status}`}>
              <strong>{restoreReport.status === "restored" ? "最近恢复成功" : "最近恢复失败"}</strong>
              <span>{formatDateTime(restoreReport.completed_at)}</span>
              {restoreReport.pre_restore_backup && <small>恢复前备份：{restoreReport.pre_restore_backup}</small>}
              {restoreReport.error && <small>{restoreReport.error}</small>}
            </div>}
          </section>

          <section className="v2-panel v2-settings-panel v2-upgrade-panel">
            <div className="v2-settings-heading"><div><h3>一次性升级到网络版</h3><small>离线 SQLite → 网络 PostgreSQL</small></div><ShieldAlert size={20} /></div>
            <div className="v2-upgrade-steps">
              <div><span>1</span><strong>生成升级包</strong><button className="v2-button" type="button" onClick={() => void exportUpgradePackage()} disabled={dataOperationLoading}>导出 .invpack</button></div>
              <div><span>2</span><strong>选择升级包</strong><button className="v2-button" type="button" onClick={() => void chooseUpgradePackage()} disabled={dataOperationLoading}>选择目录</button></div>
              <div><span>3</span><strong>导入空工作区</strong><button className="v2-button primary" type="button" onClick={() => void upgradeOfflineToNetwork()} disabled={dataOperationLoading || !networkStatus?.authenticated}>执行升级</button></div>
            </div>
            <label className="v2-settings-field"><span>升级包路径</span><input value={upgradePackagePath} onChange={(event) => setUpgradePackagePath(event.target.value)} placeholder="选择 .invpack 目录" disabled={dataOperationLoading} /></label>
            <label className="v2-settings-field"><span>目标网络工作区 ID</span><input value={upgradeTargetWorkspaceId} onChange={(event) => setUpgradeTargetWorkspaceId(event.target.value)} placeholder="UUID" disabled={dataOperationLoading} /></label>
            <div className="v2-rule-hint"><ShieldAlert size={17} /><span>服务端确认条码、关系、计数和 checksum 完全一致后，本地库才会冻结。冻结后不再允许新增或修改业务数据。</span></div>
            {!networkStatus?.authenticated && <div className="v2-notice error">执行导入前，请切换到网络版并登录具备升级权限的账号。</div>}
            {upgradeExport && <div className="v2-upgrade-result"><strong>已生成升级包</strong><span>export_id：{upgradeExport.export_id}</span><span>checksum：{upgradeExport.checksum}</span></div>}
            {upgradeImport && <div className="v2-upgrade-result success"><strong>服务端已确认导入</strong><span>migration_id：{upgradeImport.import.migration_id}</span><span>状态：{upgradeImport.import.status}</span><span>本地归档：{upgradeImport.local_archived ? "已完成" : "未完成"}</span></div>}
          </section>
        </div>
        {dataNotice && <div className={`v2-notice ${dataNotice.type}`}>{dataNotice.text}</div>}
      </section>
    );
  }

  const availableNavigationItems = navigationItems.filter((item) => !item.mode || item.mode === mode);
  const overviewNavigationItem = availableNavigationItems.find((item) => item.id === "overview")!;
  const OverviewIcon = overviewNavigationItem.icon;
  const modeSwitchDisabled = workspaceOperationInProgress();

  return (
    <div className="v2-workspace">
      <aside className="v2-sidebar">
        <div className="v2-brand"><span><Boxes size={22} /></span><div><strong>库存管理</strong><small>{mode === "network" ? "团队协作工作区" : "本机工作区"}</small></div></div>
        <div className="v2-mode-switch" aria-label="工作模式">
          <button type="button" className={mode === "offline" ? "active" : ""} onClick={() => void switchMode("offline")} disabled={modeSwitchDisabled}>本机版</button>
          <button type="button" className={mode === "network" ? "active" : ""} onClick={() => void switchMode("network")} disabled={modeSwitchDisabled}>团队版</button>
        </div>
        <nav className="v2-desktop-navigation" aria-label="库存模块">
          <button type="button" className={`v2-nav-overview ${page === "overview" ? "active" : ""}`} onClick={() => navigateToPage("overview")} disabled={modeSwitchDisabled}><OverviewIcon size={19} /><span><strong>{overviewNavigationItem.label}</strong><small>{overviewNavigationItem.description}</small></span></button>
          <div className="v2-nav-groups">
            {navigationGroups.map((group) => {
              const groupItems = availableNavigationItems.filter((item) => item.group === group.id);
              if (groupItems.length === 0) return null;
              const GroupIcon = group.icon;
              const expanded = expandedNavGroups.has(group.id);
              const active = groupItems.some((item) => item.id === page);
              return <section className={`v2-nav-group ${active ? "active" : ""}`} key={group.id}>
                <button className="v2-nav-group-toggle" type="button" onClick={() => toggleNavGroup(group.id)} aria-expanded={expanded}><GroupIcon size={16} /><strong>{group.label}</strong><ChevronDown size={15} className={expanded ? "expanded" : ""} /></button>
                {expanded && <div className="v2-nav-group-items">{groupItems.map((item) => {
                  const Icon = item.icon;
                  return <button key={item.id} type="button" className={page === item.id ? "active" : ""} onClick={() => navigateToPage(item.id)} disabled={modeSwitchDisabled}><Icon size={17} /><span><strong>{item.label}</strong><small>{item.description}</small></span></button>;
                })}</div>}
              </section>;
            })}
          </div>
        </nav>
        <label className="v2-mobile-navigation"><span>当前模块</span><select value={page} onChange={(event) => navigateToPage(event.target.value as WorkspacePage)} disabled={modeSwitchDisabled}><option value="overview">概览</option>{navigationGroups.map((group) => {
          const groupItems = availableNavigationItems.filter((item) => item.group === group.id);
          return groupItems.length > 0 ? <optgroup label={group.label} key={group.id}>{groupItems.map((item) => <option value={item.id} key={item.id}>{item.label}</option>)}</optgroup> : null;
        })}</select></label>
        <div className="v2-sidebar-footer">
          <span>{mode === "network" ? "团队版" : "本机版"}</span>
          <small>{mode === "network" ? (networkStatus?.authenticated ? `已登录 · ${networkStatus.user_id ?? ""}` : "多人协作 · 服务端授权") : (offlineActivated ? "单机使用 · 自动保存" : "授权无效 · 只读和备份可用")}</small>
          {mode === "network" && networkStatus?.authenticated && <button className="v2-back-button" type="button" onClick={() => void logoutNetwork()} disabled={modeSwitchDisabled}><LogOut size={15} /> 退出团队版</button>}
          {mode === "offline" && !offlineActivated && onRequestActivation && <button className="v2-back-button" type="button" onClick={onRequestActivation} disabled={modeSwitchDisabled}><ShieldAlert size={15} /> 离线激活</button>}
          {onBackToLegacy && <button className="v2-back-button" type="button" onClick={onBackToLegacy} disabled={modeSwitchDisabled}><ArrowLeft size={16} /> 返回旧版工具</button>}
        </div>
      </aside>
      <main className="v2-content">
        {mode === "offline" && !offlineActivated && <div className="v2-readonly-banner"><ShieldAlert size={17} /><span>离线授权当前无效：查询、备份和恢复仍可使用，入库、质检、凑单、出库与退回已锁定。</span></div>}
        {mode === "network" && !networkStatus?.authenticated ? renderNetworkLogin() : <>
          {page === "overview" && renderOverview()}
          {page === "catalog" && renderCatalog()}
          {page === "receipt" && renderReceipt()}
          {page === "quality" && renderQuality()}
          {page === "inventory" && renderInventory()}
          {page === "lifecycle" && renderLifecycle()}
          {page === "records" && renderRecords()}
          {page === "returns" && renderReturns()}
          {page === "outbound" && renderOutbound()}
          {page === "legacy-import" && <LegacyImportPanel actorId={resolvedActorId} activated={offlineActivated} onBusyChange={handleChildPanelBusyChange} onCommitted={() => void refreshDashboard()} />}
          {page === "users" && <IdentityAdminPanel currentUserId={networkStatus?.user_id ?? null} onBusyChange={handleChildPanelBusyChange} />}
          {page === "settings" && renderSettings()}
        </>}
      </main>
    </div>
  );
}
