import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open, save, message } from "@tauri-apps/plugin-dialog";
import "./App.css";

type View = "home" | "shipment_setup" | "recording";
type Mode = "shipment" | "return";
type RecipientListColumn = "shipment_barcode" | "customer" | "return_barcode";

const CUSTOMER_STATEMENT_SUFFIX = "出退货清单";

const recipientListColumns: { value: RecipientListColumn; label: string }[] = [
  { value: "shipment_barcode", label: "出货条码" },
  { value: "customer", label: "客户名称" },
  { value: "return_barcode", label: "退货条码" },
];

interface CustomerStat {
  name: string;
  shipment_count: number;
  return_count: number;
}

interface Summary {
  total_shipments: number;
  total_returns: number;
  customer_stats: CustomerStat[];
}

function App() {
  const [view, setView] = useState<View>("home");
  const [mode, setMode] = useState<Mode>("shipment");
  const [customer, setCustomer] = useState("");
  const [barcode, setBarcode] = useState("");
  const [batchBarcodes, setBatchBarcodes] = useState<string[]>([]);
  const [returnOwnerNotice, setReturnOwnerNotice] = useState<{ barcode: string; customer: string } | null>(null);
  const [log, setLog] = useState<{msg: string, type: 'success' | 'error'}[]>([]);
  const [summary, setSummary] = useState<Summary>({ total_shipments: 0, total_returns: 0, customer_stats: [] });
  const [selectedCustomerNames, setSelectedCustomerNames] = useState<string[]>([]);
  const [statementUnitPrice, setStatementUnitPrice] = useState("0");
  
  const [importPath, setImportPath] = useState<string | null>(null);
  const [columns, setColumns] = useState<string[]>([]);
  const [shipCol, setShipCol] = useState("");
  const [retCol, setRetCol] = useState("");
  const [custCol, setCustCol] = useState("");
  const [showNewTableDialog, setShowNewTableDialog] = useState(false);
  const [newTableName, setNewTableName] = useState("");
  const [showImportDialog, setShowImportDialog] = useState(false);
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
      const msg = await invoke<string>(
        mode === "shipment" ? "commit_shipment_batch" : "commit_return_batch",
        mode === "shipment" ? { customer, barcodes: batchBarcodes } : { barcodes: batchBarcodes }
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
        let autoCust = "";

        const shipKeywords = ["出货", "条码", "barcode", "shipment", "sn", "序列号"];
        const retKeywords = ["退货", "return", "退回"];
        const custKeywords = ["客户", "姓名", "name", "customer", "client", "收货"];

        for (const col of cols) {
          const lowerCol = col.toLowerCase();
          if (!autoShip && shipKeywords.some(k => lowerCol.includes(k)) && !lowerCol.includes("退货")) {
            autoShip = col;
          }
          if (!autoRet && retKeywords.some(k => lowerCol.includes(k))) {
            autoRet = col;
          }
          if (!autoCust && custKeywords.some(k => lowerCol.includes(k))) {
            autoCust = col;
          }
        }

        setShipCol(autoShip || cols[0] || "");
        setRetCol(autoRet || (cols.length > 1 ? cols[1] : (cols[0] || "")));
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
      await invoke("import_data", { path: importPath, shipCol, returnCol: retCol, customerCol: custCol });
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
      setColumns(["出货条码", "客户", "退货条码"]);
      setShipCol("出货条码");
      setCustCol("客户");
      setRetCol("退货条码");
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
    if (!importPath) return "inventory_export出货.xlsx";

    const separatorIndex = Math.max(importPath.lastIndexOf("/"), importPath.lastIndexOf("\\"));
    const directory = separatorIndex >= 0 ? importPath.slice(0, separatorIndex + 1) : "";
    const filename = separatorIndex >= 0 ? importPath.slice(separatorIndex + 1) : importPath;
    const extensionIndex = filename.toLowerCase().endsWith(".xlsx") ? filename.length - 5 : filename.lastIndexOf(".");

    if (extensionIndex > 0) {
      const basename = filename.slice(0, extensionIndex);
      const extension = filename.slice(extensionIndex);
      return `${directory}${basename.endsWith("出货") ? basename : `${basename}出货`}${extension}`;
    }

    return `${directory}${filename.endsWith("出货") ? filename : `${filename}出货`}.xlsx`;
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

  const renderHome = () => (
    <div className="home-view">
      <div className="main-actions">
        <button className="large-btn ship" onClick={() => { setMode("shipment"); setReturnOwnerNotice(null); setView("shipment_setup"); }}>开始出货录入</button>
        <button className="large-btn return" onClick={() => { setMode("return"); setReturnOwnerNotice(null); setView("recording"); setBatchBarcodes([]); }}>开始退货录入</button>
      </div>
      <div className="secondary-actions">
        <button onClick={handleNewTable}>新建表格</button>
        <button onClick={handleImport}>导入数据</button>
        <button onClick={handleExport}>导出数据</button>
        <button onClick={() => setShowRecipientListDialog(true)}>导出收货清单</button>
        <button className="settings-btn" onClick={() => setShowSettings(true)}>设置</button>
      </div>
    </div>
  );

  const renderShipmentSetup = () => (
    <div className="setup-view">
      <h2>出货设置</h2>
      <div className="input-group">
        <label>输入客户名称:</label>
        <input 
          type="text" 
          value={customer} 
          onChange={(e) => setCustomer(e.target.value)}
          placeholder="例如: 某某贸易有限公司"
          autoFocus
        />
      </div>
      <div className="setup-actions">
        <button disabled={!customer} onClick={() => { setReturnOwnerNotice(null); setView("recording"); setBatchBarcodes([]); }}>开始录入</button>
        <button className="secondary" onClick={() => setView("home")}>取消</button>
      </div>
    </div>
  );

  const renderRecording = () => (
    <div className="recording-view">
      <div className="recording-header">
        <h2>{mode === "shipment" ? `出货录入中 - ${customer}` : "退货录入中"}</h2>
        <div className="batch-stats">当前批次已扫: <span>{batchBarcodes.length}</span> 件</div>
      </div>
      
      <form onSubmit={handleScan} className="barcode-form">
        <input 
          ref={barcodeRef}
          type="text" 
          value={barcode} 
          onChange={(e) => setBarcode(e.target.value)}
          placeholder="请扫描条码..."
        />
        <button type="submit">扫描</button>
      </form>

      {mode === "return" && returnOwnerNotice && (
        <div className="owner-notice">
          <span className="owner-notice-label">归属提示</span>
          <span className="owner-notice-code">{returnOwnerNotice.barcode}</span>
          <span>是</span>
          <strong>{returnOwnerNotice.customer}</strong>
          <span>的货</span>
        </div>
      )}

      <div className="current-batch">
        <h3>当前批次条码:</h3>
        <div className="batch-list">
          {batchBarcodes.map((bc, i) => (
            <div key={`${bc}-${i}`} className="batch-item">
              <span className="batch-code">{bc}</span>
              <button
                type="button"
                className="delete-batch-btn"
                onClick={() => confirmDeleteBatchBarcode(bc, i)}
              >
                删除
              </button>
            </div>
          ))}
        </div>
      </div>

      <div className="recording-actions">
        <button className="finish-btn" onClick={finishBatch}>完成录入</button>
      </div>
    </div>
  );

  const selectedCustomerSet = new Set(selectedCustomerNames);
  const allCustomersSelected = summary.customer_stats.length > 0 &&
    summary.customer_stats.every(stat => selectedCustomerSet.has(stat.name));

  return (
    <div className="container">
      <h1>仓库出货退货管理系统</h1>

      <div className={`view-container ${view === "recording" ? "recording-container" : ""}`}>
        {view === "home" && renderHome()}
        {view === "shipment_setup" && renderShipmentSetup()}
        {view === "recording" && renderRecording()}
      </div>

      <div className="summary-section">
        <div className="summary-header">
          <div>
            <h3>总体统计</h3>
            <span>总出货: {summary.total_shipments} | 总退货: {summary.total_returns}</span>
          </div>
          <div className="summary-actions">
            <label className="unit-price-field">
              单价
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
              className="summary-export-btn"
              disabled={selectedCustomerNames.length === 0}
              onClick={exportSelectedCustomerStatements}
            >
              导出选中出退货清单
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
                <th>出货数量</th>
                <th>退货数量</th>
                <th>操作</th>
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
                  <td>{stat.name}</td>
                  <td>{stat.shipment_count} 件</td>
                  <td className={stat.return_count > 0 ? "has-returns" : ""}>{stat.return_count} 件</td>
                  <td>
                    <button type="button" className="row-export-btn" onClick={() => exportCustomerStatement(stat.name)}>
                      导出
                    </button>
                  </td>
                </tr>
              ))}
              {summary.customer_stats.length === 0 && <tr><td colSpan={5} style={{textAlign:'center'}}>暂无数据</td></tr>}
            </tbody>
          </table>
        </div>
      </div>

      <div className="log-panel">
        <h4>操作日志</h4>
        <div className="log-list">
          {log.map((entry, i) => (
            <div key={i} className={`log-entry ${entry.type}`}>{entry.msg}</div>
          ))}
        </div>
      </div>

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
            <div className="modal-buttons">
              <button onClick={confirmImport}>确认导入</button>
              <button onClick={() => setShowImportDialog(false)}>取消</button>
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
              <p className="hint">新建前如果有未保存数据，会先要求保存；新表会创建为包含出货条码、客户、退货条码表头的 Excel 文件。</p>
            </div>
            <div className="modal-buttons">
              <button onClick={confirmNewTable}>确认新建</button>
              <button onClick={() => setShowNewTableDialog(false)}>取消</button>
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
              <p className="hint">用于发送给收货人的单列清单，默认文件名会追加“出货”。</p>
            </div>
            <div className="modal-buttons">
              <button onClick={confirmRecipientListExport}>确认导出</button>
              <button onClick={() => setShowRecipientListDialog(false)}>取消</button>
            </div>
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
              <button className="secondary" onClick={playAlertSound}>测试声音</button>
              <button onClick={() => setShowSettings(false)}>关闭</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default App;
