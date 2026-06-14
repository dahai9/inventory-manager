use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use serde::{Serialize, Deserialize};
use calamine::{Reader, open_workbook_auto};
use rust_xlsxwriter::*;
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

#[derive(Default, Serialize, Deserialize, Clone)]
pub struct AppData {
    // barcode -> customer (owner)
    pub shipments: HashMap<String, String>,
    pub returns: HashSet<String>,
    pub is_dirty: bool,
}

pub struct AppState(pub Mutex<AppData>);

#[tauri::command]
fn check_shipment(state: tauri::State<AppState>, barcode: String) -> Result<(), String> {
    let data = state.0.lock().unwrap();
    if data.shipments.contains_key(&barcode) {
        return Err(format!("此货 {} 已经扫描过出货", barcode));
    }
    Ok(())
}

#[tauri::command]
fn check_return(state: tauri::State<AppState>, barcode: String) -> Result<(), String> {
    let data = state.0.lock().unwrap();
    if !data.shipments.contains_key(&barcode) {
        return Err(format!("找不到此货 {}, 不是我们的货", barcode));
    }
    if data.returns.contains(&barcode) {
        return Err(format!("此货 {} 已经扫描过退货", barcode));
    }
    Ok(())
}

#[tauri::command]
fn commit_shipment_batch(state: tauri::State<AppState>, customer: String, barcodes: Vec<String>) -> Result<String, String> {
    let mut data = state.0.lock().unwrap();
    let mut added = 0;
    for bc in barcodes {
        if !data.shipments.contains_key(&bc) {
            data.shipments.insert(bc, customer.clone());
            added += 1;
        }
    }
    if added > 0 {
        data.is_dirty = true;
    }
    Ok(format!("成功录入 {} 件货物", added))
}

#[tauri::command]
fn commit_return_batch(state: tauri::State<AppState>, barcodes: Vec<String>) -> Result<String, String> {
    let mut data = state.0.lock().unwrap();
    let mut added = 0;
    for bc in barcodes {
        if data.shipments.contains_key(&bc) && !data.returns.contains(&bc) {
            data.returns.insert(bc);
            added += 1;
        }
    }
    if added > 0 {
        data.is_dirty = true;
    }
    Ok(format!("成功退货 {} 件货物", added))
}

#[derive(Serialize)]
pub struct Summary {
    pub total_shipments: usize,
    pub total_returns: usize,
    pub customer_stats: Vec<CustomerStat>,
}

#[derive(Serialize)]
pub struct CustomerStat {
    pub name: String,
    pub shipment_count: usize,
    pub return_count: usize,
}

#[tauri::command]
fn get_summary(state: tauri::State<AppState>) -> Summary {
    let data = state.0.lock().unwrap();
    let mut stats: HashMap<String, CustomerStat> = HashMap::new();
    
    for customer in data.shipments.values() {
        stats.entry(customer.clone()).or_insert(CustomerStat {
            name: customer.clone(),
            shipment_count: 0,
            return_count: 0,
        }).shipment_count += 1;
    }
    
    for barcode in &data.returns {
        if let Some(customer) = data.shipments.get(barcode) {
            stats.entry(customer.clone()).or_insert(CustomerStat {
                name: customer.clone(),
                shipment_count: 0,
                return_count: 0,
            }).return_count += 1;
        }
    }
    
    let mut customer_stats: Vec<_> = stats.into_values().collect();
    customer_stats.sort_by(|a, b| b.shipment_count.cmp(&a.shipment_count));

    Summary {
        total_shipments: data.shipments.len(),
        total_returns: data.returns.len(),
        customer_stats,
    }
}

#[tauri::command]
async fn get_excel_columns(path: String) -> Result<Vec<String>, String> {
    let mut workbook = open_workbook_auto(path).map_err(|e| e.to_string())?;
    let sheet_name = workbook.sheet_names().get(0).ok_or("文件中没有工作表")?.clone();
    let range = workbook.worksheet_range(&sheet_name)
        .map_err(|e| format!("无法读取工作表: {}", e))?;
    
    if let Some(first_row) = range.rows().next() {
        let columns = first_row.iter().map(|cell| cell.to_string()).collect();
        Ok(columns)
    } else {
        Err("工作表是空的".into())
    }
}

