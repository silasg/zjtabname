# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### ✨ Features

- Upgrade to zellij-tile 0.44 with rename_tab_with_id and CwdChanged

### 🐛 Bug Fixes

- **ci:** Drop dtolnay/rust-toolchain, rely on mise for Rust + clippy + wasm target

### 👷 CI/CD

- Add GitHub Actions workflow for lint, test, and WASM build

### 📚 Documentation

- **readme:** Update for zellij 0.44 upgrade
- Add project-level AGENTS.md with build/test/release instructions

### 🔧 Miscellaneous

- **config:** Revert default poll interval to 2 seconds
- Bump version to 0.2.0 for zellij-tile 0.44 upgrade
- Add git-cliff config and generate initial CHANGELOG.md
## [0.1.0-pre-zellij-0.44] - 2026-03-12

### ♻️ Refactor

- Apply code review findings

### ✨ Features

- Initial zjtabname Zellij plugin
- Add rename_active_tab_only setting to work around Zellij bug #3535
- **rename:** Use CLI workaround for tab rename by default

### 🐛 Bug Fixes

- Use 0-indexed tab position for rename_tab()
- **poll:** Skip refocus when floating panes are visible

### 📚 Documentation

- **readme:** Document rare race condition with CLI rename workaround

### 🔧 Miscellaneous

- **debug:** Add eprintln logging for PaneManifest keys vs TabInfo positions
