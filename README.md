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

网络版使用 PostgreSQL 作为唯一事实源，运行账号必须是非超级用户、不能绕过 RLS 且不拥有业务表。迁移使用单独的 schema owner 连接。创建运行角色后，用仓库中的授权脚本授予最小业务权限：

```bash
psql "$INVENTORY_MIGRATION_DATABASE_URL" -v ON_ERROR_STOP=1 \
  -c "CREATE ROLE inventory_runtime LOGIN PASSWORD '从密钥管理器读取' NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOBYPASSRLS"

psql "$INVENTORY_MIGRATION_DATABASE_URL" -v ON_ERROR_STOP=1 \
  -v runtime_role=inventory_runtime \
  -f src-tauri/deploy/postgres/runtime-role.sql
```

生产密码不得写入仓库或命令历史。迁移完成后再使用两个独立连接启动服务：

```bash
INVENTORY_DATABASE_URL=postgres://inventory_runtime:***@db/inventory \
INVENTORY_MIGRATION_DATABASE_URL=postgres://inventory_migrator:***@db/inventory \
cargo run --bin inventory-server
```

当前服务提供认证、库存查询、概览、幂等入库、质检、上游订单、库存分配、出库、交货和退回接口。服务默认只监听 `127.0.0.1:3100`；多用户部署必须放在 TLS 反向代理后，不要把明文 HTTP 或 PostgreSQL 直接暴露到公网。桌面客户端只接受 HTTPS 网络地址，本机 POC 的 `http://127.0.0.1` 例外。

- `/health` 只表示进程存活；`/ready` 会实际探测 PostgreSQL，负载均衡器应使用后者接流量。
- 普通 JSON 请求体限制为 2 MiB，Bearer token 限制为 1024 字节；升级上传使用下述独立上限。
- 登录后客户端通过受保护接口读取可收货仓库，操作员不需要填写仓库 UUID。

一次性升级使用 `POST /v1/upgrades/offline-imports`，调用账号必须拥有 `inventory.upgrade.import` 权限。桌面端先通过 `v2_export_upgrade_package` 生成并验证 `.invpack`，再调用 `v2_upgrade_offline_to_network` 上传到空网络工作区。只有服务端返回完全一致的 `export_id`、`migration_id` 和 checksum 后，桌面端才会把原 SQLite 工作区冻结为只读，并在 `migration_result_reports` 保存目标工作区、关键表计数和导入结果。网络导入成功但本地归档因断电失败时，可以用同一个包重试；服务端返回幂等重放后会继续完成本地归档，不会重复写入 PostgreSQL。

当前上传协议是有明确上限的首版实现：一个请求最多包含 64 MiB 的包数据，服务器请求体上限为 80 MiB。更大的工作区必须先实现可恢复的分片上传，不应调高上限来绕过内存边界。

## 离线备份与恢复

V2 的“数据与设置”页面可创建带 SHA-256、schema 版本和来源身份的一致性 `.invbackup`。备份使用 SQLite `VACUUM INTO`，不会遗漏仍在 WAL 中的已提交数据。恢复会先校验包完整性和工作区身份，然后请求应用重启；数据库连接关闭后，启动流程先生成恢复前保护性备份，再原子替换当前 SQLite 文件。恢复失败会保留原数据库并在页面显示结果。

离线授权失效时，入库、质检、凑单、出库和退回仍由 Rust 命令拒绝；库存只读查询、备份、恢复和一次性升级导出保持可用，避免用户因授权问题无法取回自己的数据。
