# 仓库出货退货管理系统 (Rust + Tauri)

这是一个使用 Rust 和 Tauri 开发的跨平台桌面应用程序，用于管理仓库的出货和退货。

## 功能特性

- **导入 XLSX**: 支持导入现有的 Excel 表格，并手动选择哪一列作为“出货条码”，哪一列作为“退货条码”。
- **扫码出货**:
  - 支持设定收货人。
  - 条码唯一性检查（重复扫描会提示）。
  - 自动记录出货数量。
- **扫码退货**:
  - 必须是在出货记录中存在的条码才能退货。
  - 条码唯一性检查。
  - 自动记录退货数量。
- **实时统计**: 界面顶部实时显示已出货和已退货的数量。
- **导出 XLSX**: 将当前所有的出货和退货记录导出到新的 Excel 文件。

## 开发与运行

### 前置条件

1. 安装 [Rust](https://www.rust-lang.org/)。
2. 安装 [Node.js](https://nodejs.org/)。
3. 安装 Tauri 依赖 (不同操作系统请参考 [Tauri 官网](https://tauri.app/v2/start/prerequisites/))。

### 运行步骤

1. 进入项目目录:
   ```bash
   cd inventory-manager
   ```
2. 安装前端依赖:
   ```bash
   npm install
   ```
3. 启动开发模式:
   ```bash
   npm run tauri dev
   ```
4. 构建安装包:
   ```bash
   npm run tauri build
   ```

## 技术栈

- **后端**: Rust + Tauri 2.0
- **前端**: React + TypeScript + Vite
- **Excel 处理**: `calamine` (读取), `rust_xlsxwriter` (写入)
- **UI 组件**: 原生 CSS 样式
