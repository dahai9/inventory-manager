use calamine::{open_workbook_auto, Reader};
use rodio::{source::SineWave, DeviceSinkBuilder, MixerDeviceSink, Source};
use rust_xlsxwriter::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

#[derive(Default, Serialize, Deserialize, Clone)]
pub struct AppData {
    // barcode -> customer (owner)
    pub shipments: HashMap<String, String>,
    pub returns: HashSet<String>,
    pub is_dirty: bool,
}

pub struct AppState {
    pub data: Mutex<AppData>,
    pub mixer_handle: Option<MixerDeviceSink>,
}

#[tauri::command]
fn play_beep(state: tauri::State<AppState>) {
    if let Some(handle) = &state.mixer_handle {
        // 使用 2000Hz 的高频音，非常尖锐，适合嘈杂环境
        // 播放两个连续的短促尖鸣声
        let beep1 = SineWave::new(2000.0)
            .take_duration(Duration::from_millis(100))
            .amplify(0.3);
        let beep2 = SineWave::new(2000.0)
            .delay(Duration::from_millis(150)) // 在 150ms 后播放第二声，形成 50ms 间隔
            .take_duration(Duration::from_millis(250)) // 延时 150ms + 100ms 鸣叫
            .amplify(0.3);

        // 直接将两个源加入混音器，混音器会自动处理并发/延时播放
        // Mixer::add 要求 T: Source + Send + 'static
        handle.mixer().add(beep1);
        handle.mixer().add(beep2);
    }
}

#[tauri::command]
fn check_shipment(state: tauri::State<AppState>, barcode: String) -> Result<(), String> {
    let data = state.data.lock().unwrap();
    if data.shipments.contains_key(&barcode) {
        return Err(format!("此货 {} 已经扫描过出货", barcode));
    }
    Ok(())
}

#[tauri::command]
fn check_return(state: tauri::State<AppState>, barcode: String) -> Result<String, String> {
    let data = state.data.lock().unwrap();
    let customer = data
        .shipments
        .get(&barcode)
        .ok_or_else(|| format!("找不到此货 {}, 不是我们的货", barcode))?;
    if data.returns.contains(&barcode) {
        return Err(format!("此货 {} 已经扫描过退货", barcode));
    }
    Ok(customer.clone())
}

