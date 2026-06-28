use calamine::{open_workbook_auto, Reader};
use rodio::{source::SineWave, DeviceSinkBuilder, MixerDeviceSink, Source};
use rust_xlsxwriter::*;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

#[derive(Default, Serialize, Deserialize, Clone)]
pub struct AppData {
    // barcode -> shipment record
    pub shipments: HashMap<String, ShipmentRecord>,
    // barcode -> return time
    pub returns: HashMap<String, String>,
    pub is_dirty: bool,
}

#[derive(Default, Serialize, Deserialize, Clone)]
pub struct ShipmentRecord {
    pub customer: String,
    pub shipment_time: String,
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
    let shipment = data
        .shipments
        .get(&barcode)
        .ok_or_else(|| format!("找不到此货 {}, 不是我们的货", barcode))?;
    if data.returns.contains_key(&barcode) {
        return Err(format!("此货 {} 已经扫描过退货", barcode));
    }
    Ok(shipment.customer.clone())
}

#[derive(Serialize)]
pub struct ReturnLookupResult {
    pub barcode: String,
    pub customer: String,
    pub shipment_time: String,
    pub is_returned: bool,
    pub return_time: Option<String>,
}

#[tauri::command]
fn lookup_return(
    state: tauri::State<AppState>,
    barcode: String,
) -> Result<ReturnLookupResult, String> {
    let data = state.data.lock().unwrap();
    let shipment = data
        .shipments
        .get(&barcode)
        .ok_or_else(|| format!("找不到此货 {}, 不是我们的货", barcode))?;
    let return_time = data.returns.get(&barcode).cloned();
    let is_returned = return_time.is_some();

    Ok(ReturnLookupResult {
        barcode: barcode.clone(),
        customer: shipment.customer.clone(),
        shipment_time: shipment.shipment_time.clone(),
        is_returned,
        return_time,
    })
}

