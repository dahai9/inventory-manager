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

        # 1. 前端构建阶段
        frontend = pkgs.buildNpmPackage {
          pname = "inventory-manager-frontend";
          version = "0.2.5";
          src = ./.;
          
          # 这个 hash 是关键，它锁定了 npm 依赖
          # 第一次构建会报错并给出正确的 hash，我们需要填入它
          npmDepsHash = "sha256-dhcMCKpti6sTHx/pGFFMI1yQaqRCl8S78v/tXPckVF4=";
          
          # 仅构建前端，跳过一些不必要的检查
          dontNpmBuild = false;
          
          installPhase = ''
            mkdir -p $out
            cp -r dist/* $out/
          '';
        };
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

        # 2. 最终软件包定义 (后端构建)
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "inventory-manager";
          version = "0.2.5";
          src = ./.;

          cargoRoot = "src-tauri";
          cargoLock.lockFile = ./src-tauri/Cargo.lock;

          inherit nativeBuildInputs;
          buildInputs = libraries;

          # 将前端阶段生成的 dist 拷贝进来
          preBuild = ''
            cp -r ${frontend} dist
          '';

          cargoBuildFlags = [
            "--features=tauri/custom-protocol"
          ];

          buildAndTestSubdir = "src-tauri";

          postInstall = ''
            wrapProgram $out/bin/inventory-manager \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath libraries}"
          '';
        };
      });
}
