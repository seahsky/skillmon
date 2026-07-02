# 4. Native tray UI per OS — macOS menu-bar dropdown, Windows Fluent/Mica flyout

## Status

Accepted.

## Context

skillmon is a tray-first app; the panel must feel native on each OS.
The Tauri audit confirmed native window materials exist (macOS `Effect::Popover` vibrancy, Windows 11 `Effect::Mica`) but that a true `NSPopover` (system arrow, auto-anchor, click-outside dismissal) and a system-owned Windows 11 flyout require custom native code.

## Decision

Render the panel as a borderless, effect-backed window anchored to the tray icon: an NSPopover-style menu-bar dropdown with macOS vibrancy on macOS, and a Fluent/Mica flyout on Windows.

Implementation uses core tray events + `tauri-plugin-positioner` (calling `on_tray_event` in the handler), `ActivationPolicy::Accessory` to hide the Dock icon on macOS, template-image tray icons for light/dark menubar, `show_menu_on_left_click(true)` on Windows, and manual hide-on-blur via `WindowEvent::Focused(false)`.

## Consequences

- v1 does not ship a real `NSPopover` arrow or a taskbar-docked system flyout; the anchored-window approach is the accepted fidelity level and matches most Tauri menubar apps.
- Windows requires a valid AppUserModelID / Start-menu shortcut for both toasts and correct tray behavior; Mica needs Windows 11, acrylic covers Windows 10.
- Positioner accuracy on macOS multi-monitor/notch setups must be verified on real hardware; treated as a known risk.
- A first-run Windows coach mark nudges the user to pin the tray icon (it defaults to overflow).

## Options considered

- **Custom Swift/objc2 NSPopover + Win32 flyout now** — best fidelity, significant native code; deferred, not rejected.
- **Anchored borderless window with native effects** — chosen for v1.
