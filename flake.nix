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
        pkgs = import nixpkgs {
          inherit system overlays;
        };

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
          openssl
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

        # 软件包定义 (nix build / nix run)
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "inventory-manager";
          version = "0.2.0";
          src = ./.;

          # 指向相对于 src 根目录的 Cargo.lock
          cargoLock.lockFile = ./src-tauri/Cargo.lock;

          # Nix 要求在 src 根目录下看到 Cargo.lock 才能进行校验
          prePatch = ''
            ln -s src-tauri/Cargo.lock Cargo.lock
          '';

          inherit nativeBuildInputs;
          buildInputs = libraries;

          # 构建前端流程
          preBuild = ''
            export HOME=$(mktemp -d)
            npm install
            npm run build
          '';

          # 指定 Rust 代码所在的子目录 (相对于 src)
          buildAndTestSubdir = "src-tauri";

          postInstall = ''
            # 确保二进制文件在运行时能找到必要的库 (针对 NixOS 非 FHS 环境)
            wrapProgram $out/bin/inventory-manager \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath libraries}"
          '';
        };
      });
}
