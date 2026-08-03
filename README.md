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
- **离线激活**: 程序启动后先校验本机授权，未激活时只能查看机器码并输入激活码。

## 激活设计

激活码采用离线签名授权：桌面程序只内置 Ed25519 公钥，发码方保留私钥。激活码内容包含产品标识、授权编号、客户名、本机机器码、签发日期和可选到期日。程序会把验证通过的激活码保存到 Tauri 应用数据目录，后续启动和业务操作都会再次验签。

业务命令会在 Rust 后端统一检查授权状态，所以仅修改前端页面不能绕过正常使用限制。

### 发码流程

1. 生成一组发码密钥，只在发码机器执行一次:
   ```bash
   cd src-tauri
   cargo run --example license_keygen -- init-keypair
   ```
2. 保存输出的 `private_key_seed`，不要提交到仓库。将输出的 `public_key` 用于正式构建:
   ```bash
   INVENTORY_LICENSE_PUBLIC_KEY=<public_key> npm run tauri build
   ```
   Release 构建如果没有设置 `INVENTORY_LICENSE_PUBLIC_KEY` 会失败，避免正式包误用开发密钥。
   Nix 打包使用:
   ```bash
   INVENTORY_LICENSE_PUBLIC_KEY=<public_key> nix build --impure
   ```
3. 客户打开程序后复制“本机机器码”，发码方生成激活码:
   ```bash
   cd src-tauri
   cargo run --example license_keygen -- issue \
     --private-key <private_key_seed> \
     --machine <客户机器码> \
     --customer <客户名称> \
     --expires 2027-12-31
   ```
4. 客户把输出的激活码粘贴到程序激活页即可使用。省略 `--expires` 表示永久授权。

本地 debug 运行内置了开发公钥，可用下面的固定开发 seed 发测试激活码；不要用于正式发版:
```bash
cd src-tauri
cargo run --example license_keygen -- issue \
  --private-key KioqKioqKioqKioqKioqKioqKioqKioqKioqKioqKio \
  --machine <本机机器码> \
  --customer 本地测试
```

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

## 网络版服务端 POC

网络版使用 PostgreSQL 作为唯一事实源，运行账号必须是非超级用户、不能绕过 RLS 且不拥有业务表。迁移使用单独的高权限连接：

```bash
INVENTORY_DATABASE_URL=postgres://inventory_runtime:***@db/inventory \
INVENTORY_MIGRATION_DATABASE_URL=postgres://inventory_migrator:***@db/inventory \
cargo run --bin inventory-server
```

当前服务提供 `/health`、`/v1/auth/login`、`/v1/auth/refresh`、`/v1/auth/logout` 和幂等入库接口 `/v1/inbound/receipts`。服务默认只监听 `127.0.0.1:3100`；多用户部署应放在 TLS 反向代理后，不要把明文 HTTP 直接暴露到公网。
