# 将 Tauri 应用集成到 NixOS

由于您使用的是 NixOS，您可以通过修改现有的 `flake.nix` 来定义一个可以直接运行或安装的软件包。

### 1. 修改 `flake.nix`

建议将您的 `flake.nix` 更新为以下结构。它不仅提供开发环境，还定义了如何构建最终的二进制文件：

```nix
{
  description = "仓库出货退货管理系统";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # 运行时需要的库
        libraries = with pkgs; [
          webkitgtk_4_1
          gtk3
          cairo
          gdk-pixbuf
          glib
          dbus
          librsvg
          libsoup_3
          libayatana-appindicator
          alsa-lib
        ];

        # 编译时需要的工具
        nativeBuildInputs = with pkgs; [
          pkg-config
          wrapGAppsHook4
          nodejs
          (rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" ];
          })
        ];
      in
      {
        # 开发环境 (nix develop)
        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs;
          buildInputs = libraries;
          shellHook = ''
            export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath libraries}:$LD_LIBRARY_PATH
            export XDG_DATA_DIRS=${pkgs.gsettings-desktop-schemas}/share/gsettings-data-schemas:${pkgs.gtk3}/share/gsettings-data-schemas:$XDG_DATA_DIRS
          '';
        };

        # 软件包定义 (nix build)
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "inventory-manager";
          version = "0.2.0";
          src = ./.; # 指向包含 package.json 和 src-tauri 的目录

          # 这里的 hash 需要在第一次构建报错后根据提示填入，或者使用下方的本地构建方式
          cargoLock.lockFile = ./src-tauri/Cargo.lock;

          inherit nativeBuildInputs;
          buildInputs = libraries;

          # 构建前端
          preBuild = ''
            cd inventory-manager
            npm install
            npm run build
            cd ..
          '';

          # 告诉 Cargo 路径
          buildAndTestSubdir = "inventory-manager/src-tauri";

          postInstall = ''
            # 确保二进制文件能找到必要的库
            wrapProgram $out/bin/inventory-manager \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath libraries}"
          '';
        };
      });
}
```

### 2. 在 NixOS 系统配置中引用

如果您想将此应用永久添加到您的系统：

1.  **在 `configuration.nix` 中引用 Flake**：
    如果您的系统配置也是基于 Flake 的，可以直接将此仓库作为 input 加入：
    ```nix
    inputs.cart-app.url = "git+https://github.com/dahai9/inventory-manager.git";
    ```
2.  **添加到 `environment.systemPackages`**：
    ```nix
    environment.systemPackages = [
      inputs.cart-app.packages.${system}.default
    ];
    ```

### 3. 临时运行（不修改系统配置）

如果您只是想临时运行构建好的版本，不需要安装：
```bash
nix run github:dahai9/inventory-manager
```
或者在本地目录运行：
```bash
nix run .
```

### 注意事项：
*   **Cargo.lock**: 请确保 `inventory-manager/src-tauri/Cargo.lock` 已提交到 Git，否则 Nix 无法构建。
*   **二进制包装**: 我添加了 `wrapGAppsHook4`，它会自动处理图标、主题和 GTK 相关设置，确保在 NixOS 上运行时界面不会因为找不到 schema 而崩溃。
