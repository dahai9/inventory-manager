import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open, save, message } from "@tauri-apps/plugin-dialog";
import {
  Archive,
  Bell,
  Boxes,
  Check,
  ClipboardList,
  FileOutput,
  FilePlus2,
  FolderOpen,
  PackageCheck,
  PackagePlus,
  PanelTopOpen,
  RotateCcw,
  Search,
  Settings,
  Trash2,
  Upload,
} from "lucide-react";
import "./App.css";

type View = "home" | "shipment_setup" | "recording";
type Mode = "shipment" | "return";
type RecipientListColumn = "shipment_barcode" | "customer" | "return_barcode";

const CUSTOMER_STATEMENT_SUFFIX = "出退货清单";

const recipientListColumns: { value: RecipientListColumn; label: string }[] = [
  { value: "shipment_barcode", label: "出货条码" },
  { value: "customer", label: "客户名称" },
  { value: "return_barcode", label: "退货条码和时间" },
];

interface CustomerStat {
  name: string;
  shipment_count: number;
  return_count: number;
  delivered_count: number;
}

interface ReturnTimeStat {
  return_time: string;
  customer: string;
  return_count: number;
}

interface Summary {
  total_shipments: number;
  total_returns: number;
  total_delivered: number;
  return_time_stats: ReturnTimeStat[];
  customer_stats: CustomerStat[];
}

interface ReturnLookupResult {
  barcode: string;
  customer: string;
  is_returned: boolean;
  return_time: string | null;
}