#[tauri::command]
fn commit_shipment_batch(
    state: tauri::State<AppState>,
    customer: String,
    shipment_time: String,
    barcodes: Vec<String>,
) -> Result<String, String> {
    let customer = customer.trim().to_string();
    if customer.is_empty() {
        return Err("请输入客户名称".into());
    }
    let shipment_time = shipment_time.trim().to_string();
    if shipment_time.is_empty() {
        return Err("请选择出货时间".into());
    }

    let mut data = state.data.lock().unwrap();
    let mut added = 0;
    for bc in barcodes {
        if !data.shipments.contains_key(&bc) {
            data.shipments.insert(
                bc,
                ShipmentRecord {
                    customer: customer.clone(),
                    shipment_time: shipment_time.clone(),
                },
            );
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
    return_time: String,
) -> Result<String, String> {
    let return_time = return_time.trim().to_string();
    if return_time.is_empty() {
        return Err("请选择退货时间".into());
    }

    let mut data = state.data.lock().unwrap();
    let mut added = 0;
    for bc in barcodes {
        if data.shipments.contains_key(&bc) && !data.returns.contains_key(&bc) {
            data.returns.insert(bc, return_time.clone());
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
    pub total_delivered: usize,
    pub return_time_stats: Vec<ReturnTimeCustomerStat>,
    pub customer_stats: Vec<CustomerStat>,
}

#[derive(Serialize, Clone)]
pub struct ReturnTimeCustomerStat {
    pub return_time: String,
    pub customer: String,
    pub return_count: usize,
}

#[derive(Serialize)]
pub struct CustomerStat {
    pub name: String,
    pub shipment_count: usize,
    pub return_count: usize,
    pub delivered_count: usize,
}

fn build_summary(data: &AppData) -> Summary {
    let mut stats: HashMap<String, CustomerStat> = HashMap::new();

    for shipment in data.shipments.values() {
        stats
            .entry(shipment.customer.clone())
            .or_insert(CustomerStat {
                name: shipment.customer.clone(),
                shipment_count: 0,
                return_count: 0,
                delivered_count: 0,
            })
            .shipment_count += 1;
    }

    for barcode in data.returns.keys() {
        if let Some(shipment) = data.shipments.get(barcode) {
            stats
                .entry(shipment.customer.clone())
                .or_insert(CustomerStat {
                    name: shipment.customer.clone(),
                    shipment_count: 0,
                    return_count: 0,
                    delivered_count: 0,
                })
                .return_count += 1;
        }
    }

    let mut customer_stats: Vec<_> = stats.into_values().collect();
    for stat in &mut customer_stats {
        stat.delivered_count = stat.shipment_count.saturating_sub(stat.return_count);
    }
    customer_stats.sort_by(|a, b| b.shipment_count.cmp(&a.shipment_count));

    Summary {
        total_shipments: data.shipments.len(),
        total_returns: data.returns.len(),
        total_delivered: data.shipments.len().saturating_sub(data.returns.len()),
        return_time_stats: build_return_time_customer_stats(data.returns.iter().filter_map(
            |(barcode, return_time)| {
                data.shipments
                    .get(barcode)
                    .map(|shipment| (return_time.as_str(), shipment.customer.as_str()))
            },
        )),
        customer_stats,
    }
}

#[tauri::command]
fn get_summary(state: tauri::State<AppState>) -> Summary {
    let data = state.data.lock().unwrap();
    build_summary(&data)
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
    shipment_time_col: Option<String>,
    return_col: String,
    return_time_col: Option<String>,
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
    let shipment_time_idx = match shipment_time_col.as_deref().map(str::trim) {
        Some("") | None => None,
        Some(col) => Some(
            header
                .iter()
                .position(|c| c == col)
                .ok_or("未找到出货时间列")?,
        ),
    };
    let return_time_idx = match return_time_col.as_deref().map(str::trim) {
        Some("") | None => None,
        Some(col) => Some(
            header
                .iter()
                .position(|c| c == col)
                .ok_or("未找到退货时间列")?,
        ),
    };

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
        let shipment_time_val = shipment_time_idx
            .and_then(|idx| row.get(idx))
            .map(|v| v.to_string())
            .unwrap_or_default()
            .trim()
            .to_string();
        let return_time_val = return_time_idx
            .and_then(|idx| row.get(idx))
            .map(|v| v.to_string())
            .unwrap_or_default()
            .trim()
            .to_string();

        if !customer_val.is_empty() {
            last_customer = customer_val;
        }

        if !ship_val.is_empty() {
            data.shipments.insert(
                ship_val,
                ShipmentRecord {
                    customer: last_customer.clone(),
                    shipment_time: shipment_time_val,
                },
            );
        }
        if !return_val.is_empty() {
            data.returns.insert(return_val, return_time_val);
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
        .write_with_format(0, 2, "出货时间", &header_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(0, 3, "退货条码", &header_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(0, 4, "退货时间", &header_format)
        .map_err(|e| e.to_string())?;

    worksheet
        .set_column_width(0, 30)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(1, 20)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(2, 18)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(3, 30)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(4, 18)
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

fn compare_return_times(a_time: &str, b_time: &str) -> Ordering {
    let a_missing_time = a_time.trim().is_empty();
    let b_missing_time = b_time.trim().is_empty();
    a_missing_time
        .cmp(&b_missing_time)
        .then_with(|| a_time.cmp(b_time))
}

fn compare_return_fields(a_barcode: &str, a_time: &str, b_barcode: &str, b_time: &str) -> Ordering {
    compare_return_times(a_time, b_time).then_with(|| a_barcode.cmp(b_barcode))
}

#[derive(Clone)]
struct ShipmentEntry {
    barcode: String,
    shipment_time: String,
}

fn compare_shipment_fields(a: &ShipmentEntry, b: &ShipmentEntry) -> Ordering {
    compare_return_times(&a.shipment_time, &b.shipment_time).then_with(|| a.barcode.cmp(&b.barcode))
}

fn recipient_shipment_rows(data: &AppData) -> Vec<ShipmentEntry> {
    let mut rows: Vec<_> = data
        .shipments
        .iter()
        .filter_map(|(barcode, shipment)| {
            if data.returns.contains_key(barcode) {
                None
            } else {
                Some(ShipmentEntry {
                    barcode: barcode.clone(),
                    shipment_time: shipment.shipment_time.clone(),
                })
            }
        })
        .collect();
    rows.sort_by(compare_shipment_fields);
    rows
}

fn build_return_time_customer_stats<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<ReturnTimeCustomerStat> {
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for (return_time, customer) in entries {
        *counts
            .entry((return_time.trim().to_string(), customer.trim().to_string()))
            .or_insert(0) += 1;
    }

    let mut stats: Vec<_> = counts
        .into_iter()
        .map(
            |((return_time, customer), return_count)| ReturnTimeCustomerStat {
                return_time,
                customer,
                return_count,
            },
        )
        .collect();
    stats.sort_by(|a, b| {
        compare_return_times(&a.return_time, &b.return_time)
            .then_with(|| a.customer.cmp(&b.customer))
    });
    stats
}

fn return_time_display_label(return_time: &str) -> String {
    let return_time = return_time.trim();
    if return_time.is_empty() {
        return "未记录时间".to_string();
    }

    let mut parts = return_time.split('-');
    if let (Some(_year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    {
        if let (Ok(month), Ok(day)) = (month.parse::<u32>(), day.parse::<u32>()) {
            return format!("{month}月{day}日");
        }
    }

    return_time.to_string()
}

fn write_return_time_customer_stats(
    worksheet: &mut Worksheet,
    start_row: u32,
    stats: &[ReturnTimeCustomerStat],
    header_format: &Format,
    text_format: &Format,
    integer_format: &Format,
    total_format: &Format,
) -> Result<u32, String> {
    if stats.is_empty() {
        return Ok(start_row);
    }

    worksheet
        .write_with_format(start_row, 0, "退货时间统计", header_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(start_row, 1, "客户", header_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(start_row, 2, "数量", header_format)
        .map_err(|e| e.to_string())?;

    let mut current_row = start_row + 1;
    let mut i = 0;
    while i < stats.len() {
        let return_time = stats[i].return_time.as_str();
        let group_start = i;
        let mut group_total = 0usize;
        while i < stats.len() && stats[i].return_time == return_time {
            group_total += stats[i].return_count;
            i += 1;
        }

        if i - group_start > 1 {
            worksheet
                .write_with_format(
                    current_row,
                    0,
                    return_time_display_label(return_time),
                    total_format,
                )
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(current_row, 1, "合计", total_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(current_row, 2, group_total as u32, total_format)
                .map_err(|e| e.to_string())?;
            current_row += 1;
        }

        for stat in &stats[group_start..i] {
            worksheet
                .write_with_format(
                    current_row,
                    0,
                    return_time_display_label(&stat.return_time),
                    text_format,
                )
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(current_row, 1, &stat.customer, text_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write_with_format(current_row, 2, stat.return_count as u32, integer_format)
                .map_err(|e| e.to_string())?;
            current_row += 1;
        }
    }

    Ok(current_row)
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
        .write(0, 2, "出货时间")
        .map_err(|e| e.to_string())?;
    worksheet
        .write(0, 3, "退货条码")
        .map_err(|e| e.to_string())?;
    worksheet
        .write(0, 4, "退货时间")
        .map_err(|e| e.to_string())?;

    worksheet
        .set_column_width(0, 30)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(1, 20)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(2, 18)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(3, 30)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(4, 18)
        .map_err(|e| e.to_string())?;

    // Group shipments by customer
    let mut shipments_by_cust: HashMap<String, Vec<ShipmentEntry>> = HashMap::new();
    for (barcode, shipment) in &data.shipments {
        shipments_by_cust
            .entry(shipment.customer.clone())
            .or_default()
            .push(ShipmentEntry {
                barcode: barcode.clone(),
                shipment_time: shipment.shipment_time.clone(),
            });
    }

    let mut row_idx = 1;
    let mut customers: Vec<_> = shipments_by_cust.keys().collect();
    customers.sort();

    for cust in customers {
        let entries = &shipments_by_cust[cust];
        let start_row = row_idx;

        let mut active_barcodes = Vec::new();
        let mut returned_barcodes = Vec::new();
        for entry in entries {
            if let Some(return_time) = data.returns.get(&entry.barcode) {
                returned_barcodes.push((entry, return_time));
            } else {
                active_barcodes.push(entry);
            }
        }
        active_barcodes.sort_by(|a, b| compare_shipment_fields(a, b));
        returned_barcodes.sort_by(|(a_entry, a_time), (b_entry, b_time)| {
            compare_return_fields(&a_entry.barcode, a_time, &b_entry.barcode, b_time)
        });

        for entry in active_barcodes {
            worksheet
                .write(row_idx as u32, 0, &entry.barcode)
                .map_err(|e| e.to_string())?;
            worksheet
                .write(row_idx as u32, 2, &entry.shipment_time)
                .map_err(|e| e.to_string())?;
            row_idx += 1;
        }

        for (entry, _) in returned_barcodes {
            worksheet
                .write_with_format(row_idx as u32, 0, &entry.barcode, &red_format)
                .map_err(|e| e.to_string())?;
            worksheet
                .write(row_idx as u32, 2, &entry.shipment_time)
                .map_err(|e| e.to_string())?;
            row_idx += 1;
        }

        let row_count = row_idx - start_row;
        if row_count > 1 {
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
        } else if row_count == 1 {
            worksheet
                .write_with_format(start_row as u32, 1, cust, &center_format)
                .map_err(|e| e.to_string())?;
        }
    }

    // Write returns list independently in Columns D-E.
    let mut returns_vec: Vec<_> = data.returns.iter().collect();
    returns_vec.sort_by(|(a_barcode, a_time), (b_barcode, b_time)| {
        compare_return_fields(a_barcode, a_time, b_barcode, b_time)
    });

    for (i, (barcode, return_time)) in returns_vec.into_iter().enumerate() {
        worksheet
            .write((i + 1) as u32, 3, barcode)
            .map_err(|e| e.to_string())?;
        worksheet
            .write((i + 1) as u32, 4, return_time)
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

    match column.as_str() {
        "shipment_barcode" => {
            worksheet
                .write(0, 0, "出货条码")
                .map_err(|e| e.to_string())?;
            worksheet
                .write(0, 1, "出货时间")
                .map_err(|e| e.to_string())?;
            worksheet
                .set_column_width(0, 30)
                .map_err(|e| e.to_string())?;
            worksheet
                .set_column_width(1, 18)
                .map_err(|e| e.to_string())?;

            let mut row_idx = 1;
            for entry in recipient_shipment_rows(&data) {
                worksheet
                    .write(row_idx, 0, &entry.barcode)
                    .map_err(|e| e.to_string())?;
                worksheet
                    .write(row_idx, 1, &entry.shipment_time)
                    .map_err(|e| e.to_string())?;
                row_idx += 1;
            }
        }
        "customer" => {
            worksheet.write(0, 0, "客户").map_err(|e| e.to_string())?;
            worksheet
                .set_column_width(0, 20)
                .map_err(|e| e.to_string())?;

            let mut rows: Vec<_> = data
                .shipments
                .values()
                .map(|shipment| shipment.customer.as_str())
                .collect();
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
                .write(0, 1, "退货时间")
                .map_err(|e| e.to_string())?;
            worksheet
                .set_column_width(0, 30)
                .map_err(|e| e.to_string())?;
            worksheet
                .set_column_width(1, 18)
                .map_err(|e| e.to_string())?;

            let mut rows: Vec<_> = data.returns.iter().collect();
            rows.sort_by(|(a_barcode, a_time), (b_barcode, b_time)| {
                compare_return_fields(a_barcode, a_time, b_barcode, b_time)
            });
            for (i, (barcode, return_time)) in rows.into_iter().enumerate() {
                worksheet
                    .write((i + 1) as u32, 0, barcode)
                    .map_err(|e| e.to_string())?;
                worksheet
                    .write((i + 1) as u32, 1, return_time)
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
    shipment_entries: Vec<ShipmentEntry>,
    return_entries: Vec<ReturnEntry>,
    shipment_count: usize,
    return_count: usize,
}

struct ReturnEntry {
    barcode: String,
    return_time: String,
}

fn customer_statement_row(data: &AppData, customer: &str) -> Result<CustomerStatementRow, String> {
    let mut shipment_entries: Vec<_> = data
        .shipments
        .iter()
        .filter_map(|(barcode, shipment)| {
            if shipment.customer == customer {
                Some(ShipmentEntry {
                    barcode: barcode.clone(),
                    shipment_time: shipment.shipment_time.clone(),
                })
            } else {
                None
            }
        })
        .collect();
    shipment_entries.sort_by(compare_shipment_fields);

    let mut return_entries: Vec<_> = data
        .returns
        .iter()
        .filter_map(|(barcode, return_time)| {
            if data
                .shipments
                .get(barcode)
                .is_some_and(|shipment| shipment.customer == customer)
            {
                Some(ReturnEntry {
                    barcode: barcode.clone(),
                    return_time: return_time.clone(),
                })
            } else {
                None
            }
        })
        .collect();
    return_entries.sort_by(|a, b| {
        compare_return_fields(&a.barcode, &a.return_time, &b.barcode, &b.return_time)
    });

    if shipment_entries.is_empty() && return_entries.is_empty() {
        return Err(format!("客户 {} 没有可导出的出退货数据", customer));
    }

    Ok(CustomerStatementRow {
        customer: customer.to_string(),
        shipment_count: shipment_entries.len(),
        return_count: return_entries.len(),
        shipment_entries,
        return_entries,
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
    let return_time_stats = build_return_time_customer_stats(rows.iter().flat_map(|row| {
        row.return_entries
            .iter()
            .map(|entry| (entry.return_time.as_str(), row.customer.as_str()))
    }));

    worksheet
        .merge_range(0, 0, 0, 8, "出退货清单", &title_format)
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

    let mut detail_header_row = 3u32;
    if !return_time_stats.is_empty() {
        detail_header_row = write_return_time_customer_stats(
            &mut *worksheet,
            detail_header_row,
            &return_time_stats,
            &header_format,
            &text_format,
            &integer_format,
            &total_format,
        )?;
    }

    let headers = [
        "客户",
        "出货数量",
        "退货数量",
        "单价",
        "最终货款",
        "出货编码",
        "出货时间",
        "退货编码",
        "退货时间",
    ];
    for (col, header) in headers.iter().enumerate() {
        worksheet
            .write_with_format(detail_header_row, col as u16, *header, &header_format)
            .map_err(|e| e.to_string())?;
    }

    let mut current_row = detail_header_row + 1;
    for row in rows {
        let num_barcodes = row
            .shipment_entries
            .len()
            .max(row.return_entries.len())
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
            if let Some(entry) = row.shipment_entries.get(i) {
                worksheet
                    .write_with_format(r, 5, &entry.barcode, &text_format)
                    .map_err(|e| e.to_string())?;
                worksheet
                    .write_with_format(r, 6, &entry.shipment_time, &text_format)
                    .map_err(|e| e.to_string())?;
            } else {
                worksheet
                    .write_with_format(r, 5, "", &text_format)
                    .map_err(|e| e.to_string())?;
                worksheet
                    .write_with_format(r, 6, "", &text_format)
                    .map_err(|e| e.to_string())?;
            }

            if let Some(entry) = row.return_entries.get(i) {
                worksheet
                    .write_with_format(r, 7, &entry.barcode, &text_format)
                    .map_err(|e| e.to_string())?;
                worksheet
                    .write_with_format(r, 8, &entry.return_time, &text_format)
                    .map_err(|e| e.to_string())?;
            } else {
                worksheet
                    .write_with_format(r, 7, "", &text_format)
                    .map_err(|e| e.to_string())?;
                worksheet
                    .write_with_format(r, 8, "", &text_format)
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
        .set_column_width(6, 18)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(7, 34)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(8, 18)
        .map_err(|e| e.to_string())?;

    workbook.save(path.into()).map_err(|e| e.to_string())?;
    Ok(())
}

fn write_total_quantity_table(path: impl Into<PathBuf>, summary: &Summary) -> Result<(), String> {
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
    let text_format = Format::new().set_border(FormatBorder::Thin);
    let total_format = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xF2F2F2))
        .set_border(FormatBorder::Thin);

    worksheet
        .merge_range(0, 0, 0, 7, "总出退货数量表", &title_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 0, "总出货", &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 1, summary.total_shipments as u32, &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 2, "退货总数", &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 3, summary.total_returns as u32, &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 4, "客户数", &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 5, summary.customer_stats.len() as u32, &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 6, "成功交货", &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(1, 7, summary.total_delivered as u32, &total_format)
        .map_err(|e| e.to_string())?;

    let mut detail_header_row = 3u32;
    if !summary.return_time_stats.is_empty() {
        detail_header_row = write_return_time_customer_stats(
            &mut *worksheet,
            detail_header_row,
            &summary.return_time_stats,
            &header_format,
            &text_format,
            &integer_format,
            &total_format,
        )?;
    }

    let headers = ["客户", "出货数量", "退货数量", "成功交货数量"];
    for (col, header) in headers.iter().enumerate() {
        worksheet
            .write_with_format(detail_header_row, col as u16, *header, &header_format)
            .map_err(|e| e.to_string())?;
    }

    let mut current_row = detail_header_row + 1;
    for stat in &summary.customer_stats {
        worksheet
            .write_with_format(current_row, 0, &stat.name, &text_format)
            .map_err(|e| e.to_string())?;
        worksheet
            .write_with_format(current_row, 1, stat.shipment_count as u32, &integer_format)
            .map_err(|e| e.to_string())?;
        worksheet
            .write_with_format(current_row, 2, stat.return_count as u32, &integer_format)
            .map_err(|e| e.to_string())?;
        worksheet
            .write_with_format(current_row, 3, stat.delivered_count as u32, &integer_format)
            .map_err(|e| e.to_string())?;
        current_row += 1;
    }

    worksheet
        .write_with_format(current_row, 0, "合计", &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(
            current_row,
            1,
            summary.total_shipments as u32,
            &total_format,
        )
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(current_row, 2, summary.total_returns as u32, &total_format)
        .map_err(|e| e.to_string())?;
    worksheet
        .write_with_format(
            current_row,
            3,
            summary.total_delivered as u32,
            &total_format,
        )
        .map_err(|e| e.to_string())?;

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
        .set_column_width(4, 12)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(5, 12)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(6, 12)
        .map_err(|e| e.to_string())?;
    worksheet
        .set_column_width(7, 12)
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
async fn export_total_quantity_table(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let data = state.data.lock().unwrap();
    let summary = build_summary(&data);
    write_total_quantity_table(path, &summary)
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
            lookup_return,
            commit_shipment_batch,
            commit_return_batch,
            get_summary,
            get_excel_columns,
            import_data,
            has_unsaved_changes,
            create_new_workbook,
            export_data,
            export_total_quantity_table,
            export_recipient_list,
            export_customer_statement,
            export_customer_statements_to_dir,
            play_beep
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipment(customer: &str, shipment_time: &str) -> ShipmentRecord {
        ShipmentRecord {
            customer: customer.to_string(),
            shipment_time: shipment_time.to_string(),
        }
    }

    #[test]
    fn recipient_shipment_rows_excludes_returned_products() {
        let mut data = AppData::default();
        data.shipments
            .insert("SHIP-2".to_string(), shipment("客户A", "2026-06-02"));
        data.shipments
            .insert("SHIP-1".to_string(), shipment("客户A", "2026-06-01"));
        data.shipments
            .insert("RETURNED".to_string(), shipment("客户A", "2026-06-03"));
        data.returns
            .insert("RETURNED".to_string(), "2026-06-04".to_string());

        let rows = recipient_shipment_rows(&data);
        let barcodes: Vec<_> = rows.iter().map(|row| row.barcode.as_str()).collect();

        assert_eq!(barcodes, ["SHIP-1", "SHIP-2"]);
    }
}
