# 9. Svelte + TypeScript for the tray panel

## Status

Accepted.

## Context

The Tauri web UI is a small, mostly-static panel: a sortable skill list with a few columns, badges, and disable/uninstall actions.
It is not a large SPA, and it runs inside a WebView where runtime weight and startup cost matter for a background tray app.

## Decision

Build the frontend in Svelte + TypeScript.

Its compiled, near-zero-runtime output suits a lightweight always-resident panel, and the component model is enough for this UI without a heavier framework's ceremony.

## Consequences

- Charting/table ecosystem is thinner than React's; if richer usage visualizations are needed later, expect to hand-build or pull smaller Svelte-specific libraries.
- All rendering logic stays in `src/`; the Rust core exposes typed Tauri commands and holds no view concerns.

## Options considered

- **React + TS + Vite** — largest ecosystem (charts, components), but a heavier runtime than this panel needs; rejected.
- **SolidJS + TS** — tiny and fast, but a smaller community; rejected.
- **Vanilla TS** — minimal deps, but hand-rolls list/sort/state; rejected.
- **Svelte + TS** — chosen: compiled, light, enough structure for the panel.
