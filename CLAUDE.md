# skillmon

Cross-platform macOS menu-bar / Windows system-tray app that monitors Claude Code skills and plugins — context footprint, attributed usage, sort, disable/uninstall, plugin-lock-aware removal.
Stack: Tauri v2 — a Rust core holds all domain logic, a Svelte + TypeScript web UI renders the tray panel.

Ubiquitous language is multi-context: @CONTEXT-MAP.md points to one glossary per context (`src-tauri/CONTEXT.md` is the domain). Use those exact terms.
Architecture, data sources, and resolved decisions are in @docs/DESIGN.md and `docs/adr/`.
This file is a starting point; grow it a line at a time as the code lands and as agents get things wrong.

## Status

Planning complete, no source yet.
Everything under Commands and Structure is **planned** until the Tauri project is scaffolded — don't treat these paths as existing.

## Commands (planned)

- Dev: `pnpm tauri dev`
- Build: `pnpm tauri build`
- Rust core tests: `cargo test` in `src-tauri/`; one test: `cargo test <name> -- --exact`
- Frontend tests: `pnpm test` (Vitest); one file: `pnpm test <file>`

## Structure (planned)

- `src-tauri/` — Rust core: harness adapter, skill discovery, footprint counter, transcript attribution, mutation ops, `rusqlite` persistence, file watcher.
- `src/` — Svelte + TS tray panel: skill list, three-layer footprint columns, usage column, sort/group, disable/uninstall actions.
- `CONTEXT-MAP.md` → `src-tauri/CONTEXT.md` glossary · `docs/DESIGN.md` design · `docs/adr/` decisions.

## Project rules

- **Adapter boundary (ADR 0002).** Every Claude-Code-specific path, file format, or CLI call lives inside the harness adapter — never in the UI or the generic core. A new fact about `~/.claude` layout goes in the adapter.
- **Two honest metrics, never blended (ADR 0003).** Footprint is exact (from `count_tokens`); attributed usage is an estimate. Render usage with a `~`, demoted, labeled "tokens during this skill" — never as an exact figure or a bill. No dollar values anywhere.
- **Mutations are reversible (ADR 0007).** Disable = quarantine move; uninstall = two-phase trash then purge; never hard-delete a skill dir. For plugins prefer the `claude plugin` CLI; snapshot before any direct JSON edit; respect `.in_use/<pid>` before deleting a plugin cache dir. Show the "restart Claude Code to apply" nudge after any change.
- **Token counting (ADR 0005/0006).** Dedup transcript token rows by `message.id`, never by record `uuid` (overcounts up to 11×). Parse transcripts incrementally via byte-offset checkpoints. Cache footprint by `(content hash, model_id)`; trust native `attributionSkill`/`attributionPlugin` before reconstructing.
- **Discovery.** Personal skills are discovered depth-1 only; many entries are symlinks managed by other tools — detect and record the target, don't assume skillmon owns them.

## Verification (planned)

A change is done when `cargo test` and `pnpm test` pass **and** the affected flow is exercised against a real `~/.claude` fixture, not just unit tests: footprint matches `count_tokens`, and mutations round-trip (disable→enable, uninstall→restore).

## Agent skills

### Issue tracker

GitHub Issues via the `gh` CLI; PRs are not a triage surface.
No git remote exists yet — create the GitHub repo and add the remote before the `gh`-based skills can run.
See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical roles, default strings (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`).
See `docs/agents/triage-labels.md`.

### Domain docs

Multi-context: `CONTEXT-MAP.md` at the root points to one `CONTEXT.md` per context (`domain` at `src-tauri/`, `ui` at `src/` lazily).
See `docs/agents/domain.md`.
