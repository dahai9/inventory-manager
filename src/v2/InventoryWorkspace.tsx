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
  Bell,
  Boxes,
  CheckCircle2,
  ChevronDown,
  ClipboardCheck,
  FileSpreadsheet,
  Gauge,
  LogOut,
  PackagePlus,
  Pencil,
  Plus,
  RefreshCw,
  Search,
  Settings,
  ShieldAlert,
  Truck,
  Tags,
  Users,
  Warehouse,
  X,
  type LucideIcon,
} from "lucide-react";
import IdentityAdminPanel from "./IdentityAdminPanel";
import LegacyImportPanel from "./LegacyImportPanel";
import "./InventoryWorkspace.css";

type WorkspacePage = "overview" | "catalog" | "receipt" | "quality" | "inventory" | "outbound" | "legacy-import" | "users" | "settings";
type WorkspaceMode = "offline" | "network";
type NavigationGroupId = "operations" | "inventory_data" | "catalog" | "system";
type CatalogTab = "products" | "parties";
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
type Notice = { type: "success" | "warning" | "error"; text: string };

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

interface CreateCatalogProductRequest {
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
  received_at: string;
  inventory_status: InventoryStatus;
  quality_status: QualityStatus;
  inspections: Array<{
    inspection_no: string;
    inspection_type: InspectionKind;
    result: QualityOutcome;
    inspected_at: string;
    defect_code: string | null;
    notes: string | null;
  }>;
  outbound: Array<{
    allocation_id: string;
    allocation_status: string;
    allocated_at: string;
    order_id: string;
    order_no: string;
    upstream_receiver_name: string;
    shipment_id: string | null;
    shipment_no: string | null;
    shipped_at: string | null;
    confirmation_code: string | null;
    confirmed_at: string | null;
    delivery_result: string | null;
    return_no: string | null;
    returned_at: string | null;
    return_reason: string | null;
    return_disposition: string | null;
  }>;
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

interface NetworkAllocateOutboundRequest {
  request_id: string;
  idempotency_key: string;
  order_id: string;
  order_line_id: string;
  barcodes: string[];
}

interface NetworkShipOutboundRequest {
  request_id: string;
  idempotency_key: string;
  order_id: string;
  shipment_no: string;
  allocation_ids: string[];
  barcodes: string[];
  shipped_at: string;
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
  { id: "inventory", label: "库存查询", description: "单件库存与追溯", icon: Boxes, group: "inventory_data" },
  { id: "legacy-import", label: "Excel 导入", description: "历史数据迁移", icon: FileSpreadsheet, group: "inventory_data", mode: "offline" },
  { id: "catalog", label: "资料维护", description: "商品与往来方", icon: Tags, group: "catalog" },
  { id: "users", label: "用户与角色", description: "账号和权限", icon: Users, group: "system", mode: "network" },
  { id: "settings", label: "数据与设置", description: "备份、恢复和升级", icon: Settings, group: "system" },
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

function getStoredNetworkValue(key: string): string {
  try {
    return window.localStorage.getItem(key) ?? "";
  } catch {
    return "";
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

  const [catalog, setCatalog] = useState<ReferenceCatalog | null>(null);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [catalogNotice, setCatalogNotice] = useState<Notice | null>(null);
  const [catalogTab, setCatalogTab] = useState<CatalogTab>("products");
  const [catalogCreateOpen, setCatalogCreateOpen] = useState(false);
  const [newProductCode, setNewProductCode] = useState("");
  const [newProductName, setNewProductName] = useState("");
  const [newProductSerialPrefix, setNewProductSerialPrefix] = useState("");
  const [newProductForbiddenChars, setNewProductForbiddenChars] = useState("-, ");
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

  const [qualityItems, setQualityItems] = useState<InventoryListItem[]>([]);
  const [qualityLoading, setQualityLoading] = useState(false);
  const [qualityNotice, setQualityNotice] = useState<Notice | null>(null);
  const [selectedBarcodes, setSelectedBarcodes] = useState<Set<string>>(() => new Set());
  const [qualityScannerInput, setQualityScannerInput] = useState("");
  const [qualityBulkInput, setQualityBulkInput] = useState("");
  const [qualityScanNotice, setQualityScanNotice] = useState<Notice | null>(null);
  const [qualityScanChecking, setQualityScanChecking] = useState(false);
  const qualityScannerInputRef = useRef<HTMLInputElement>(null);
  const qualityScanCheckingRef = useRef(false);
  const [inspectionKind, setInspectionKind] = useState<InspectionKind>("initial");
  const [inspectionOutcome, setInspectionOutcome] = useState<QualityOutcome>("passed");
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

  const [outboundReceiver, setOutboundReceiver] = useState("");
  const [outboundOrderNo, setOutboundOrderNo] = useState("");
  const [outboundSkuCode, setOutboundSkuCode] = useState("");
  const [outboundSkuName, setOutboundSkuName] = useState("");
  const [outboundQuantity, setOutboundQuantity] = useState("1");
  const [outboundBarcodes, setOutboundBarcodes] = useState("");
  const [outboundScannerInput, setOutboundScannerInput] = useState("");
  const [outboundBulkInput, setOutboundBulkInput] = useState("");
  const [outboundScannedBarcodes, setOutboundScannedBarcodes] = useState<string[]>([]);
  const [outboundScanNotice, setOutboundScanNotice] = useState<Notice | null>(null);
  const outboundScannerInputRef = useRef<HTMLInputElement>(null);
  const outboundScanCheckingRef = useRef(false);
  const [outboundScanChecking, setOutboundScanChecking] = useState(false);
  const [outboundShipmentNo, setOutboundShipmentNo] = useState("");
  const [outboundConfirmationCode, setOutboundConfirmationCode] = useState("");
  const [outboundReturnReason, setOutboundReturnReason] = useState("");
  const [outboundNotice, setOutboundNotice] = useState<Notice | null>(null);
  const [outboundLoading, setOutboundLoading] = useState(false);
  const [outboundOrder, setOutboundOrder] = useState<CreateOutboundOrderResponse | null>(null);
  const [outboundAllocation, setOutboundAllocation] = useState<AllocateOutboundResponse | null>(null);
  const [outboundShipment, setOutboundShipment] = useState<ShipOutboundResponse | null>(null);
  const [outboundResolved, setOutboundResolved] = useState(false);

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
  const receiptMissingDetails = [
    !selectedProduct ? "商品" : null,
    !catalog?.suppliers.some((party) => party.display_name === supplierName) ? "供应商" : null,
    !receivedAt ? "入库时间" : null,
    mode === "network" && !networkWarehouses.some((warehouse) => warehouse.warehouse_id === networkWarehouseId)
      ? "入库仓库"
      : null,
  ].filter((value): value is string => Boolean(value));
  const receiptDetailsReady = receiptMissingDetails.length === 0;
  const barcodes = scannedBarcodes;

  const eligibleQualityItems = useMemo(() => {
    return qualityItems.filter((item) => isInspectionEligible(item, inspectionKind));
  }, [inspectionKind, qualityItems]);

  const outboundAllocatedBarcodes = useMemo(
    () => outboundAllocation?.allocations.map((item) => item.barcode.toUpperCase()) ?? [],
    [outboundAllocation],
  );
  const outboundScanComplete = outboundAllocatedBarcodes.length > 0
    && outboundScannedBarcodes.length === outboundAllocatedBarcodes.length
    && outboundScannedBarcodes.every((barcode) => outboundAllocatedBarcodes.includes(barcode));

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
      setSupplierName((current) => nextCatalog.suppliers.some((party) => party.display_name === current)
        ? current
        : (nextCatalog.suppliers[0]?.display_name ?? ""));
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
      if (context === workspaceContextRef.current) setDashboard(response);
    } catch (error) {
      if (context === workspaceContextRef.current) setDashboardError(displayError(error));
    } finally {
      if (context === workspaceContextRef.current) setDashboardLoading(false);
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

  const refreshInventory = useCallback(async () => {
    const context = workspaceContextRef.current;
    if (mode === "network" && !networkStatus?.authenticated) return;
    setInventoryLoading(true);
    setInventoryError(null);
    try {
      const query: InventoryListQuery = {
        ...emptyInventoryQuery(),
        search: inventorySearch.trim() || null,
        inventory_status: inventoryStatus || null,
        quality_status: qualityStatus || null,
      };
      const command = mode === "network" ? "v2_network_list_inventory" : "v2_list_inventory";
      const response = await invoke<InventoryListResponse>(command, { query });
      if (context !== workspaceContextRef.current) return;
      setInventoryItems(response.items);
      setInventoryTotal(response.total);
    } catch (error) {
      if (context === workspaceContextRef.current) setInventoryError(displayError(error));
    } finally {
      if (context === workspaceContextRef.current) setInventoryLoading(false);
    }
  }, [inventorySearch, inventoryStatus, qualityStatus, mode, networkStatus?.authenticated]);

  async function openInventoryTrace(barcode: string) {
    const context = workspaceContextRef.current;
    setInventoryTraceBarcode(barcode);
    setInventoryTrace(null);
    setInventoryTraceError(null);
    setInventoryTraceLoading(true);
    try {
      const command = mode === "network" ? "v2_network_inventory_trace" : "v2_inventory_trace";
      const trace = await invoke<InventoryTrace>(command, { barcode });
      if (context === workspaceContextRef.current) setInventoryTrace(trace);
    } catch (error) {
      if (context === workspaceContextRef.current) setInventoryTraceError(displayError(error));
    } finally {
      if (context === workspaceContextRef.current) setInventoryTraceLoading(false);
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
    if (page === "catalog" || page === "receipt") void refreshCatalog();
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
    if (page === "quality") void refreshQualityItems();
    if (page === "inventory") void refreshInventory();
  }, [mode, networkStatus?.authenticated, page, refreshDashboard, refreshInventory, refreshQualityItems]);

  useEffect(() => {
    setSelectedBarcodes(new Set());
    setQualityScannerInput("");
    setQualityBulkInput("");
    setQualityScanNotice(null);
    setQualityNotice(null);
    window.requestAnimationFrame(() => qualityScannerInputRef.current?.focus());
  }, [inspectionKind]);

  useEffect(() => {
    let focusFrame: number | null = null;
    if (page === "receipt" && receiptDetailsReady && !catalogLoading && !receiptLoading && !scanChecking) {
      focusFrame = window.requestAnimationFrame(() => scannerInputRef.current?.focus());
    } else if (page === "quality" && !qualityLoading && !qualityScanChecking) {
      focusFrame = window.requestAnimationFrame(() => qualityScannerInputRef.current?.focus());
    } else if (page === "outbound" && outboundAllocation && !outboundLoading && !outboundScanChecking && !outboundShipment) {
      focusFrame = window.requestAnimationFrame(() => outboundScannerInputRef.current?.focus());
    }
    return () => {
      if (focusFrame !== null) window.cancelAnimationFrame(focusFrame);
    };
  }, [catalogLoading, page, outboundAllocation, outboundLoading, outboundScanChecking, outboundShipment, qualityLoading, qualityScanChecking, receiptDetailsReady, receiptLoading, scanChecking]);

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
      setOutboundOrderNo("");
      setOutboundSkuCode("");
      setOutboundSkuName("");
    } else {
      setOutboundOrderNo("");
    }
    setOutboundQuantity("1");
    setOutboundBarcodes("");
    setOutboundScannerInput("");
    setOutboundBulkInput("");
    setOutboundScannedBarcodes([]);
    setOutboundShipmentNo("");
    setOutboundConfirmationCode("");
    setOutboundReturnReason("");
    setOutboundScanNotice(null);
    setOutboundNotice(null);
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

    setCatalog(null);
    setCatalogLoading(false);
    setCatalogNotice(null);
    setCatalogCreateOpen(false);
    resetCatalogDraft();
    setSelectedProductId("");
    setSupplierName("");

    setScannedBarcodes([]);
    setScannerInput("");
    setReceiptBulkInput("");
    setReceiptNotice(null);
    setReceiptLoading(false);
    setSourceReference("");
    setReceivedAt(getLocalDateTimeValue());

    setQualityItems([]);
    setQualityLoading(false);
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
    const activeScanCount = scannedBarcodes.length + selectedBarcodes.size + outboundScannedBarcodes.length;
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
      const input: CreateCatalogProductRequest = {
        code: newProductCode,
        name: newProductName,
        serial_prefix: newProductSerialPrefix.trim() || null,
        serial_forbidden_chars: newProductForbiddenChars,
      };
      const command = mode === "network" ? "v2_network_create_catalog_product" : "v2_create_catalog_product";
      const product = await invoke<CatalogProduct>(command, { input });
      setSelectedProductId(product.sku_id);
      setNewProductCode("");
      setNewProductName("");
      setNewProductSerialPrefix("");
      setNewProductForbiddenChars("-, ");
      setCatalogNotice({ type: "success", text: `已创建商品 ${product.code}。` });
      await refreshCatalog();
      setCatalogCreateOpen(false);
    } catch (error) {
      setCatalogNotice({ type: "error", text: `创建商品失败：${displayError(error)}` });
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
      if (party.roles.includes("supplier")) setSupplierName(party.display_name);
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
      setScannedBarcodes([]);
      setScannerInput("");
      setSourceReference("");
      setReceivedAt(getLocalDateTimeValue());
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
      throw new Error(`SN ${trace.barcode} 当前为“${inventoryStatusLabels[trace.inventory_status]} / ${qualityStatusLabels[trace.quality_status]}”，不属于${required}。`);
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
    if (inspectionOutcome === "failed" && !defectCode.trim() && !inspectionNotes.trim()) {
      setQualityNotice({ type: "error", text: "不合格结果请填写缺陷代码或备注。" });
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

    const completionText = `${response.inspection_no} 已完成：合格 ${response.passed_count} 件，不合格 ${response.failed_count} 件${
      response.idempotent_replay ? "（幂等回放）" : ""
    }。`;
    setDefectCode("");
    setInspectionNotes("");
    setSelectedBarcodes(new Set());
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

  async function createOutboundOrder(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setOutboundNotice(null);
    if (outboundOrder) {
      setOutboundNotice({ type: "error", text: "当前出库订单尚未结束，不能重复建单。" });
      return;
    }
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
      const common = {
        request_id: operationId,
        idempotency_key: `outbound-order:${operationId}`,
        order_no: outboundOrderNo.trim(),
        upstream_receiver_name: outboundReceiver.trim(),
        sku_code: outboundSkuCode.trim(),
        sku_name: outboundSkuName.trim(),
        required_quantity: quantity,
        required_at: null,
      };
      const command = mode === "network" ? "v2_network_create_outbound_order" : "v2_create_outbound_order";
      const input = mode === "network"
        ? (common satisfies NetworkCreateOutboundOrderRequest)
        : ({ ...common, actor_id: resolvedActorId } satisfies CreateOutboundOrderRequest);
      const response = await invoke<CreateOutboundOrderResponse>(command, { input });
      setOutboundOrder(response);
      setOutboundResolved(false);
      setOutboundAllocation(null);
      setOutboundShipment(null);
      setOutboundBarcodes("");
      setOutboundScannerInput("");
      setOutboundBulkInput("");
      setOutboundScannedBarcodes([]);
      setOutboundScanNotice(null);
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
      const common = {
        request_id: operationId,
        idempotency_key: `outbound-allocation:${operationId}`,
        order_id: outboundOrder.order_id,
        order_line_id: outboundOrder.order_line_id,
        barcodes: parseBarcodeLines(outboundBarcodes),
      };
      const command = mode === "network" ? "v2_network_allocate_outbound_order" : "v2_allocate_outbound_order";
      const input = mode === "network"
        ? (common satisfies NetworkAllocateOutboundRequest)
        : ({ ...common, actor_id: resolvedActorId });
      const response = await invoke<AllocateOutboundResponse>(command, { input });
      setOutboundAllocation(response);
      setOutboundBarcodes("");
      setOutboundScannerInput("");
      setOutboundBulkInput("");
      setOutboundScannedBarcodes([]);
      setOutboundScanNotice(null);
      setOutboundNotice({ type: "success", text: `已分配 ${response.allocated_count} 件，状态：${response.order_status}。` });
    } catch (error) {
      setOutboundNotice({ type: "error", text: `库存分配失败：${displayError(error)}` });
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

  function validateOutboundBarcodeBatch(values: string[]): string[] {
    if (!outboundAllocation || outboundAllocatedBarcodes.length === 0) {
      throw new Error("请先分配待出库库存，再开始扫码核对。");
    }
    const known = new Set(outboundScannedBarcodes);
    const validated: string[] = [];
    for (const value of values) {
      const barcode = value.trim().toUpperCase();
      if (!barcode) throw new Error("SN 不能为空。");
      if (known.has(barcode)) throw new Error(`SN ${barcode} 已经扫描过，请勿重复扫描。`);
      if (!outboundAllocatedBarcodes.includes(barcode)) {
        throw new Error(`SN ${barcode} 不属于当前订单已分配的库存，禁止出库。`);
      }
      known.add(barcode);
      validated.push(barcode);
    }
    return validated;
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
      const validated = validateOutboundBarcodeBatch(candidates);
      const barcode = validated[0];
      setOutboundScannedBarcodes((current) => [...current, barcode]);
      const nextCount = outboundScannedBarcodes.length + 1;
      setOutboundScanNotice({
        type: "success",
        text: nextCount === outboundAllocatedBarcodes.length
          ? `SN ${barcode} 核对通过，当前订单 ${nextCount} 件已全部扫齐。`
          : `SN ${barcode} 核对通过，还需扫描 ${outboundAllocatedBarcodes.length - nextCount} 件。`,
      });
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
    try {
      const validated = validateOutboundBarcodeBatch(candidates);
      setOutboundScannedBarcodes((current) => [...current, ...validated]);
      setOutboundBulkInput("");
      setOutboundScanNotice({
        type: "success",
        text: `备用批量录入已核对并加入 ${validated.length} 个 SN。`,
      });
    } catch (error) {
      await rejectOutboundScan(`备用批量录入已取消：${displayError(error)}`);
    } finally {
      outboundScanCheckingRef.current = false;
      setOutboundScanChecking(false);
    }
  }

  function removeOutboundScannedBarcode(barcode: string) {
    if (outboundScanCheckingRef.current || outboundLoading) return;
    setOutboundScannedBarcodes((current) => current.filter((item) => item !== barcode));
    setOutboundScanNotice({ type: "success", text: `已从出库核对批次移除 SN ${barcode}。` });
    outboundScannerInputRef.current?.focus();
  }

  async function clearOutboundScanBatch() {
    if (outboundScannedBarcodes.length === 0 || outboundScanCheckingRef.current || outboundLoading) return;
    const approved = await confirm(
      `确定清空已核对的 ${outboundScannedBarcodes.length} 个出库 SN 吗？`,
      { title: "清空出库扫码", kind: "warning" },
    );
    if (!approved) return;
    setOutboundScannedBarcodes([]);
    setOutboundScanNotice({ type: "success", text: "当前出库扫码核对已清空。" });
    outboundScannerInputRef.current?.focus();
  }

  async function shipOutboundOrder() {
    if (outboundScanCheckingRef.current || !outboundOrder || !outboundAllocation || outboundAllocation.allocations.length === 0) return;
    if (!outboundScanComplete) {
      await rejectOutboundScan(`当前只核对 ${outboundScannedBarcodes.length} / ${outboundAllocatedBarcodes.length} 件，必须逐件扫齐后才能出库。`);
      return;
    }
    setOutboundLoading(true);
    setOutboundNotice(null);
    try {
      const operationId = createId();
      const common = {
        request_id: operationId,
        idempotency_key: `outbound-shipment:${operationId}`,
        order_id: outboundOrder.order_id,
        shipment_no: outboundShipmentNo.trim() || makeDocumentNumber("CK"),
        allocation_ids: [],
        barcodes: outboundScannedBarcodes,
        shipped_at: new Date().toISOString(),
      };
      const command = mode === "network" ? "v2_network_ship_outbound_order" : "v2_ship_outbound_order";
      const input = mode === "network"
        ? (common satisfies NetworkShipOutboundRequest)
        : ({ ...common, actor_id: resolvedActorId });
      const response = await invoke<ShipOutboundResponse>(command, { input });
      setOutboundShipment(response);
      setOutboundShipmentNo(response.shipment_no);
      setOutboundNotice({ type: "success", text: `${response.shipment_no} 已出库 ${response.shipped_count} 件。` });
      setOutboundScanNotice({ type: "success", text: `服务端已按实际扫描的 ${response.shipped_count} 个 SN 完成原子出库。` });
      void refreshDashboard();
    } catch (error) {
      const message = `出库失败：${displayError(error)}`;
      setOutboundNotice({ type: "error", text: message });
      await rejectOutboundScan(message);
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
      const common = {
        request_id: operationId,
        idempotency_key: `outbound-return:${operationId}`,
        shipment_id: outboundShipment.shipment_id,
        shipment_line_ids: [],
        return_no: makeDocumentNumber("TH"),
        returned_at: new Date().toISOString(),
        reason: outboundReturnReason.trim(),
      };
      const command = mode === "network" ? "v2_network_return_outbound_shipment" : "v2_return_outbound_shipment";
      const input = mode === "network"
        ? (common satisfies NetworkReturnOutboundShipmentRequest)
        : ({ ...common, actor_id: resolvedActorId });
      const response = await invoke<ReturnOutboundShipmentResponse>(command, { input });
      setOutboundNotice({ type: "success", text: `${response.return_no} 已登记退回 ${response.quarantined_count} 件，并进入隔离区待复检。` });
      setOutboundReturnReason("");
      setOutboundResolved(true);
      void refreshDashboard();
    } catch (error) {
      setOutboundNotice({ type: "error", text: `退回登记失败：${displayError(error)}` });
    } finally {
      setOutboundLoading(false);
    }
  }

  function startNextOutboundOrder() {
    setOutboundOrder(null);
    setOutboundAllocation(null);
    setOutboundShipment(null);
    setOutboundResolved(false);
    setOutboundOrderNo("");
    setOutboundQuantity("1");
    setOutboundBarcodes("");
    setOutboundScannerInput("");
    setOutboundBulkInput("");
    setOutboundScannedBarcodes([]);
    setOutboundShipmentNo("");
    setOutboundConfirmationCode("");
    setOutboundReturnReason("");
    setOutboundScanNotice(null);
    setOutboundNotice(null);
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
        <section className="v2-overview-shortcuts" aria-label="常用作业">
          <strong>常用作业</strong>
          <div>
            <button type="button" onClick={() => navigateToPage("receipt")} disabled={modeSwitchDisabled}><PackagePlus size={18} /><span><b>入库</b><small>新建扫码批次</small></span></button>
            <button type="button" onClick={() => navigateToPage("quality")} disabled={modeSwitchDisabled}><ClipboardCheck size={18} /><span><b>质检</b><small>{quality?.untested ?? 0} 件待检</small></span></button>
            <button type="button" onClick={() => navigateToPage("inventory")} disabled={modeSwitchDisabled}><Boxes size={18} /><span><b>库存</b><small>{dashboard?.total_units ?? 0} 件可追溯</small></span></button>
            <button type="button" onClick={() => navigateToPage("outbound")} disabled={modeSwitchDisabled}><Truck size={18} /><span><b>出库</b><small>凑单与交货</small></span></button>
          </div>
        </section>
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
                <thead><tr><th>商品编码</th><th>商品名称</th><th>SN 前缀</th><th>禁用字符或片段</th></tr></thead>
                <tbody>
                  {!catalogLoading && products.length === 0 && <tr><td className="v2-table-empty" colSpan={4}><div className="v2-catalog-empty"><Tags size={28} /><strong>暂无商品</strong><button className="v2-button primary" type="button" onClick={() => openCatalogCreate("products")} disabled={mutationDisabled}><Plus size={16} /> 新增商品</button></div></td></tr>}
                  {products.map((product) => {
                    const forbiddenTokens = parseForbiddenSerialTokens(product.serial_forbidden_chars);
                    return (
                      <tr key={product.sku_id}>
                        <td><strong className="v2-mono">{product.code}</strong></td>
                        <td>{product.name}</td>
                        <td>{product.serial_prefix ? <code className="v2-rule-token">{product.serial_prefix}</code> : <span className="v2-muted-value">不限</span>}</td>
                        <td>{forbiddenTokens.length > 0 ? <div className="v2-token-list">{forbiddenTokens.map((token, index) => <code className="v2-rule-token danger" key={`${token}-${index}`}>{token === " " ? "空格" : token}</code>)}</div> : <span className="v2-muted-value">无</span>}</td>
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
              <div><span>{activeTabLabel}</span><h3 id="v2-catalog-create-title">{catalogTab === "parties" && editingPartyId ? "编辑往来方" : createButtonLabel}</h3></div>
              <button className="v2-icon-button" type="button" onClick={closeCatalogCreate} disabled={catalogLoading} aria-label="关闭新增窗口" title="关闭"><X size={17} /></button>
            </header>
            {catalogTab === "products" ? <form className="v2-form v2-catalog-modal-form" onSubmit={submitCatalogProduct}>
              <div className="v2-form-grid">
                <label><span>商品编码 *</span><input value={newProductCode} onChange={(event) => setNewProductCode(event.target.value)} placeholder="例如 DDR4-32G-3200" autoComplete="off" autoFocus required disabled={catalogLoading || mutationDisabled} /></label>
                <label><span>商品名称 *</span><input value={newProductName} onChange={(event) => setNewProductName(event.target.value)} placeholder="例如 32G 3200 内存" autoComplete="off" required disabled={catalogLoading || mutationDisabled} /></label>
                <label><span>SN 前缀</span><input value={newProductSerialPrefix} onChange={(event) => setNewProductSerialPrefix(event.target.value)} placeholder="可选，例如 RAM" autoComplete="off" disabled={catalogLoading || mutationDisabled} /></label>
                <label><span>SN 禁用字符或片段</span><input value={newProductForbiddenChars} onChange={(event) => setNewProductForbiddenChars(event.target.value)} placeholder="使用英文逗号分隔" autoComplete="off" disabled={catalogLoading || mutationDisabled} /><small>英文逗号分隔；默认禁止连字符和空格。</small></label>
              </div>
              {catalogNotice?.type === "error" && <div className="v2-notice error">{catalogNotice.text}</div>}
              <div className="v2-form-actions"><button className="v2-button" type="button" onClick={closeCatalogCreate} disabled={catalogLoading}>取消</button><button className="v2-button primary" type="submit" disabled={catalogLoading || mutationDisabled}><Plus size={16} /> 保存商品</button></div>
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
    const stepOneState = receiptDetailsReady ? "complete" : "active";
    const stepTwoState = barcodes.length > 0 ? "complete" : receiptDetailsReady ? "active" : "pending";
    const stepThreeState = receiptReady ? "active" : "pending";
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
            <li className={stepThreeState}><span>3</span><div><strong>确认入库</strong><small>{receiptReady ? "可提交" : "待完成"}</small></div></li>
          </ol>

          <section className="v2-receipt-details-step" aria-labelledby="v2-receipt-details-title">
            <div className="v2-receipt-section-heading"><span>1</span><div><h3 id="v2-receipt-details-title">选择资料</h3><small>{receiptDetailsReady ? "资料已完整" : "完成必填项"}</small></div></div>
            {!catalogLoading && catalog && missingCatalogEntries.length > 0 && <div className="v2-notice warning v2-receipt-prerequisite" role="alert"><span>缺少基础资料：{missingCatalogEntries.join("、")}。请先新增后再扫码。</span><button className="v2-button" type="button" onClick={() => openCatalogCreateFromReceipt(firstMissingCatalogTab)}><Plus size={16} /> 新增{missingCatalogEntries[0]}</button></div>}
            <div className="v2-form-grid">
            <label><span>商品 *</span><select value={selectedProductId} onChange={(event) => { setSelectedProductId(event.target.value); setScannerInput(""); setReceiptNotice(null); }} required disabled={catalogLoading || scanChecking || productLocked || products.length === 0}>
              {products.length === 0 && <option value="">{catalogLoading ? "正在读取商品…" : "没有可用商品"}</option>}
              {products.map((product) => <option key={product.sku_id} value={product.sku_id}>{product.code} · {product.name}</option>)}
            </select>{productLocked && <small>当前批次已有 SN，商品已锁定。</small>}</label>
            <label><span>供应商 *</span><select value={supplierName} onChange={(event) => setSupplierName(event.target.value)} required disabled={catalogLoading || suppliers.length === 0}>
              {suppliers.length === 0 && <option value="">{catalogLoading ? "正在读取供应商…" : "没有可用供应商"}</option>}
              {suppliers.map((party) => <option key={party.party_id} value={party.display_name}>{party.display_name}</option>)}
            </select></label>
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
            </div>
          </section>

          <section className={`v2-scanner-section ${receiptDetailsReady ? "" : "locked"}`} aria-labelledby="v2-scanner-title" aria-disabled={!receiptDetailsReady}>
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
          </section>

          <section className="v2-receipt-confirm-step" aria-labelledby="v2-receipt-confirm-title">
            <div className="v2-receipt-section-heading"><span>3</span><div><h3 id="v2-receipt-confirm-title">确认入库</h3><small>{receiptReady ? "核对后提交" : "完成前两步后提交"}</small></div></div>
            <div className="v2-receipt-confirm-summary">
              <span><small>商品</small><strong>{selectedProduct?.code ?? "—"}</strong></span>
              <span><small>供应商</small><strong>{supplierName || "—"}</strong></span>
              <span><small>数量</small><strong>{barcodes.length} 件</strong></span>
            </div>
            <div className="v2-receipt-confirm-actions"><button className="v2-button primary v2-receipt-submit" type="submit" disabled={receiptLoading || scanChecking || mutationDisabled || !receiptReady}>{receiptLoading ? "正在原子入库…" : `确认入库 ${barcodes.length} 件`}</button></div>
          </section>
        </form>
      </section>
    );
  }

  function renderQuality() {
    const mutationDisabled = mode === "offline" && !offlineActivated;
    return (
      <section className="v2-page" aria-labelledby="v2-quality-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">质量作业</span><h2 id="v2-quality-title">扫码质检</h2><p>设置本批结果后，用扫码枪逐件采集待检 SN。</p></div>
          <button className="v2-button" type="button" onClick={() => void refreshQualityItems()} disabled={qualityLoading || qualityScanChecking}><RefreshCw size={16} className={qualityLoading ? "v2-spin" : ""} /> 刷新</button>
        </div>
        <form className="v2-quality-layout" onSubmit={submitInspection}>
          <div className="v2-panel v2-quality-scanner-panel">
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
              <span><strong>本批结果</strong>{inspectionOutcome === "passed" ? "合格" : "不合格"}</span>
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
                        <span className={`v2-badge quality-${item.quality_status}`}>{qualityStatusLabels[item.quality_status]}</span>
                      </label>
                    ))}
                  </div>
                </div>
              </div>
            </details>
          </div>
          <aside className="v2-panel v2-inspection-form">
            <div className="v2-section-heading compact"><div><h3>本批质检结果</h3><small>应用到本批所有 SN</small></div><ClipboardCheck size={20} /></div>
            <label><span>结果 *</span><select value={inspectionOutcome} onChange={(event) => setInspectionOutcome(event.target.value as QualityOutcome)} disabled={qualityLoading || qualityScanChecking || mutationDisabled}><option value="passed">合格</option><option value="failed">不合格</option></select></label>
            <label><span>缺陷代码</span><input value={defectCode} onChange={(event) => setDefectCode(event.target.value)} placeholder="不合格时建议填写" disabled={qualityLoading || qualityScanChecking || mutationDisabled} /></label>
            <label><span>质检备注</span><textarea value={inspectionNotes} onChange={(event) => setInspectionNotes(event.target.value)} placeholder="记录现象、测试方法或复检说明" disabled={qualityLoading || qualityScanChecking || mutationDisabled} /></label>
            <div className="v2-rule-hint"><ShieldAlert size={17} /><span>不合格单件会进入隔离区；只有合格或经授权放行的单件可参与出库分配。</span></div>
            <button className="v2-button primary wide" type="submit" disabled={qualityLoading || qualityScanChecking || selectedBarcodes.size === 0 || mutationDisabled}>{qualityLoading ? "正在提交…" : `确认质检 ${selectedBarcodes.size} 件`}</button>
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
                    <td><span className={`v2-badge quality-${item.quality_status}`}>{qualityStatusLabels[item.quality_status]}</span></td>
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
                <div><dt>质检状态</dt><dd>{qualityStatusLabels[inventoryTrace.quality_status]}</dd></div>
              </dl>
              <section className="v2-trace-section"><h4>质检记录</h4>{inventoryTrace.inspections.length === 0 ? <p>尚无质检记录</p> : <div className="v2-trace-list">{inventoryTrace.inspections.map((inspection) => <article key={`${inspection.inspection_no}-${inspection.inspected_at}`}><strong>{inspection.inspection_no} · {inspection.inspection_type === "initial" ? "初检" : "复检"}</strong><span>{inspection.result === "passed" ? "合格" : "不合格"} · {formatDateTime(inspection.inspected_at)}</span>{inspection.defect_code && <small>缺陷：{inspection.defect_code}</small>}{inspection.notes && <small>{inspection.notes}</small>}</article>)}</div>}</section>
              <section className="v2-trace-section"><h4>凑单、出库与交货</h4>{inventoryTrace.outbound.length === 0 ? <p>尚未参与上游订单</p> : <div className="v2-trace-list">{inventoryTrace.outbound.map((event) => <article key={event.allocation_id}><strong>{event.order_no} · {event.upstream_receiver_name}</strong><span>分配：{formatDateTime(event.allocated_at)}（{event.allocation_status}）</span>{event.shipment_no && <span>出库：{event.shipment_no} · {formatDateTime(event.shipped_at ?? "")}</span>}{event.confirmation_code && <span>交货确认：{event.confirmation_code} · {formatDateTime(event.confirmed_at ?? "")}</span>}{event.return_no && <span>退回：{event.return_no} · {formatDateTime(event.returned_at ?? "")}</span>}{event.return_reason && <small>{event.return_reason}（{event.return_disposition}）</small>}</article>)}</div>}</section>
            </div>}
          </section>
        </div>}
      </section>
    );
  }

  function renderOutbound() {
    const mutationDisabled = mode === "offline" && !offlineActivated;
    const orderStepState = outboundOrder ? "complete" : "active";
    const allocationStepState = outboundAllocation ? "complete" : outboundOrder ? "active" : "pending";
    const scanStepState = outboundShipment ? "complete" : outboundAllocation ? "active" : "pending";
    const deliveryStepState = outboundResolved ? "complete" : outboundShipment ? "active" : "pending";
    return (
      <section className="v2-page" aria-labelledby="v2-outbound-title">
        <div className="v2-page-heading">
          <div><span className="v2-eyebrow">出库作业</span><h2 id="v2-outbound-title">扫码出库</h2><p>系统先分配合格库存，作业员再用扫码枪逐件核对实物。</p></div>
        </div>
        <div className="v2-panel v2-outbound-workbench">
          <ol className="v2-workflow-progress v2-workflow-progress-four" aria-label="出库进度">
            <li className={orderStepState}><span>1</span><div><strong>建立需求</strong><small>{outboundOrder?.order_no ?? "待建单"}</small></div></li>
            <li className={allocationStepState}><span>2</span><div><strong>分配库存</strong><small>{outboundAllocation ? `${outboundAllocation.allocated_count} 件` : "待分配"}</small></div></li>
            <li className={scanStepState}><span>3</span><div><strong>扫码核对</strong><small>{outboundAllocation ? `${outboundScannedBarcodes.length} / ${outboundAllocatedBarcodes.length}` : "待扫码"}</small></div></li>
            <li className={deliveryStepState}><span>4</span><div><strong>交货处理</strong><small>{outboundShipment?.shipment_no ?? "待出库"}</small></div></li>
          </ol>

          <form className="v2-outbound-step" onSubmit={createOutboundOrder}>
            <div className="v2-receipt-section-heading"><span>1</span><div><h3>建立上游需求</h3><small>填写本次订单条件</small></div></div>
            {!outboundOrder ? <>
              <div className="v2-form-grid">
                <label><span>上游收货方 *</span><input value={outboundReceiver} onChange={(event) => setOutboundReceiver(event.target.value)} placeholder="例如：上游客户 A" disabled={outboundLoading || mutationDisabled} /></label>
                <label><span>订单号 *</span><input value={outboundOrderNo} onChange={(event) => setOutboundOrderNo(event.target.value)} placeholder="例如：SO-20260803-01" disabled={outboundLoading || mutationDisabled} /></label>
                <label><span>型号编码 *</span><input value={outboundSkuCode} onChange={(event) => setOutboundSkuCode(event.target.value)} placeholder="例如：DDR4-32G-3200" disabled={outboundLoading || mutationDisabled} /></label>
                <label><span>型号名称 *</span><input value={outboundSkuName} onChange={(event) => setOutboundSkuName(event.target.value)} placeholder="例如：32G 3200 内存" disabled={outboundLoading || mutationDisabled} /></label>
                <label><span>需求数量 *</span><input type="number" min="1" step="1" value={outboundQuantity} onChange={(event) => setOutboundQuantity(event.target.value)} disabled={outboundLoading || mutationDisabled} /></label>
              </div>
              <div className="v2-form-actions"><button className="v2-button primary" type="submit" disabled={outboundLoading || mutationDisabled}>{outboundLoading ? "处理中…" : "创建出库订单"}</button></div>
            </> : <div className="v2-step-summary"><span><small>订单</small><strong>{outboundOrder.order_no}</strong></span><span><small>收货方</small><strong>{outboundReceiver}</strong></span><span><small>商品</small><strong>{outboundSkuCode}</strong></span><span><small>数量</small><strong>{outboundOrder.required_quantity} 件</strong></span></div>}
          </form>

          <section className={`v2-outbound-step ${!outboundOrder ? "locked" : ""}`} aria-disabled={!outboundOrder}>
            <div className="v2-receipt-section-heading"><span>2</span><div><h3>分配合格库存</h3><small>{outboundOrder ? `${outboundOrder.order_no} · 需求 ${outboundOrder.required_quantity} 件` : "完成建单后可用"}</small></div></div>
            {outboundOrder && !outboundAllocation && <>
              <p className="v2-outbound-meta">默认按入库时间自动分配合格库存，减少人工选货。</p>
              <button className="v2-button primary" type="button" onClick={() => void allocateOutboundOrder()} disabled={outboundLoading || mutationDisabled}>{outboundLoading ? "处理中…" : "按 FIFO 分配库存"}</button>
              <details className="v2-alternative-entry">
                <summary><span>备用分配</span><small>需要指定 SN 时使用</small><ChevronDown size={16} /></summary>
                <div className="v2-alternative-content">
                  <label><span>指定 SN（每行一个）</span><textarea value={outboundBarcodes} onChange={(event) => setOutboundBarcodes(event.target.value)} placeholder={"SN0001\nSN0002"} disabled={outboundLoading || mutationDisabled} /></label>
                  <button className="v2-button" type="button" onClick={() => void allocateOutboundOrder()} disabled={!outboundBarcodes.trim() || outboundLoading || mutationDisabled}>按指定 SN 分配</button>
                </div>
              </details>
            </>}
            {outboundAllocation && <div className="v2-inline-success"><CheckCircle2 size={18} /><span>已锁定 {outboundAllocation.allocated_count} 件合格库存。</span></div>}
          </section>

          <section className={`v2-outbound-step v2-outbound-scan-step ${!outboundAllocation ? "locked" : ""}`} aria-disabled={!outboundAllocation}>
            <div className="v2-scanner-heading">
              <div className="v2-receipt-section-heading"><span>3</span><div><h3>扫码核对实物</h3><small>必须逐件扫齐</small></div></div>
              <strong>{outboundScannedBarcodes.length}<small>/ {outboundAllocatedBarcodes.length} 件</small></strong>
            </div>
            {!outboundAllocation && <p className="v2-outbound-meta">完成上一步分配后，这里会打开扫码核对。</p>}
            {outboundAllocation && !outboundShipment && <>
            <label className="v2-scan-field">
              <span>扫码枪输入 *</span>
              <div className="v2-scanner-control">
                <Bell size={21} aria-hidden="true" />
                <input ref={outboundScannerInputRef} value={outboundScannerInput} onChange={(event) => setOutboundScannerInput(event.target.value)} onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    void addOutboundScannedBarcode();
                  }
                }} placeholder="请扫描待出库 SN（扫码枪自动回车）" autoComplete="off" autoCapitalize="characters" spellCheck={false} disabled={!outboundAllocation || Boolean(outboundShipment) || outboundScanChecking || outboundLoading || mutationDisabled} />
                <button className="v2-button" type="button" onClick={() => void addOutboundScannedBarcode()} disabled={!outboundScannerInput.trim() || !outboundAllocation || Boolean(outboundShipment) || outboundScanChecking || outboundLoading || mutationDisabled}>{outboundScanChecking ? "正在核对…" : "手动加入"}</button>
              </div>
              <small>重复 SN、其他订单 SN 或未分配 SN 会立即报警，并且不会进入出库批次。</small>
            </label>

            {outboundScanNotice && <div className={`v2-scanner-feedback ${outboundScanNotice.type}`} role={outboundScanNotice.type === "error" ? "alert" : "status"} aria-live={outboundScanNotice.type === "error" ? "assertive" : "polite"}>{outboundScanNotice.type === "error" ? <Bell size={19} /> : <CheckCircle2 size={19} />}<span>{outboundScanNotice.text}</span></div>}

            <div className="v2-scanned-heading">
              <div><strong>分配核对清单</strong><span>{outboundScanComplete ? "已全部扫齐" : `还差 ${Math.max(0, outboundAllocatedBarcodes.length - outboundScannedBarcodes.length)} 件`}</span></div>
              <button className="v2-button" type="button" onClick={() => void clearOutboundScanBatch()} disabled={outboundScanChecking || outboundLoading || Boolean(outboundShipment) || outboundScannedBarcodes.length === 0}>清空扫码</button>
            </div>
            <div className="v2-outbound-checklist" aria-label="出库分配核对清单">
              {!outboundAllocation && <div className="v2-scanned-empty"><Truck size={26} /><span>等待分配库存</span></div>}
              {outboundAllocation?.allocations.map((item, index) => {
                const scanned = outboundScannedBarcodes.includes(item.barcode.toUpperCase());
                return <div className={scanned ? "scanned" : ""} key={item.allocation_id}>
                  <span>{scanned ? <CheckCircle2 size={17} /> : String(index + 1).padStart(2, "0")}</span>
                  <strong>{item.barcode}</strong>
                  <small>{scanned ? "已核对" : "待扫描"}</small>
                  {scanned && !outboundShipment && <button className="v2-icon-button danger" type="button" onClick={() => removeOutboundScannedBarcode(item.barcode.toUpperCase())} disabled={outboundScanChecking || outboundLoading} aria-label={`移除出库 SN ${item.barcode}`} title="重新核对"><X size={16} /></button>}
                </div>;
              })}
            </div>

            <details className="v2-alternative-entry">
              <summary><span>备用录入</span><small>批量粘贴已分配 SN</small><ChevronDown size={16} /></summary>
              <div className="v2-alternative-content">
                <label><span>每行一个 SN</span><textarea value={outboundBulkInput} onChange={(event) => setOutboundBulkInput(event.target.value)} placeholder={"SN0001\nSN0002"} disabled={!outboundAllocation || Boolean(outboundShipment) || outboundScanChecking || outboundLoading || mutationDisabled} /></label>
                <button className="v2-button" type="button" onClick={() => void importOutboundBarcodes()} disabled={!outboundBulkInput.trim() || !outboundAllocation || Boolean(outboundShipment) || outboundScanChecking || outboundLoading || mutationDisabled}>核对并加入批次</button>
              </div>
            </details>

            <div className="v2-outbound-submit-row">
              <label><span>出库批次号</span><input value={outboundShipmentNo} onChange={(event) => setOutboundShipmentNo(event.target.value)} placeholder="留空自动生成" disabled={!outboundAllocation || Boolean(outboundShipment) || outboundScanChecking || outboundLoading || mutationDisabled} /></label>
              <button className="v2-button primary" type="button" onClick={() => void shipOutboundOrder()} disabled={!outboundScanComplete || Boolean(outboundShipment) || outboundScanChecking || outboundLoading || mutationDisabled}>{outboundLoading ? "正在原子出库…" : `确认出库 ${outboundScannedBarcodes.length} 件`}</button>
            </div>
            </>}
            {outboundShipment && <div className="v2-inline-success"><CheckCircle2 size={18} /><span>{outboundShipment.shipment_no} 已按扫码记录出库 {outboundShipment.shipped_count} 件。</span></div>}
          </section>

          <section className={`v2-outbound-step ${!outboundShipment ? "locked" : ""}`} aria-disabled={!outboundShipment}>
            <div className="v2-receipt-section-heading"><span>4</span><div><h3>交货确认或退回</h3><small>{outboundShipment ? outboundShipment.shipment_no : "完成出库后可用"}</small></div></div>
            {!outboundShipment && <p className="v2-outbound-meta">完成扫码出库后，这里会打开交货和退回处理。</p>}
            {outboundShipment && <div className="v2-delivery-actions">
              <div>
                <label><span>上游确认码</span><input value={outboundConfirmationCode} onChange={(event) => setOutboundConfirmationCode(event.target.value)} placeholder="签收单号 / 确认码" disabled={!outboundShipment || outboundResolved || outboundLoading || mutationDisabled} /></label>
                <button className="v2-button primary" type="button" onClick={() => void confirmOutboundDelivery()} disabled={!outboundShipment || outboundResolved || outboundLoading || mutationDisabled}>确认已交货</button>
              </div>
              <div>
                <label><span>退回原因</span><textarea value={outboundReturnReason} onChange={(event) => setOutboundReturnReason(event.target.value)} placeholder="退回后进入隔离区，复检通过前不可再次出库" disabled={!outboundShipment || outboundResolved || outboundLoading || mutationDisabled} /></label>
                <button className="v2-button" type="button" onClick={() => void returnOutboundShipment()} disabled={!outboundShipment || outboundResolved || outboundLoading || mutationDisabled}>登记退回并隔离</button>
              </div>
            </div>}
            {outboundResolved && <button className="v2-button primary" type="button" onClick={startNextOutboundOrder}>开始下一单</button>}
          </section>
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
        <div className="v2-settings-grid">
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
          {page === "outbound" && renderOutbound()}
          {page === "legacy-import" && <LegacyImportPanel actorId={resolvedActorId} activated={offlineActivated} onBusyChange={handleChildPanelBusyChange} onCommitted={() => void refreshDashboard()} />}
          {page === "users" && <IdentityAdminPanel currentUserId={networkStatus?.user_id ?? null} onBusyChange={handleChildPanelBusyChange} />}
          {page === "settings" && renderSettings()}
        </>}
      </main>
    </div>
  );
}
