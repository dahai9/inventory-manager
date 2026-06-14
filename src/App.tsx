import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open, save, message } from "@tauri-apps/plugin-dialog";
import "./App.css";

type View = "home" | "shipment_setup" | "recording";
type Mode = "shipment" | "return";

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
  const [log, setLog] = useState<{msg: string, type: 'success' | 'error'}[]>([]);
  const [summary, setSummary] = useState<Summary>({ total_shipments: 0, total_returns: 0, customer_stats: [] });
  
  const [importPath, setImportPath] = useState<string | null>(null);
  const [columns, setColumns] = useState<string[]>([]);
  const [shipCol, setShipCol] = useState("");
  const [retCol, setRetCol] = useState("");
  const [custCol, setCustCol] = useState("");
  const [showImportDialog, setShowImportDialog] = useState(false);
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
      await playAlertSound();
      setBarcode("");
      return;
    }

    if (batchBarcodes.includes(barcode)) {
      addLog(`条码 ${barcode} 在当前批次中已存在`, "error");
      setBarcode("");
      return;
    }

    try {
      await invoke(mode === "shipment" ? "check_shipment" : "check_return", { barcode });
      setBatchBarcodes(prev => [barcode, ...prev]);
      addLog(`已扫描: ${barcode}`, "success");
    } catch (err) {
      const errMsg = String(err);
      addLog(errMsg, "error");
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
      updateSummary();
      setView("home");
    } catch (err) {
      addLog("录入失败: " + err, "error");
    }
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

  const renderHome = () => (
    <div className="home-view">
      <div className="main-actions">
        <button className="large-btn ship" onClick={() => { setMode("shipment"); setView("shipment_setup"); }}>开始出货录入</button>
        <button className="large-btn return" onClick={() => { setMode("return"); setView("recording"); setBatchBarcodes([]); }}>开始退货录入</button>
      </div>
      <div className="secondary-actions">
        <button onClick={handleImport}>导入数据</button>
        <button onClick={handleExport}>导出数据</button>
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
        <button disabled={!customer} onClick={() => { setView("recording"); setBatchBarcodes([]); }}>开始录入</button>
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

      <div className="current-batch">
        <h3>当前批次条码:</h3>
        <div className="batch-list">
          {batchBarcodes.map((bc, i) => <div key={i} className="batch-item">{bc}</div>)}
        </div>
      </div>

      <div className="recording-actions">
        <button className="finish-btn" onClick={finishBatch}>完成录入</button>
      </div>
    </div>
  );

  return (
    <div className="container">
      <h1>仓库出货退货管理系统</h1>

      <div className="view-container">
        {view === "home" && renderHome()}
        {view === "shipment_setup" && renderShipmentSetup()}
        {view === "recording" && renderRecording()}
      </div>

      <div className="summary-section">
        <div className="summary-header">
          <h3>总体统计</h3>
          <span>总出货: {summary.total_shipments} | 总退货: {summary.total_returns}</span>
        </div>
        <div className="summary-table-container">
          <table className="summary-table">
            <thead>
              <tr>
                <th>客户</th>
                <th>出货数量</th>
                <th>退货数量</th>
              </tr>
            </thead>
            <tbody>
              {summary.customer_stats.map((stat) => (
                <tr key={stat.name}>
                  <td>{stat.name}</td>
                  <td>{stat.shipment_count} 件</td>
                  <td className={stat.return_count > 0 ? "has-returns" : ""}>{stat.return_count} 件</td>
                </tr>
              ))}
              {summary.customer_stats.length === 0 && <tr><td colSpan={3} style={{textAlign:'center'}}>暂无数据</td></tr>}
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