#[tauri::command]
fn commit_shipment_batch(
    state: tauri::State<AppState>,
    customer: String,
    barcodes: Vec<String>,
) -> Result<String, String> {
    let mut data = state.data.lock().unwrap();
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
fn commit_return_batch(
    state: tauri::State<AppState>,
    barcodes: Vec<String>,
) -> Result<String, String> {
    let mut data = state.data.lock().unwrap();
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
    let data = state.data.lock().unwrap();
    let mut stats: HashMap<String, CustomerStat> = HashMap::new();

    for customer in data.shipments.values() {
        stats
            .entry(customer.clone())
            .or_insert(CustomerStat {
                name: customer.clone(),
                shipment_count: 0,
                return_count: 0,
            })
            .shipment_count += 1;
    }

    for barcode in &data.returns {
        if let Some(customer) = data.shipments.get(barcode) {
            stats
                .entry(customer.clone())
                .or_insert(CustomerStat {
                    name: customer.clone(),
                    shipment_count: 0,
                    return_count: 0,
                })
                .return_count += 1;
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
    let sheet_name = workbook
        .sheet_names()
        .get(0)
        .ok_or("文件中没有工作表")?
        .clone();
    let range = workbook
        .worksheet_range(&sheet_name)
        .map_err(|e| format!("无法读取工作表: {}", e))?;

    if let Some(first_row) = range.rows().next() {
        let columns = first_row.iter().map(|cell| cell.to_string()).collect();
        Ok(columns)
    } else {
        Err("工作表是空的".into())
    }
}

#[tauri::command]
async fn import_data(
    state: tauri::State<'_, AppState>,
    path: String,
    ship_col: String,
    return_col: String,
    customer_col: String,
) -> Result<(), String> {
    let mut workbook = open_workbook_auto(path).map_err(|e| e.to_string())?;
    let sheet_name = workbook
        .sheet_names()
        .get(0)
        .ok_or("文件中没有工作表")?
        .clone();
    let range = workbook
        .worksheet_range(&sheet_name)
        .map_err(|e| format!("无法读取工作表: {}", e))?;

    let mut rows = range.rows();
    let header: Vec<String> = rows
        .next()
        .ok_or("工作表为空")?
        .iter()
        .map(|c| c.to_string())
        .collect();

    let ship_idx = header
        .iter()
        .position(|c| c == &ship_col)
        .ok_or("未找到出货列")?;
    let return_idx = header
        .iter()
        .position(|c| c == &return_col)
        .ok_or("未找到退货列")?;
    let customer_idx = header
        .iter()
        .position(|c| c == &customer_col)
        .ok_or("未找到客户列")?;

    let mut data = state.data.lock().unwrap();
    data.shipments.clear();
    data.returns.clear();
    data.is_dirty = false;

    let mut last_customer = "未知客户".to_string();

    for row in rows {
        let ship_val = row
            .get(ship_idx)
            .map(|v| v.to_string())
            .unwrap_or_default()
            .trim()
            .to_string();
        let return_val = row
            .get(return_idx)
            .map(|v| v.to_string())
            .unwrap_or_default()
            .trim()
            .to_string();
        let customer_val = row
            .get(customer_idx)
            .map(|v| v.to_string())
            .unwrap_or_default()
            .trim()
            .to_string();

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

fn write_empty_inventory_workbook(
    path: impl Into<PathBuf>,
    sheet_name: &str,
) -> Result<(), String> {
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    let header_format = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xD9EAF7))
        .set_border(FormatBorder::Thin);

    let safe_sheet_name: String = sheet_name
        .chars()
        .map(|ch| match ch {
            ':' | '\\' | '/' | '?' | '*' | '[' | ']' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .take(31)
        .collect();
    let safe_sheet_name = safe_sheet_name.trim();
    if !safe_sheet_name.is_empty() {
        worksheet
            .set_name(safe_sheet_name)
            .map_err(|e| e.to_string())?;
    }

    worksheet
        .write_with_format(0, 0, "出货条码", &header_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(0, 1, "客户", &header_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(0, 2, "退货条码", &header_format)
        .map_err(|e| e.to_string())?;

    worksheet
        .set_column_width(0, 30)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(1, 20)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(2, 30)
        .map_err(|e| e.to_string())?;

    workbook.save(path.into()).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn has_unsaved_changes(state: tauri::State<AppState>) -> bool {
    state.data.lock().unwrap().is_dirty
}

#[tauri::command]
async fn create_new_workbook(
    state: tauri::State<'_, AppState>,
    path: String,
    table_name: String,
) -> Result<(), String> {
    write_empty_inventory_workbook(path, &table_name)?;

    let mut data = state.data.lock().unwrap();
    data.shipments.clear();
    data.returns.clear();
    data.is_dirty = false;

    Ok(())
}

#[tauri::command]
async fn export_data(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let mut data = state.data.lock().unwrap();
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();

    let red_format = Format::new().set_font_color(Color::Red);
    let center_format = Format::new()
        .set_align(FormatAlign::Center)
        .set_align(FormatAlign::VerticalCenter);

    worksheet
        .write(0, 0, "出货条码")
        .map_err(|e| e.to_string())?;
    worksheet.write(0, 1, "客户").map_err(|e| e.to_string())?;
    worksheet
        .write(0, 2, "退货条码")
        .map_err(|e| e.to_string())?;

    worksheet
        .set_column_width(0, 30)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(1, 20)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(2, 30)
        .map_err(|e| e.to_string())?;

    // Group shipments by customer
    let mut shipments_by_cust: HashMap<String, Vec<String>> = HashMap::new();
    for (barcode, customer) in &data.shipments {
        shipments_by_cust
            .entry(customer.clone())
            .or_default()
            .push(barcode.clone());
    }

    let mut row_idx = 1;
    let mut customers: Vec<_> = shipments_by_cust.keys().collect();
    customers.sort();

    for cust in customers {
        let barcodes = &shipments_by_cust[cust];
        let start_row = row_idx;
        for barcode in barcodes {
            if data.returns.contains(barcode) {
                worksheet
                    .write_with_format(row_idx as u32, 0, barcode, &red_format)
                    .map_err(|e| e.to_string())?;
            } else {
                worksheet
                    .write(row_idx as u32, 0, barcode)
                    .map_err(|e| e.to_string())?;
            }
            row_idx += 1;
        }

        if barcodes.len() > 1 {
            worksheet
                .merge_range(
                    start_row as u32,
                    1,
                    (row_idx - 1) as u32,
                    1,
                    cust,
                    &center_format,
                )
                .map_err(|e| e.to_string())?;
        } else if barcodes.len() == 1 {
            worksheet
                .write_with_format(start_row as u32, 1, cust, &center_format)
                .map_err(|e| e.to_string())?;
        }
    }

    // Write returns list independently in Column C
    let mut returns_vec: Vec<_> = data.returns.iter().collect();
    returns_vec.sort();

    for (i, barcode) in returns_vec.into_iter().enumerate() {
        worksheet
            .write((i + 1) as u32, 2, barcode)
            .map_err(|e| e.to_string())?;
    }

    workbook.save(path).map_err(|e| e.to_string())?;
    data.is_dirty = false;
    Ok(())
}

#[tauri::command]
async fn export_recipient_list(
    state: tauri::State<'_, AppState>,
    path: String,
    column: String,
) -> Result<(), String> {
    let data = state.data.lock().unwrap();
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    let red_format = Format::new().set_font_color(Color::Red);

    match column.as_str() {
        "shipment_barcode" => {
            worksheet
                .write(0, 0, "出货条码")
                .map_err(|e| e.to_string())?;
            worksheet
                .set_column_width(0, 30)
                .map_err(|e| e.to_string())?;

            let mut active_rows = Vec::new();
            let mut returned_rows = Vec::new();
            for barcode in data.shipments.keys() {
                if data.returns.contains(barcode) {
                    returned_rows.push(barcode);
                } else {
                    active_rows.push(barcode);
                }
            }
            active_rows.sort();
            returned_rows.sort();

            let mut row_idx = 1;
            for barcode in active_rows {
                worksheet
                    .write(row_idx, 0, barcode)
                    .map_err(|e| e.to_string())?;
                row_idx += 1;
            }
            for barcode in returned_rows {
                worksheet
                    .write_with_format(row_idx, 0, barcode, &red_format)
                    .map_err(|e| e.to_string())?;
                row_idx += 1;
            }
        }
        "customer" => {
            worksheet.write(0, 0, "客户").map_err(|e| e.to_string())?;
            worksheet
                .set_column_width(0, 20)
                .map_err(|e| e.to_string())?;

            let mut rows: Vec<_> = data.shipments.values().collect();
            rows.sort();
            rows.dedup();
            for (i, customer) in rows.into_iter().enumerate() {
                worksheet
                    .write((i + 1) as u32, 0, customer)
                    .map_err(|e| e.to_string())?;
            }
        }
        "return_barcode" => {
            worksheet
                .write(0, 0, "退货条码")
                .map_err(|e| e.to_string())?;
            worksheet
                .set_column_width(0, 30)
                .map_err(|e| e.to_string())?;

            let mut rows: Vec<_> = data.returns.iter().collect();
            rows.sort();
            for (i, barcode) in rows.into_iter().enumerate() {
                worksheet
                    .write((i + 1) as u32, 0, barcode)
                    .map_err(|e| e.to_string())?;
            }
        }
        _ => return Err("未知的导出列".into()),
    }

    workbook.save(path).map_err(|e| e.to_string())?;
    Ok(())
}

struct CustomerStatementRow {
    customer: String,
    shipment_barcodes: Vec<String>,
    return_barcodes: Vec<String>,
    shipment_count: usize,
    return_count: usize,
}

fn customer_statement_row(data: &AppData, customer: &str) -> Result<CustomerStatementRow, String> {
    let mut shipment_barcodes: Vec<_> = data
        .shipments
        .iter()
        .filter_map(|(barcode, owner)| {
            if owner == customer {
                Some(barcode.clone())
            } else {
                None
            }
        })
        .collect();
    shipment_barcodes.sort();

    let mut return_barcodes: Vec<_> = data
        .returns
        .iter()
        .filter_map(|barcode| {
            if data
                .shipments
                .get(barcode)
                .is_some_and(|owner| owner == customer)
            {
                Some(barcode.clone())
            } else {
                None
            }
        })
        .collect();
    return_barcodes.sort();

    if shipment_barcodes.is_empty() && return_barcodes.is_empty() {
        return Err(format!("客户 {} 没有可导出的出退货数据", customer));
    }

    Ok(CustomerStatementRow {
        customer: customer.to_string(),
        shipment_count: shipment_barcodes.len(),
        return_count: return_barcodes.len(),
        shipment_barcodes,
        return_barcodes,
    })
}

fn sanitize_filename_segment(segment: &str) -> String {
    let sanitized: String = segment
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();
    let trimmed = sanitized.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "未命名".to_string()
    } else {
        trimmed
    }
}

fn write_customer_statement(
    path: impl Into<PathBuf>,
    rows: &[CustomerStatementRow],
    unit_price: f64,
) -> Result<(), String> {
    if rows.is_empty() {
        return Err("没有可导出的客户数据".into());
    }

    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();

    let title_format = Format::new().set_bold().set_font_size(14);
    let header_format = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xD9EAF7))
        .set_border(FormatBorder::Thin);
    let integer_format = Format::new()
        .set_num_format("0")
        .set_border(FormatBorder::Thin);
    let money_format = Format::new()
        .set_num_format("#,##0.00")
        .set_border(FormatBorder::Thin);
    let text_format = Format::new().set_border(FormatBorder::Thin).set_text_wrap();
    let total_format = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xF2F2F2))
        .set_border(FormatBorder::Thin);

    let total_shipments: usize = rows.iter().map(|row| row.shipment_count).sum();
    let total_returns: usize = rows.iter().map(|row| row.return_count).sum();

    worksheet
        .merge_range(0, 0, 0, 6, "出退货清单", &title_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 0, "总数", &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 1, total_shipments as u32, &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 2, "退货总数", &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 3, total_returns as u32, &total_format)
        .map_err(|e| e.to_string())?;

    let headers = [
        "客户",
        "出货数量",
        "退货数量",
        "单价",
        "最终货款",
        "出货编码",
        "退货编码",
    ];
    for (col, header) in headers.iter().enumerate() {
        worksheet
            .write_with_format(3, col as u16, *header, &header_format)
            .map_err(|e| e.to_string())?;
    }

    let mut current_row = 4u32;
    for row in rows {
        let num_barcodes = row
            .shipment_barcodes
            .len()
            .max(row.return_barcodes.len())
            .max(1);
        let start_row = current_row;
        let end_row = current_row + num_barcodes as u32 - 1;

        let final_amount = (row.shipment_count as f64 - row.return_count as f64) * unit_price;

        if num_barcodes > 1 {
            worksheet
                .merge_range(start_row, 0, end_row, 0, "", &text_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(start_row, 0, &row.customer, &text_format)
                .map_err(|e| e.to_string())?;

            worksheet
                .merge_range(start_row, 1, end_row, 1, "", &integer_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(start_row, 1, row.shipment_count as u32, &integer_format)
                .map_err(|e| e.to_string())?;

            worksheet
                .merge_range(start_row, 2, end_row, 2, "", &integer_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(start_row, 2, row.return_count as u32, &integer_format)
                .map_err(|e| e.to_string())?;

            worksheet
                .merge_range(start_row, 3, end_row, 3, "", &money_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(start_row, 3, unit_price, &money_format)
                .map_err(|e| e.to_string())?;

            worksheet
                .merge_range(start_row, 4, end_row, 4, "", &money_format)
                .map_err(|e| e.to_string())?;
            let excel_start_row = start_row + 1;
            let formula = Formula::new(format!(
                "=(B{excel_start_row}-C{excel_start_row})*D{excel_start_row}"
            ))
            .set_result(format!("{final_amount:.2}"));
            worksheet
                .write_formula_with_format(start_row, 4, formula, &money_format)
                .map_err(|e| e.to_string())?;
        } else {
            worksheet
                .write_with_format(start_row, 0, &row.customer, &text_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(start_row, 1, row.shipment_count as u32, &integer_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(start_row, 2, row.return_count as u32, &integer_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(start_row, 3, unit_price, &money_format)
                .map_err(|e| e.to_string())?;

            let excel_row = start_row + 1;
            let formula = Formula::new(format!("=(B{excel_row}-C{excel_row})*D{excel_row}"))
                .set_result(format!("{final_amount:.2}"));
            worksheet
                .write_formula_with_format(start_row, 4, formula, &money_format)
                .map_err(|e| e.to_string())?;
        }

        for i in 0..num_barcodes {
            let r = start_row + i as u32;
            if let Some(barcode) = row.shipment_barcodes.get(i) {
                worksheet
                    .write_with_format(r, 5, barcode, &text_format)
                    .map_err(|e| e.to_string())?;
            } else {
                worksheet
                    .write_with_format(r, 5, "", &text_format)
                    .map_err(|e| e.to_string())?;
            }

            if let Some(barcode) = row.return_barcodes.get(i) {
                worksheet
                    .write_with_format(r, 6, barcode, &text_format)
                    .map_err(|e| e.to_string())?;
            } else {
                worksheet
                    .write_with_format(r, 6, "", &text_format)
                    .map_err(|e| e.to_string())?;
            }
        }

        current_row = end_row + 1;
    }

    worksheet
        .set_column_width(0, 24)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(1, 12)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(2, 12)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(3, 12)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(4, 14)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(5, 34)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(6, 34)
        .map_err(|e| e.to_string())?;

    workbook.save(path.into()).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn export_customer_statement(
    state: tauri::State<'_, AppState>,
    path: String,
    customer: String,
    unit_price: f64,
) -> Result<(), String> {
    let data = state.data.lock().unwrap();
    let row = customer_statement_row(&data, &customer)?;
    write_customer_statement(path, &[row], unit_price)
}

#[tauri::command]
async fn export_customer_statements_to_dir(
    state: tauri::State<'_, AppState>,
    directory: String,
    base_name: String,
    customers: Vec<String>,
    unit_price: f64,
) -> Result<Vec<String>, String> {
    if customers.is_empty() {
        return Err("请先选择要导出的客户".into());
    }

    let data = state.data.lock().unwrap();
    let safe_base_name = sanitize_filename_segment(&base_name);
    let mut exported_paths = Vec::new();

    for customer in customers {
        let row = customer_statement_row(&data, &customer)?;
        let filename = format!(
            "{}_{}_出退货清单.xlsx",
            safe_base_name,
            sanitize_filename_segment(&customer)
        );
        let mut path = PathBuf::from(&directory);
        path.push(filename);
        write_customer_statement(path.clone(), &[row], unit_price)?;
        exported_paths.push(path.to_string_lossy().into_owned());
    }

    Ok(exported_paths)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mixer_handle = DeviceSinkBuilder::open_default_sink().ok();

    tauri::Builder::default()
        .manage(AppState {
            data: Mutex::new(AppData::default()),
            mixer_handle,
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                let dirty = state.data.lock().unwrap().is_dirty;
                if dirty {
                    api.prevent_close();
                    window
                        .dialog()
                        .message(
                            "您有新录入但未导出的数据，确定要退出吗？\n退出后未导出的数据将丢失。",
                        )
                        .title("保存提醒")
                        .buttons(MessageDialogButtons::OkCancelCustom(
                            "退出而不保存".to_string(),
                            "返回保存".to_string(),
                        ))
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
            has_unsaved_changes,
            create_new_workbook,
            export_data,
            export_recipient_list,
            export_customer_statement,
            export_customer_statements_to_dir,
            play_beep
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
