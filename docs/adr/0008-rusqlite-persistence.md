# 8. Persist with rusqlite (bundled) in the Rust core

## Status

Proposed.

## Context

skillmon's model logic — footprint cache, deduped `turn_usage` rows, transcript-file checkpoints, `skill_ref` — lives in the Rust core, not the frontend.
Tauri offers `tauri-plugin-sql` (sqlx, exposes SQL to the JS frontend, bundled migrations) and `tauri-plugin-store` (key/value), while `rusqlite` is a direct, synchronous, typed SQLite binding.

## Decision

Use `rusqlite` with the bundled SQLite feature directly in the core for all structured data (the `transcript_file`, `turn_usage`, and `skill_ref` tables and the footprint cache).
Expose no SQL surface to the frontend; the web UI reads results through typed Tauri commands.
Optionally use `tauri-plugin-store` for small app settings (window prefs, thresholds, calibration factor) if a key/value store is cleaner than a settings table.

## Consequences

- Synchronous, typed access with no async/sqlx overhead and no SQL injection surface in the webview.
- Migrations are hand-managed in Rust (a `parse_epoch`/schema-version column already invalidates caches on schema change).
- If a future need arises to let the UI run ad-hoc queries, `tauri-plugin-sql` can be added alongside; not needed for v1.

## Options considered

- **tauri-plugin-sql (sqlx)** — convenient if the frontend queried directly and wants bundled migrations, but pushes SQL into the webview and adds async; rejected for the core.
- **tauri-plugin-store only** — fine for settings, insufficient for the relational usage/dedup model; used only as an optional settings companion.
- **rusqlite bundled, direct in core** — chosen.
