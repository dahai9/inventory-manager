# Repository Guidelines

## Project Structure & Module Organization

This repository contains a Tauri 2 desktop app for warehouse shipping and return tracking. Frontend code lives in `src/`, with React entry points in `src/main.tsx`, `src/App.tsx`, and styling in `src/App.css`. Static assets are under `public/` and `src/assets/`. Rust backend commands, Excel import/export logic, app state, and audio alerts live in `src-tauri/src/`; Tauri configuration is in `src-tauri/tauri.conf.json`. Nix development and packaging support is in `flake.nix`, with Linux notes in `NIXOS.md`.

## Build, Test, and Development Commands

Run commands from `inventory-manager/`.

- `npm install`: install dependencies from `package-lock.json`.
- `npm run dev`: start the Vite frontend only.
- `npm run tauri dev`: run the full desktop app in development mode.
- `npm run build`: type-check TypeScript with `tsc` and build the Vite bundle.
- `npm run tauri build`: build the production desktop app.
- `cd src-tauri && cargo test`: compile and run Rust tests.
- `nix develop`: enter a shell with Rust, Node, and native Tauri libraries.
- `nix build`: build the packaged app through the flake.

## Coding Style & Naming Conventions

Use TypeScript with functional React components and hooks. Prefer existing plain CSS patterns in `App.css` over adding a UI framework. Keep frontend state names descriptive, using `camelCase` for variables and handlers such as `handleImport`. Rust uses edition 2021 conventions: `snake_case` functions, explicit `Result<_, String>` for Tauri command errors, and serializable structs for frontend-facing data. Format Rust with `cargo fmt`.

## Testing Guidelines

There is no dedicated frontend test suite committed yet. Treat `npm run build` as the minimum frontend validation. For backend changes, run `cd src-tauri && cargo test`; add Rust unit tests near affected import/export, barcode validation, summary, or state mutation logic. If introducing frontend tests, use `*.test.tsx` or `*.spec.tsx` beside the component.

## Commit & Pull Request Guidelines

Recent history mostly follows Conventional Commits, for example `feat: implement hermetic npm build in flake using buildNpmPackage`, `fix: adjust npm build for Nix sandbox`, and `ci: add libasound2-dev dependency for linux builds`. Prefer lowercase types such as `feat`, `fix`, `chore`, and `ci`.

Pull requests should describe the user-facing change, list validation commands run, mention Nix or Tauri packaging impacts, and include screenshots for visible UI changes. Link related issues when available.

## Security & Configuration Tips

Do not commit generated `dist/`, local spreadsheet data, or machine-specific paths. Keep Tauri filesystem and dialog permissions scoped in `src-tauri/capabilities/default.json`.
