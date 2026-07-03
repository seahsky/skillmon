# skillmon

Cross-platform macOS menu-bar / Windows system-tray app that monitors Claude Code skills and plugins — context footprint, attributed usage, sort, disable/uninstall, plugin-lock-aware removal.
Stack: Tauri v2 — a Rust core holds all domain logic, a Svelte + TypeScript web UI renders the tray panel.

Ubiquitous language is multi-context: @CONTEXT-MAP.md points to one glossary per context (`src-tauri/CONTEXT.md` is the domain). Use those exact terms.
Architecture, data sources, and resolved decisions are in @docs/DESIGN.md and `docs/adr/`.
This file is a starting point; grow it a line at a time as the code lands and as agents get things wrong.

## Status

Rust core is well underway; no UI yet. Landed in the core: skill/plugin discovery, the three-layer footprint counter (content-hash SQLite cache, `tiktoken` estimate, optional exact `count_tokens` via a keychain-stored API key), the `HarnessAdapter` trait, a debounced registry file-watcher (ADR 0019), harness-neutral scan orchestration (`scan_all`, ADR 0021), and the `list_skills` Tauri command. The default `greet` command + demo page are still present. Requires Rust ≥ 1.89 (the notification plugin's `notify-rust` dep).

**Next up** is tracked as GitHub issues (`seahsky/skillmon`, label `ready-for-agent`):

- [#1](https://github.com/seahsky/skillmon/issues/1) — tray panel rendering `list_skills` (the keystone; folds in removing the `greet` demo)
- [#2](https://github.com/seahsky/skillmon/issues/2) — cut the ~120s cold tokenizer cost (re-validates ADR 0006)
- [#3](https://github.com/seahsky/skillmon/issues/3) — cut the ~7s warm per-scan cost (incremental index)
- [#4](https://github.com/seahsky/skillmon/issues/4) — API-key settings UI + set/delete commands (blocked by #1)
- [#5](https://github.com/seahsky/skillmon/issues/5) — attributed usage, ADR 0005 (blocked by #1)

The post-mutation "restart Claude Code to apply" nudge is deferred until the disable/uninstall mutation ops are scoped.

## Commands

- Dev: `pnpm tauri dev` (launches the tray app)
- Build frontend only: `pnpm build`; typecheck: `pnpm check`
- Build/bundle app: `pnpm tauri build`
- Rust core: `cargo build --manifest-path src-tauri/Cargo.toml`; tests: `cargo test --manifest-path src-tauri/Cargo.toml` (one: append `<name> -- --exact`)
- No JS test runner yet — add one (Vitest) when the UI grows logic worth testing.

## Structure

- `src-tauri/` — Rust core (Cargo crate `skillmon`, lib `skillmon_lib`): `src/lib.rs` is the entry point. Will hold the harness adapter, skill discovery, footprint counter, transcript attribution, mutation ops, `rusqlite` persistence, file watcher. Capabilities in `src-tauri/capabilities/`.
- `src/` — SvelteKit-TS frontend (adapter-static, SPA): `src/routes/` pages, `src/app.html`. Becomes the tray panel: skill list, three-layer footprint columns, usage column, sort/group, disable/uninstall.
- `CONTEXT-MAP.md` → `src-tauri/CONTEXT.md` glossary · `docs/DESIGN.md` design · `docs/adr/` decisions (0001–0021).

## Project rules

- **Adapter boundary (ADR 0002).** Every Claude-Code-specific path, file format, or CLI call lives inside the harness adapter — never in the UI or the generic core. A new fact about `~/.claude` layout goes in the adapter.
- **Two honest metrics, never blended (ADR 0003).** Footprint is exact (from `count_tokens`); attributed usage is an estimate. Render usage with a `~`, demoted, labeled "tokens during this skill" — never as an exact figure or a bill. No dollar values anywhere.
- **Mutations are reversible (ADR 0007).** Disable = quarantine move; uninstall = two-phase trash then purge; never hard-delete a skill dir. For plugins prefer the `claude plugin` CLI; snapshot before any direct JSON edit; respect `.in_use/<pid>` before deleting a plugin cache dir. Show the "restart Claude Code to apply" nudge after any change.
- **Token counting (ADR 0005/0006).** Dedup transcript token rows by `message.id`, never by record `uuid` (overcounts up to 11×). Parse transcripts incrementally via byte-offset checkpoints. Cache footprint by content hash (`model_id` is a staleness column, not part of the key — ADR 0006/0018); trust native `attributionSkill`/`attributionPlugin` before reconstructing.
- **Discovery.** Personal skills are discovered depth-1 only; many entries are symlinks managed by other tools — detect and record the target, don't assume skillmon owns them.
- **No superpowers skills in this project.** The `superpowers` plugin is disabled for this repo in `.claude/settings.json`; do not invoke any `superpowers:*` skill here. Use Matt Pocock's skills for planning and domain modeling instead. Decisions are recorded as ADRs (`docs/adr/`); pending work is tracked as GitHub issues (label `ready-for-agent`), not in standalone plan files.

## Verification

A change is done when `cargo build`/`cargo test` and `pnpm check` pass **and** the affected flow is exercised against a real `~/.claude` fixture, not just unit tests: footprint matches `count_tokens`, and mutations round-trip (disable→enable, uninstall→restore). Once a JS test runner exists, `pnpm test` joins this gate.

## Agent skills

### Issue tracker

GitHub Issues via the `gh` CLI; PRs are not a triage surface.
Repo: `seahsky/skillmon` (public); the five triage labels exist.
See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical roles, default strings (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`).
See `docs/agents/triage-labels.md`.

### Domain docs

Multi-context: `CONTEXT-MAP.md` at the root points to one `CONTEXT.md` per context (`domain` at `src-tauri/`, `ui` at `src/` lazily).
See `docs/agents/domain.md`.
