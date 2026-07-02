# 1. Use Tauri v2 (Rust core + web UI)

## Status

Accepted.

## Context

skillmon must run as a lightweight, always-resident tray/menu-bar app on both macOS and Windows, with native window materials (Mica, macOS vibrancy), global hotkeys, autostart, native notifications, a file watcher over `~/.claude`, and local SQLite.
The domain logic (transcript parsing, tokenization, mutation ops) is systems work that wants a fast, typed, native runtime; the panel is a small amount of UI.

## Decision

Build on Tauri v2: a Rust core for all domain logic and a web UI for the panel.

The capability audit confirmed every required primitive exists in Tauri v2 or its official plugins: core `TrayIconBuilder`, `tauri-plugin-positioner`, `tauri-plugin-global-shortcut`, `tauri-plugin-notification`, `tauri-plugin-autostart`, `tauri-plugin-single-instance`, `tauri-plugin-updater`, native window effects (`EffectsBuilder` / `window-vibrancy`), and the bundler for signing/notarization.

## Consequences

- All confirmed crates are pinned and re-verified at build time; versions drift.
- Three things need custom native code and are out of scope for v1 polish: a true macOS `NSPopover` (arrow, auto-anchor), a system-owned Windows 11 flyout with taskbar-edge docking, and interactive Windows toast actions. The positioner + hide-on-blur + `Accessory` activation policy approach is the shipping default.
- Notifications and the updater only behave correctly from a signed/installed build; dev builds suppress toasts on both OSes. Notification and update testing must run against installed builds.

## Options considered

- **Electron** — heavier runtime, no typed systems core; rejected.
- **Native per-OS (SwiftUI + WinUI)** — best fidelity, but doubles the codebase and the domain logic; rejected for v1.
- **Tauri v2** — chosen: one Rust core, small web UI, all primitives confirmed.