#[tauri::command]
async fn import_data(state: tauri::State<'_, AppState>, path: String, ship_col: String, return_col: String, customer_col: String) -> Result<(), String> {
    let mut workbook = open_workbook_auto(path).map_err(|e| e.to_string())?;
    let sheet_name = workbook.sheet_names().get(0).ok_or("文件中没有工作表")?.clone();
    let range = workbook.worksheet_range(&sheet_name)
        .map_err(|e| format!("无法读取工作表: {}", e))?;
    
    let mut rows = range.rows();
    let header: Vec<String> = rows.next().ok_or("工作表为空")?.iter().map(|c| c.to_string()).collect();
    
    let ship_idx = header.iter().position(|c| c == &ship_col).ok_or("未找到出货列")?;
    let return_idx = header.iter().position(|c| c == &return_col).ok_or("未找到退货列")?;
    let customer_idx = header.iter().position(|c| c == &customer_col).ok_or("未找到客户列")?;

    let mut data = state.0.lock().unwrap();
    data.shipments.clear();
    data.returns.clear();
    data.is_dirty = false;

    let mut last_customer = "未知客户".to_string();

    for row in rows {
        let ship_val = row.get(ship_idx).map(|v| v.to_string()).unwrap_or_default().trim().to_string();
        let return_val = row.get(return_idx).map(|v| v.to_string()).unwrap_or_default().trim().to_string();
        let customer_val = row.get(customer_idx).map(|v| v.to_string()).unwrap_or_default().trim().to_string();
        
        if !customer_val.is_empty() {
            last_customer = customer_val;
        }

        if !ship_val.is_empty() {
            data.shipments.insert(ship_val, last_customer.clone());
        }
        if !return_val.is_empty() {
            data.returns.insert(return_val);
        }
    }
    Ok(())
}

#[tauri::command]
async fn export_data(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let mut data = state.0.lock().unwrap();
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    
    let red_format = Format::new().set_font_color(Color::Red);
    let center_format = Format::new()
        .set_align(FormatAlign::Center)
        .set_align(FormatAlign::VerticalCenter);

    worksheet.write(0, 0, "出货条码").map_err(|e| e.to_string())?;
    worksheet.write(0, 1, "客户").map_err(|e| e.to_string())?;
    worksheet.write(0, 2, "退货条码").map_err(|e| e.to_string())?;

    worksheet.set_column_width(0, 30).map_err(|e| e.to_string())?;
    worksheet.set_column_width(1, 20).map_err(|e| e.to_string())?;
    worksheet.set_column_width(2, 30).map_err(|e| e.to_string())?;

    // Group shipments by customer
    let mut shipments_by_cust: HashMap<String, Vec<String>> = HashMap::new();
    for (barcode, customer) in &data.shipments {
        shipments_by_cust.entry(customer.clone()).or_default().push(barcode.clone());
    }
    
    let mut row_idx = 1;
    let mut customers: Vec<_> = shipments_by_cust.keys().collect();
    customers.sort();
    
    for cust in customers {
        let barcodes = &shipments_by_cust[cust];
        let start_row = row_idx;
        for barcode in barcodes {
            if data.returns.contains(barcode) {
                worksheet.write_with_format(row_idx as u32, 0, barcode, &red_format).map_err(|e| e.to_string())?;
            } else {
                worksheet.write(row_idx as u32, 0, barcode).map_err(|e| e.to_string())?;
            }
            row_idx += 1;
        }
        
        if barcodes.len() > 1 {
            worksheet.merge_range(start_row as u32, 1, (row_idx - 1) as u32, 1, cust, &center_format).map_err(|e| e.to_string())?;
        } else if barcodes.len() == 1 {
            worksheet.write_with_format(start_row as u32, 1, cust, &center_format).map_err(|e| e.to_string())?;
        }
    }

    // Write returns list independently in Column C
    let mut returns_vec: Vec<_> = data.returns.iter().collect();
    returns_vec.sort();
    
    for (i, barcode) in returns_vec.into_iter().enumerate() {
        worksheet.write((i + 1) as u32, 2, barcode).map_err(|e| e.to_string())?;
    }

    workbook.save(path).map_err(|e| e.to_string())?;
    data.is_dirty = false;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState(Mutex::new(AppData::default())))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                let dirty = state.0.lock().unwrap().is_dirty;
                if dirty {
                    api.prevent_close();
                    window.dialog()
                        .message("您有新录入但未导出的数据，确定要退出吗？\n退出后未导出的数据将丢失。")
                        .title("保存提醒")
                        .buttons(MessageDialogButtons::OkCancelCustom("退出而不保存".to_string(), "返回保存".to_string()))
                        .show(move |res| {
                            if res {
                                std::process::exit(0);
                            }
                        });
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            check_shipment,
            check_return,
            commit_shipment_batch,
            commit_return_batch,
            get_summary,
            get_excel_columns,
            import_data,
            export_data
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