function getLocalDateValue(date = new Date()) {
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

function getPathBasename(path: string | null) {
  if (!path) return "未打开表格";
  const separatorIndex = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return separatorIndex >= 0 ? path.slice(separatorIndex + 1) : path;
}

function App() {
  const [view, setView] = useState<View>("home");
  const [mode, setMode] = useState<Mode>("shipment");
  const [customer, setCustomer] = useState("");
  const [barcode, setBarcode] = useState("");
  const [batchBarcodes, setBatchBarcodes] = useState<string[]>([]);
  const [returnOwnerNotice, setReturnOwnerNotice] = useState<{ barcode: string; customer: string } | null>(null);
  const [log, setLog] = useState<{msg: string, type: 'success' | 'error'}[]>([]);
  const [summary, setSummary] = useState<Summary>({ total_shipments: 0, total_returns: 0, total_delivered: 0, return_time_stats: [], customer_stats: [] });
  const [selectedCustomerNames, setSelectedCustomerNames] = useState<string[]>([]);
  const [statementUnitPrice, setStatementUnitPrice] = useState("0");
  
  const [importPath, setImportPath] = useState<string | null>(null);
  const [columns, setColumns] = useState<string[]>([]);
  const [shipCol, setShipCol] = useState("");
  const [retCol, setRetCol] = useState("");
  const [retTimeCol, setRetTimeCol] = useState("");
  const [custCol, setCustCol] = useState("");
  const [returnTime, setReturnTime] = useState(getLocalDateValue);
  const [showNewTableDialog, setShowNewTableDialog] = useState(false);
  const [newTableName, setNewTableName] = useState("");
  const [showImportDialog, setShowImportDialog] = useState(false);
  const [showReturnLookupDialog, setShowReturnLookupDialog] = useState(false);
  const [returnLookupBarcode, setReturnLookupBarcode] = useState("");
  const [returnLookupResult, setReturnLookupResult] = useState<ReturnLookupResult | null>(null);
  const [showRecipientListDialog, setShowRecipientListDialog] = useState(false);
  const [recipientListColumn, setRecipientListColumn] = useState<RecipientListColumn>("shipment_barcode");
  const [showSettings, setShowSettings] = useState(false);
  const [ignoreChars, setIgnoreChars] = useState<string>(() => {
    return localStorage.getItem("ignoreChars") || "-, ";
  });

  const barcodeRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    localStorage.setItem("ignoreChars", ignoreChars);
  }, [ignoreChars]);

  async function playAlertSound() {
    try {
      await invoke("play_beep");
    } catch (e) {
      console.error("Failed to play alert sound via backend", e);
    }
  }

  useEffect(() => {
    updateSummary();
    
    const handleKeyDown = (e: KeyboardEvent) => {
      // 捕获 Ctrl+S 或 Cmd+S，不区分大小写
      if ((e.ctrlKey || e.metaKey) && (e.key === 's' || e.key === 'S')) {
        e.preventDefault();
        handleQuickSave();
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [importPath]); // Re-bind when importPath changes to ensure handleQuickSave uses the latest

  async function handleQuickSave() {
    if (importPath) {
      try {
        await invoke("export_data", { path: importPath });
        addLog("自动保存成功: " + importPath, "success");
        updateSummary();
      } catch (err) {
        addLog("自动保存失败: " + err, "error");
      }
    } else {
      handleExport();
    }
  }

  useEffect(() => {
    if (view === "recording") {
      barcodeRef.current?.focus();
    }
  }, [view]);

  useEffect(() => {
    setSelectedCustomerNames(prev =>
      prev.filter(name => summary.customer_stats.some(stat => stat.name === name))
    );
  }, [summary.customer_stats]);

  async function updateSummary() {
    const s = await invoke<Summary>("get_summary");
    setSummary(s);
  }

  function addLog(msg: string, type: "success" | "error") {
    setLog((prev) => [{ msg: `[${new Date().toLocaleTimeString()}] ${msg}`, type }, ...prev].slice(0, 50));
  }

  async function handleScan(e: React.FormEvent) {
    e.preventDefault();
    if (!barcode) return;

    // 校验是否包含忽略字符
    const charsToIgnore = ignoreChars.split(",").map(c => c === " " ? " " : c.trim()).filter(c => c !== "");
    const shouldIgnore = charsToIgnore.some(c => barcode.includes(c));

    if (shouldIgnore) {
      addLog(`检测到型号/非法字符，已忽略条码: ${barcode}`, "error");
      setReturnOwnerNotice(null);
      await playAlertSound();
      setBarcode("");
      return;
    }

    if (batchBarcodes.includes(barcode)) {
      addLog(`条码 ${barcode} 在当前批次中已存在`, "error");
      setReturnOwnerNotice(null);
      setBarcode("");
      return;
    }

    try {
      if (mode === "shipment") {
        await invoke("check_shipment", { barcode });
        addLog(`已扫描: ${barcode}`, "success");
        setReturnOwnerNotice(null);
      } else {
        const owner = await invoke<string>("check_return", { barcode });
        addLog(`已扫描退货: ${barcode}，客户: ${owner}`, "success");
        setReturnOwnerNotice({ barcode, customer: owner });
      }
      setBatchBarcodes(prev => [barcode, ...prev]);
    } catch (err) {
      const errMsg = String(err);
      addLog(errMsg, "error");
      setReturnOwnerNotice(null);
      if (mode === "return" && errMsg.includes("不是我们的货")) {
        await message(errMsg, { title: "扫码错误", kind: "error" });
      }
    } finally {
      setBarcode("");
      barcodeRef.current?.focus();
    }
  }

  async function finishBatch() {
    if (batchBarcodes.length === 0) {
      setReturnOwnerNotice(null);
      setView("home");
      return;
    }

    const confirmed = window.confirm(`确定录入当前批次 (${batchBarcodes.length} 件) 吗？`);
    if (!confirmed) return;

    try {
      if (mode === "return" && !returnTime) {
        addLog("请选择退货时间", "error");
        return;
      }
      const msg = await invoke<string>(
        mode === "shipment" ? "commit_shipment_batch" : "commit_return_batch",
        mode === "shipment" ? { customer, barcodes: batchBarcodes } : { barcodes: batchBarcodes, returnTime }
      );
      addLog(msg, "success");
      setBatchBarcodes([]);
      setReturnOwnerNotice(null);
      updateSummary();
      setView("home");
    } catch (err) {
      addLog("录入失败: " + err, "error");
    }
  }

  function confirmDeleteBatchBarcode(targetBarcode: string, targetIndex: number) {
    const confirmed = window.confirm(`确定从当前批次删除编码 ${targetBarcode} 吗？`);
    if (!confirmed) {
      barcodeRef.current?.focus();
      return;
    }

    setBatchBarcodes(prev => prev.filter((_, index) => index !== targetIndex));
    setReturnOwnerNotice(prev => prev?.barcode === targetBarcode ? null : prev);
    addLog(`已删除当前批次编码: ${targetBarcode}`, "success");
    barcodeRef.current?.focus();
  }

  async function handleImport() {
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: "Excel", extensions: ["xlsx"] }]
      });
      if (selected && typeof selected === "string") {
        const cols = await invoke<string[]>("get_excel_columns", { path: selected });
        setImportPath(selected);
        setColumns(cols);
        
        // 自动识别列
        let autoShip = "";
        let autoRet = "";
        let autoRetTime = "";
        let autoCust = "";

        const shipKeywords = ["出货", "条码", "barcode", "shipment", "sn", "序列号"];
        const retKeywords = ["退货", "return", "退回"];
        const retTimeKeywords = ["退货时间", "退货日期", "return time", "return date", "returned at"];
        const custKeywords = ["客户", "姓名", "name", "customer", "client", "收货"];

        for (const col of cols) {
          const lowerCol = col.toLowerCase();
          if (!autoShip && shipKeywords.some(k => lowerCol.includes(k)) && !lowerCol.includes("退货")) {
            autoShip = col;
          }
          if (!autoRet && retKeywords.some(k => lowerCol.includes(k)) && !lowerCol.includes("时间") && !lowerCol.includes("日期")) {
            autoRet = col;
          }
          if (!autoRetTime && (
            retTimeKeywords.some(k => lowerCol.includes(k)) ||
            (lowerCol.includes("退货") && (lowerCol.includes("时间") || lowerCol.includes("日期")))
          )) {
            autoRetTime = col;
          }
          if (!autoCust && custKeywords.some(k => lowerCol.includes(k))) {
            autoCust = col;
          }
        }

        setShipCol(autoShip || cols[0] || "");
        setRetCol(autoRet || (cols.length > 1 ? cols[1] : (cols[0] || "")));
        setRetTimeCol(autoRetTime);
        setCustCol(autoCust || (cols.length > 2 ? cols[2] : (cols[0] || "")));
        
        setShowImportDialog(true);
      }
    } catch (err) {
      addLog("导入失败: " + err, "error");
    }
  }

  async function confirmImport() {
    if (!importPath || !shipCol || !retCol || !custCol) return;
    try {
      await invoke("import_data", {
        path: importPath,
        shipCol,
        returnCol: retCol,
        returnTimeCol: retTimeCol || null,
        customerCol: custCol
      });
      addLog("数据导入成功", "success");
      setShowImportDialog(false);
      updateSummary();
    } catch (err) {
      addLog("导入失败: " + err, "error");
    }
  }

  async function handleExport() {
    try {
      const path = await save({
        filters: [{ name: "Excel", extensions: ["xlsx"] }],
        defaultPath: importPath || "inventory_export.xlsx"
      });
      if (path) {
        await invoke("export_data", { path });
        addLog("导出成功: " + path, "success");
        setImportPath(path); // 更新当前路径，下次 Ctrl+S 即可自动保存
      }
    } catch (err) {
      addLog("导出失败: " + err, "error");
    }
  }

  async function saveCurrentDataBeforeNewTable() {
    const hasUnsavedChanges = await invoke<boolean>("has_unsaved_changes");
    if (!hasUnsavedChanges) return true;

    const confirmed = window.confirm("当前存在尚未保存的数据，必须先保存后才能新建表格。现在保存吗？");
    if (!confirmed) return false;

    try {
      if (importPath) {
        await invoke("export_data", { path: importPath });
        addLog("新建前已保存当前表格: " + importPath, "success");
        return true;
      }

      const path = await save({
        filters: [{ name: "Excel", extensions: ["xlsx"] }],
        defaultPath: "inventory_export.xlsx"
      });
      if (!path) return false;

      await invoke("export_data", { path });
      setImportPath(path);
      addLog("新建前已保存当前表格: " + path, "success");
      return true;
    } catch (err) {
      addLog("新建前保存失败: " + err, "error");
      return false;
    }
  }

  async function handleNewTable() {
    if (batchBarcodes.length > 0) {
      const confirmed = window.confirm(`当前批次还有 ${batchBarcodes.length} 个编码未完成录入。请先完成录入并保存后再新建表格，是否现在完成录入？`);
      if (confirmed) {
        await finishBatch();
      }
      return;
    }

    const defaultName = splitCurrentPath().basename === "inventory_export" ? "" : splitCurrentPath().basename;
    setNewTableName(defaultName);
    setShowNewTableDialog(true);
  }

  async function confirmNewTable() {
    const trimmedName = newTableName.trim();
    if (!trimmedName) {
      addLog("请输入新表格名称", "error");
      return;
    }

    const saved = await saveCurrentDataBeforeNewTable();
    if (!saved) return;

    try {
      const path = await save({
        filters: [{ name: "Excel", extensions: ["xlsx"] }],
        defaultPath: `${sanitizeFilenameSegment(trimmedName)}.xlsx`
      });
      if (!path) return;

      await invoke("create_new_workbook", { path, tableName: trimmedName });
      setImportPath(path);
      setColumns(["出货条码", "客户", "退货条码", "退货时间"]);
      setShipCol("出货条码");
      setCustCol("客户");
      setRetCol("退货条码");
      setRetTimeCol("退货时间");
      setBatchBarcodes([]);
      setSelectedCustomerNames([]);
      setReturnOwnerNotice(null);
      setCustomer("");
      setBarcode("");
      setView("home");
      setShowNewTableDialog(false);
      await updateSummary();
      addLog("已新建表格: " + path, "success");
    } catch (err) {
      addLog("新建表格失败: " + err, "error");
    }
  }

  function getRecipientListDefaultPath() {
    const suffix = recipientListColumn === "return_barcode" ? "退货" : "出货";
    if (!importPath) return `inventory_export${suffix}.xlsx`;

    const separatorIndex = Math.max(importPath.lastIndexOf("/"), importPath.lastIndexOf("\\"));
    const directory = separatorIndex >= 0 ? importPath.slice(0, separatorIndex + 1) : "";
    const filename = separatorIndex >= 0 ? importPath.slice(separatorIndex + 1) : importPath;
    const extensionIndex = filename.toLowerCase().endsWith(".xlsx") ? filename.length - 5 : filename.lastIndexOf(".");

    if (extensionIndex > 0) {
      const basename = filename.slice(0, extensionIndex);
      const extension = filename.slice(extensionIndex);
      return `${directory}${basename.endsWith(suffix) ? basename : `${basename}${suffix}`}${extension}`;
    }

    return `${directory}${filename.endsWith(suffix) ? filename : `${filename}${suffix}`}.xlsx`;
  }

  async function confirmRecipientListExport() {
    try {
      const path = await save({
        filters: [{ name: "Excel", extensions: ["xlsx"] }],
        defaultPath: getRecipientListDefaultPath()
      });
      if (path) {
        await invoke("export_recipient_list", { path, column: recipientListColumn });
        addLog("收货清单导出成功: " + path, "success");
        setShowRecipientListDialog(false);
      }
    } catch (err) {
      addLog("收货清单导出失败: " + err, "error");
    }
  }

  function openReturnLookupDialog() {
    setReturnLookupBarcode("");
    setReturnLookupResult(null);
    setShowReturnLookupDialog(true);
  }

  async function handleReturnLookup(e?: React.FormEvent) {
    e?.preventDefault();
    const lookupBarcode = returnLookupBarcode.trim();
    if (!lookupBarcode) {
      addLog("请输入要查找的退货编码", "error");
      return;
    }

    try {
      const result = await invoke<ReturnLookupResult>("lookup_return", { barcode: lookupBarcode });
      setReturnLookupResult(result);
      addLog(
        result.is_returned
          ? `${lookupBarcode} 已退货，客户: ${result.customer}${result.return_time ? `，退货时间: ${result.return_time}` : ""}`
          : `${lookupBarcode} 未退货，客户: ${result.customer}`,
        "success"
      );
    } catch (err) {
      setReturnLookupResult(null);
      addLog("退货查找失败: " + err, "error");
    } finally {
      setReturnLookupBarcode("");
    }
  }

  function splitCurrentPath() {
    if (!importPath) {
      return { directory: "", basename: "inventory_export" };
    }

    const separatorIndex = Math.max(importPath.lastIndexOf("/"), importPath.lastIndexOf("\\"));
    const directory = separatorIndex >= 0 ? importPath.slice(0, separatorIndex + 1) : "";
    const filename = separatorIndex >= 0 ? importPath.slice(separatorIndex + 1) : importPath;
    const extensionIndex = filename.toLowerCase().endsWith(".xlsx") ? filename.length - 5 : filename.lastIndexOf(".");
    const basename = extensionIndex > 0 ? filename.slice(0, extensionIndex) : filename;

    return { directory, basename: basename || "inventory_export" };
  }

  function sanitizeFilenameSegment(segment: string) {
    const sanitized = segment.replace(/[\\/:*?"<>|\x00-\x1F]/g, "_").trim().replace(/^\.+|\.+$/g, "");
    return sanitized || "未命名";
  }

  function getStatementDefaultPath(customerName: string) {
    const { directory, basename } = splitCurrentPath();
    return `${directory}${sanitizeFilenameSegment(basename)}_${sanitizeFilenameSegment(customerName)}_${CUSTOMER_STATEMENT_SUFFIX}.xlsx`;
  }

  function getTotalQuantityTableDefaultPath() {
    const { directory, basename } = splitCurrentPath();
    return `${directory}${sanitizeFilenameSegment(basename)}_总出退货数量表.xlsx`;
  }

  function getStatementUnitPrice() {
    const unitPrice = Number(statementUnitPrice);
    if (!Number.isFinite(unitPrice) || unitPrice < 0) {
      throw new Error("请输入有效的单价");
    }
    return unitPrice;
  }

  function toggleCustomerSelection(customerName: string, checked: boolean) {
    setSelectedCustomerNames(prev => {
      if (checked) {
        return prev.includes(customerName) ? prev : [...prev, customerName];
      }
      return prev.filter(name => name !== customerName);
    });
  }

  function toggleAllCustomers(checked: boolean) {
    setSelectedCustomerNames(checked ? summary.customer_stats.map(stat => stat.name) : []);
  }

  async function exportCustomerStatement(customerName: string) {
    try {
      const unitPrice = getStatementUnitPrice();
      const path = await save({
        filters: [{ name: "Excel", extensions: ["xlsx"] }],
        defaultPath: getStatementDefaultPath(customerName)
      });
      if (path) {
        await invoke("export_customer_statement", { path, customer: customerName, unitPrice });
        addLog(`出退货清单导出成功: ${path}`, "success");
      }
    } catch (err) {
      addLog("出退货清单导出失败: " + err, "error");
    }
  }

  async function exportSelectedCustomerStatements() {
    if (selectedCustomerNames.length === 0) {
      addLog("请先选择要导出的客户", "error");
      return;
    }

    if (selectedCustomerNames.length === 1) {
      await exportCustomerStatement(selectedCustomerNames[0]);
      return;
    }

    try {
      const unitPrice = getStatementUnitPrice();
      const { directory, basename } = splitCurrentPath();
      const selected = await open({
        directory: true,
        multiple: false,
        defaultPath: directory || undefined
      });
      if (selected && typeof selected === "string") {
        const paths = await invoke<string[]>("export_customer_statements_to_dir", {
          directory: selected,
          baseName: basename,
          customers: selectedCustomerNames,
          unitPrice
        });
        addLog(`已导出 ${paths.length} 份出退货清单到: ${selected}`, "success");
      }
    } catch (err) {
      addLog("批量导出出退货清单失败: " + err, "error");
    }
  }

  async function exportTotalQuantityTable() {
    try {
      const path = await save({
        filters: [{ name: "Excel", extensions: ["xlsx"] }],
        defaultPath: getTotalQuantityTableDefaultPath()
      });
      if (path) {
        await invoke("export_total_quantity_table", { path });
        addLog(`总出退货数量表导出成功: ${path}`, "success");
      }
    } catch (err) {
      addLog("总出退货数量表导出失败: " + err, "error");
    }
  }

  const beginReturnRecording = () => {
    setMode("return");
    setReturnTime(getLocalDateValue());
    setReturnOwnerNotice(null);
    setView("recording");
    setBatchBarcodes([]);
  };

  const beginShipmentSetup = () => {
    setMode("shipment");
    setReturnOwnerNotice(null);
    setView("shipment_setup");
  };

  const isRecording = view === "recording";
  const latestLogs = log.slice(0, 8);

  const renderModePanel = () => (
    <section className={`panel workbench-panel ${isRecording ? "is-recording" : ""}`}>
      <div className="panel-heading">
        <div>
          <span className="eyebrow">扫码工作台</span>
          <h2>{isRecording ? (mode === "shipment" ? "出货录入中" : "退货录入中") : "选择录入模式"}</h2>
        </div>
        <div className={`mode-pill ${isRecording ? mode : ""}`}>
          {isRecording ? (mode === "shipment" ? "出货" : "退货") : "待开始"}
        </div>
      </div>

      {!isRecording && (
        <div className="mode-grid">
          <button type="button" className="mode-card" onClick={beginShipmentSetup}>
            <span className="mode-icon ship"><PackagePlus size={22} /></span>
            <span>
              <strong>出货录入</strong>
              <small>选择客户后开始扫码</small>
            </span>
          </button>
          <button type="button" className="mode-card" onClick={beginReturnRecording}>
            <span className="mode-icon return"><RotateCcw size={22} /></span>
            <span>
              <strong>退货录入</strong>
              <small>按退货日期管理批次</small>
            </span>
          </button>
        </div>
      )}

      {view === "shipment_setup" && (
        <div className="task-setup">
          <label className="field">
            <span>客户名称</span>
            <input
              type="text"
              value={customer}
              onChange={(e) => setCustomer(e.target.value)}
              placeholder="例如: 某某贸易有限公司"
              autoFocus
            />
          </label>
          <div className="inline-actions">
            <button
              type="button"
              className="btn primary"
              disabled={!customer}
              onClick={() => { setReturnOwnerNotice(null); setView("recording"); setBatchBarcodes([]); }}
            >
              <PackageCheck size={16} /> 开始扫码
            </button>
            <button type="button" className="btn ghost" onClick={() => setView("home")}>取消</button>
          </div>
        </div>
      )}

      {isRecording && (
        <div className="recording-workspace">
          <div className="recording-status">
            <div className="status-item">
              <span>当前模式</span>
              <strong>{mode === "shipment" ? "出货录入" : "退货录入"}</strong>
            </div>
            {mode === "shipment" ? (
              <div className="status-item wide">
                <span>客户</span>
                <strong>{customer || "未设置客户"}</strong>
              </div>
            ) : (
              <label className="status-item date-field">
                <span>退货时间</span>
                <input
                  type="date"
                  value={returnTime}
                  onChange={(e) => setReturnTime(e.target.value)}
                />
              </label>
            )}
            <div className="status-item count">
              <span>当前批次</span>
              <strong>{batchBarcodes.length} 件</strong>
            </div>
          </div>

          <form onSubmit={handleScan} className="scan-form">
            <div className="scan-input-wrap">
              <Archive size={20} />
              <input
                ref={barcodeRef}
                type="text"
                value={barcode}
                onChange={(e) => setBarcode(e.target.value)}
                placeholder="请扫描或输入条码"
              />
            </div>
            <button type="submit" className="btn primary scan-submit">
              <PanelTopOpen size={17} /> 扫描
            </button>
          </form>

          {mode === "return" && returnOwnerNotice && (
            <div className="owner-notice">
              <span className="owner-notice-label">归属</span>
              <span className="owner-notice-code">{returnOwnerNotice.barcode}</span>
              <span>属于</span>
              <strong>{returnOwnerNotice.customer}</strong>
            </div>
          )}

          <div className="batch-panel">
            <div className="batch-header">
              <h3>当前批次条码</h3>
              <span>{batchBarcodes.length} 条</span>
            </div>
            <div className="batch-list">
              {batchBarcodes.map((bc, i) => (
                <div key={`${bc}-${i}`} className="batch-item">
                  <span className="batch-index">{batchBarcodes.length - i}</span>
                  <span className="batch-code">{bc}</span>
                  <button
                    type="button"
                    className="icon-danger-btn"
                    onClick={() => confirmDeleteBatchBarcode(bc, i)}
                    aria-label={`删除编码 ${bc}`}
                    title="删除"
                  >
                    <Trash2 size={15} />
                  </button>
                </div>
              ))}
              {batchBarcodes.length === 0 && (
                <div className="empty-batch">当前批次还没有条码</div>
              )}
            </div>
          </div>

          <div className="recording-actions">
            <button type="button" className="btn primary finish-btn" onClick={finishBatch}>
              <Check size={17} /> 完成录入
            </button>
            <button type="button" className="btn ghost" onClick={() => { setReturnOwnerNotice(null); setView("home"); }}>
              返回工作台
            </button>
          </div>
        </div>
      )}
    </section>
  );

  const selectedCustomerSet = new Set(selectedCustomerNames);
  const allCustomersSelected = summary.customer_stats.length > 0 &&
    summary.customer_stats.every(stat => selectedCustomerSet.has(stat.name));

  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="brand-block">
          <div className="brand-mark"><Boxes size={22} /></div>
          <div>
            <h1>仓库出退货操作台</h1>
            <p>{getPathBasename(importPath)}</p>
          </div>
        </div>
        <div className="topbar-actions">
          <button type="button" className="btn secondary" onClick={handleNewTable}><FilePlus2 size={16} /> 新建</button>
          <button type="button" className="btn secondary" onClick={handleImport}><FolderOpen size={16} /> 导入</button>
          <button type="button" className="btn secondary" onClick={handleExport}><FileOutput size={16} /> 导出</button>
          <button type="button" className="btn icon-btn" onClick={() => setShowSettings(true)} aria-label="设置" title="设置">
            <Settings size={17} />
          </button>
        </div>
      </header>

      <main className="workspace-grid">
        <section className="metric-grid">
          <div className="metric-card">
            <span>总出货</span>
            <strong>{summary.total_shipments}</strong>
          </div>
          <div className="metric-card returns">
            <span>总退货</span>
            <strong>{summary.total_returns}</strong>
          </div>
          <div className="metric-card delivered">
            <span>成功交货</span>
            <strong>{summary.total_delivered}</strong>
          </div>
        </section>

        <div className="workspace-main">
          {renderModePanel()}

          <section className="panel recent-panel">
            <div className="panel-heading compact">
              <h2>最近操作</h2>
            </div>
            <div className="recent-list">
              {latestLogs.map((entry, i) => (
                <div key={i} className={`recent-entry ${entry.type}`}>
                  <span className="status-dot" />
                  <p>{entry.msg}</p>
                </div>
              ))}
              {latestLogs.length === 0 && <div className="empty-log">暂无操作记录</div>}
            </div>
          </section>

          <section className="panel data-panel">
            <div className="panel-heading table-heading">
              <div>
                <span className="eyebrow">客户数据</span>
                <h2>出退货统计</h2>
              </div>
              <div className="summary-actions">
                <label className="unit-price-field">
                  <span>单价</span>
                  <input
                    type="number"
                    min="0"
                    step="0.01"
                    value={statementUnitPrice}
                    onChange={(e) => setStatementUnitPrice(e.target.value)}
                  />
                </label>
                <button
                  type="button"
                  className="btn secondary"
                  disabled={selectedCustomerNames.length === 0}
                  onClick={exportSelectedCustomerStatements}
                >
                  <FileOutput size={16} /> 导出选中
                </button>
                <button
                  type="button"
                  className="btn secondary"
                  disabled={summary.customer_stats.length === 0}
                  onClick={exportTotalQuantityTable}
                >
                  <ClipboardList size={16} /> 总数量表
                </button>
              </div>
            </div>

            <div className="summary-table-container">
              <table className="summary-table">
                <thead>
                  <tr>
                    <th className="select-col">
                      <input
                        type="checkbox"
                        checked={allCustomersSelected}
                        disabled={summary.customer_stats.length === 0}
                        onChange={(e) => toggleAllCustomers(e.target.checked)}
                        aria-label="选择全部客户"
                      />
                    </th>
                    <th>客户</th>
                    <th className="number-col">出货</th>
                    <th className="number-col">退货</th>
                    <th className="number-col">成功交货</th>
                    <th className="action-col">操作</th>
                  </tr>
                </thead>
                <tbody>
                  {summary.customer_stats.map((stat) => (
                    <tr key={stat.name}>
                      <td className="select-col">
                        <input
                          type="checkbox"
                          checked={selectedCustomerSet.has(stat.name)}
                          onChange={(e) => toggleCustomerSelection(stat.name, e.target.checked)}
                          aria-label={`选择客户 ${stat.name}`}
                        />
                      </td>
                      <td className="customer-cell">{stat.name}</td>
                      <td className="number-col">{stat.shipment_count}</td>
                      <td className="number-col">
                        <span className={stat.return_count > 0 ? "return-badge" : ""}>{stat.return_count}</span>
                      </td>
                      <td className="number-col">{stat.delivered_count}</td>
                      <td className="action-col">
                        <button type="button" className="btn table-btn" onClick={() => exportCustomerStatement(stat.name)}>
                          <FileOutput size={14} /> 导出
                        </button>
                      </td>
                    </tr>
                  ))}
                  {summary.customer_stats.length === 0 && (
                    <tr><td colSpan={6} className="empty-table">暂无数据</td></tr>
                  )}
                </tbody>
              </table>
            </div>
          </section>
        </div>

        <aside className="side-rail">
          <section className="panel quick-tools">
            <div className="panel-heading compact">
              <h2>快捷工具</h2>
            </div>
            <button type="button" className="tool-row" onClick={openReturnLookupDialog}>
              <Search size={17} /> 退货查找
            </button>
            <button type="button" className="tool-row" onClick={() => setShowRecipientListDialog(true)}>
              <FileOutput size={17} /> 导出收货清单
            </button>
            <button type="button" className="tool-row" onClick={playAlertSound}>
              <Bell size={17} /> 测试提示音
            </button>
          </section>
        </aside>
      </main>

      {showImportDialog && (
        <div className="modal-overlay">
          <div className="modal">
            <h2>选择导入列</h2>
            <div className="modal-row">
              <label>出货条码列:</label>
              <select value={shipCol} onChange={(e) => setShipCol(e.target.value)}>
                {columns.map(c => <option key={c} value={c}>{c}</option>)}
              </select>
            </div>
            <div className="modal-row">
              <label>客户名称列:</label>
              <select value={custCol} onChange={(e) => setCustCol(e.target.value)}>
                {columns.map(c => <option key={c} value={c}>{c}</option>)}
              </select>
            </div>
            <div className="modal-row">
              <label>退货条码列:</label>
              <select value={retCol} onChange={(e) => setRetCol(e.target.value)}>
                {columns.map(c => <option key={c} value={c}>{c}</option>)}
              </select>
            </div>
            <div className="modal-row">
              <label>退货时间列:</label>
              <select value={retTimeCol} onChange={(e) => setRetTimeCol(e.target.value)}>
                <option value="">无/旧表未记录</option>
                {columns.map(c => <option key={c} value={c}>{c}</option>)}
              </select>
            </div>
            <div className="modal-buttons">
              <button className="btn primary" onClick={confirmImport}><Upload size={16} /> 确认导入</button>
              <button className="btn ghost" onClick={() => setShowImportDialog(false)}>取消</button>
            </div>
          </div>
        </div>
      )}

      {showNewTableDialog && (
        <div className="modal-overlay">
          <div className="modal">
            <h2>新建表格</h2>
            <div className="modal-row">
              <label>表格名称:</label>
              <input
                type="text"
                value={newTableName}
                onChange={(e) => setNewTableName(e.target.value)}
                placeholder="例如: 6月出退货"
                autoFocus
              />
              <p className="hint">新建前如果有未保存数据，会先要求保存；新表会创建为包含出货条码、客户、退货条码、退货时间表头的 Excel 文件。</p>
            </div>
            <div className="modal-buttons">
              <button className="btn primary" onClick={confirmNewTable}><FilePlus2 size={16} /> 创建表格</button>
              <button className="btn ghost" onClick={() => setShowNewTableDialog(false)}>取消</button>
            </div>
          </div>
        </div>
      )}

      {showRecipientListDialog && (
        <div className="modal-overlay">
          <div className="modal">
            <h2>导出收货清单</h2>
            <div className="modal-row">
              <label>选择导出列:</label>
              <select
                value={recipientListColumn}
                onChange={(e) => setRecipientListColumn(e.target.value as RecipientListColumn)}
              >
                {recipientListColumns.map(c => <option key={c.value} value={c.value}>{c.label}</option>)}
              </select>
              <p className="hint">用于发送给收货人的清单；选择退货条码时会同时导出退货时间。</p>
            </div>
            <div className="modal-buttons">
              <button className="btn primary" onClick={confirmRecipientListExport}><FileOutput size={16} /> 确认导出</button>
              <button className="btn ghost" onClick={() => setShowRecipientListDialog(false)}>取消</button>
            </div>
          </div>
        </div>
      )}

      {showReturnLookupDialog && (
        <div className="modal-overlay">
          <div className="modal">
            <h2>退货查找</h2>
            <form onSubmit={handleReturnLookup} className="lookup-form">
              <div className="modal-row">
                <label>退货编码:</label>
                <input
                  type="text"
                  value={returnLookupBarcode}
                  onChange={(e) => setReturnLookupBarcode(e.target.value)}
                  placeholder="输入或扫描编码"
                  autoFocus
                />
              </div>
              <div className="modal-buttons">
                <button type="submit" className="btn primary"><Search size={16} /> 查找</button>
                <button type="button" className="btn ghost" onClick={() => setShowReturnLookupDialog(false)}>关闭</button>
              </div>
            </form>
            {returnLookupResult && (
              <div className={`lookup-result ${returnLookupResult.is_returned ? "returned" : "active"}`}>
                <div>
                  <span className="lookup-label">编码</span>
                  <strong>{returnLookupResult.barcode}</strong>
                </div>
                <div>
                  <span className="lookup-label">客户</span>
                  <strong>{returnLookupResult.customer}</strong>
                </div>
                <div>
                  <span className="lookup-label">状态</span>
                  <strong>{returnLookupResult.is_returned ? "已退货" : "未退货"}</strong>
                </div>
                {returnLookupResult.is_returned && (
                  <div>
                    <span className="lookup-label">时间</span>
                    <strong>{returnLookupResult.return_time || "未记录"}</strong>
                  </div>
                )}
              </div>
            )}
          </div>
        </div>
      )}

      {showSettings && (
        <div className="modal-overlay">
          <div className="modal">
            <h2>系统设置</h2>
            <div className="modal-row">
              <label>忽略字符 (逗号分隔):</label>
              <input 
                type="text" 
                value={ignoreChars} 
                onChange={(e) => setIgnoreChars(e.target.value)}
                placeholder="例如: -,  "
              />
              <p className="hint">扫码时如果条码包含这些字符，将被视为型号并忽略，同时发出提示音。空格请直接输入（例如：<code>-, </code> 表示忽略减号和空格）。</p>
            </div>
            <div className="modal-buttons">
              <button className="btn secondary" onClick={playAlertSound}><Bell size={16} /> 测试声音</button>
              <button className="btn ghost" onClick={() => setShowSettings(false)}>关闭</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default App;
