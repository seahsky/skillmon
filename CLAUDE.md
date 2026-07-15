# skillmon

Cross-platform macOS menu-bar / Windows system-tray app that monitors Claude Code skills and plugins — context footprint, attributed usage, sort, disable/uninstall, plugin-lock-aware removal.
Stack: Tauri v2 — a Rust core holds all domain logic, a Svelte + TypeScript web UI renders the tray panel.

Ubiquitous language is multi-context: @CONTEXT-MAP.md points to one glossary per context (`src-tauri/CONTEXT.md` is the domain). Use those exact terms.
Architecture, data sources, and resolved decisions are in @docs/DESIGN.md and `docs/adr/`.
This file is a starting point; grow it a line at a time as the code lands and as agents get things wrong.

## Status

Rust core is well underway; the tray panel renders the footprint and a demoted usage sub-line. Landed in the core: skill/plugin discovery, the three-layer footprint counter (content-hash SQLite cache, `bpe-openai` estimate, optional exact `count_tokens` via a keychain-stored API key), the `HarnessAdapter` trait, a debounced registry file-watcher (ADR 0019), harness-neutral scan orchestration (`scan_all`, ADR 0021), the `list_skills` Tauri command, an incremental listing-index memo (issue #3, ADR 0022), the API-key `set`/`delete` commands with validate-on-save (issue #4, ADR 0023), native-first attributed session usage (issue #5, ADR 0005/0024), and the rolling-24h usage window with a fixed-24h attributed-work toast budget and off-by-default per-skill anomaly toasts (issue #14, ADR 0025). The estimator swapped `tiktoken-rs` for a byte-identical, faster `bpe-openai` (issue #2, ADR 0006 update). The scan is threaded through an inherent `ClaudeCodeAdapter::scan(&ScanParams) -> ScanOutcome`; the trait `scan_all` is the clockless all-time shim over it, and "now" is injected only at the `lib.rs` command boundary. The tray panel (issue #1) renders `list_skills` as a read-only three-layer list with a demoted "tokens during this skill" sub-line, refreshes on `registry-changed`, has an All-time / Last-24h window toggle, and an API-key + usage-budget settings surface behind a gear. The read-only MVP shell then turned that window into a real macOS menu-bar dropdown — borderless/transparent 400×600 panel (`macOSPrivateApi`), tray left-click-release toggle anchored via `tauri_plugin_positioner` with blur-to-dismiss and a 200 ms double-toggle guard, a compiled-in monochrome template tray icon (raw RGBA via `Image::new` + `icon_as_template`), a `CommandOrControl+Shift+K` global hotkey, and a launch-at-login toggle (`tauri_plugin_autostart`) in settings — and finished the read-only surface: click-to-sort every layer column (a `null`/pending figure always sorts last), a group-by-plugin toggle, collapsed per-repo project sections with an active-repo-only always-on total (DESIGN #5), and an empty state that names the scanned paths; mutations, Windows, and signing stay explicitly out of MVP. Pure UI logic lives in `src/lib/skills.ts` with Vitest coverage. Requires Rust ≥ 1.89 (the notification plugin's `notify-rust` dep). The on-demand ceiling walk prunes what cannot enter context through the skill it is walking — `.git`, `node_modules`, and any nested `SKILL.md` subtree — and guards symlink cycles with a visited-canonical-path set (issue #26, ADR 0028); it was previously counting a skill-dir-that-is-a-checkout's entire tree, so any bundled-file byte figure measured before that fix (including the "~216 MB" quoted in ADR 0022) counted non-reference content.

The #2–#5 follow-ups all landed: on-demand ceiling deferral (#11), `parentUuid` reconstruction (#12), the sub-agent include toggle (#13), and `message_usage` pruning + the byte-offset tail-reader (#15, ADR 0024 — a vanished checkpoint triggers a dir-scoped conditional rebuild, never a targeted delete).
Still open from that wave: **#19**, the version-gated parent-Task hierarchical roll-up deferred out of #13.

**Next up** — the skill-source column and the removal ops (issues #27–#31, label `ready-for-agent`), designed in ADR 0026/0027 against a real 71-skill `~/.claude`:

- **Three bugs found while designing, all now fixed.** #24: 12 skills carry `disable-model-invocation: true`, so Claude Code drops them from the listing and charges zero always-on — skillmon didn't parse the field, so those rows fell to the `Reconstructed` path and were billed a cost that doesn't exist. #25: symlink detection lstat'd only the skill dir, so it caught 20 of 66 managed skills — the dominant form on disk (a real dir whose `SKILL.md` is a symlink) read as unmanaged, which `manager_root` fixes by resolving `SKILL.md` itself. #26: the on-demand walk had no exclusions, so gstack's ceiling swept 704 MB of `node_modules`, a 60 MB `.git`, and 46 nested skills (ADR 0028).
- **Source = manager root, not origin** (#30, ADR 0026). "Which is official / superpowers / mine" doesn't survive contact with a real `~/.claude`: superpowers is a plugin and already labeled, "official" means three incompatible things, and none of the 71 personal skills are self-authored. The axis that pays is *who will restore this if I remove it* — derived structurally, no tool manifests parsed.
- **Removal = entry removal** (#31, ADR 0027). skillmon removes the entry, never through it, so damaging a managing tool's content is impossible by construction. Enablers: #27 (`SkillReport` carries no path/id/symlink data), #28 (purge/tombstones — trashing gstack moves 1.1 GB, so DESIGN #6 is now a prerequisite), #29 (self-write suppression — the watcher is recursive on the skills dir).
- ADR 0027 amends ADR 0007 before any of it was built: quarantine and trash collapse into one reversible op with a retention intent, and a row with dependents is a *tool uninstall*, not a skill removal.
- Filed while verifying #26 against a real `~/.claude`: **#33**, plugin skills declared in `plugin.json` are invisible (skillmon finds 15 of ~55 — the manifest is read from the wrong path, `skills` is typed string-only when it is also an array of explicit paths, and the walk is depth-1).

The post-mutation "restart Claude Code to apply" nudge is deferred until the disable/uninstall mutation ops are scoped.

## Commands

- Dev: `pnpm tauri dev` (launches the tray app)
- Build frontend only: `pnpm build`; typecheck: `pnpm check`
- Build/bundle app: `pnpm tauri build`
- Rust core: `cargo build --manifest-path src-tauri/Cargo.toml`; tests: `cargo test --manifest-path src-tauri/Cargo.toml` (one: append `<name> -- --exact`)
- JS tests: `pnpm test` (Vitest, runs the pure `src/lib` modules via a standalone `vitest.config.ts`).

## Structure

- `src-tauri/` — Rust core (Cargo crate `skillmon`, lib `skillmon_lib`): `src/lib.rs` is the entry point. Will hold the harness adapter, skill discovery, footprint counter, transcript attribution, mutation ops, `rusqlite` persistence, file watcher. Capabilities in `src-tauri/capabilities/`.
- `src/` — SvelteKit-TS frontend (adapter-static, SPA): `src/routes/` pages, `src/app.html`. Becomes the tray panel: skill list, three-layer footprint columns, usage column, sort/group, disable/uninstall.
- `CONTEXT-MAP.md` → `src-tauri/CONTEXT.md` glossary · `docs/DESIGN.md` design · `docs/adr/` decisions (0001–0025).

## Project rules

- **Adapter boundary (ADR 0002).** Every Claude-Code-specific path, file format, or CLI call lives inside the harness adapter — never in the UI or the generic core. A new fact about `~/.claude` layout goes in the adapter.
- **Two honest metrics, never blended (ADR 0003).** Footprint is exact (from `count_tokens`); attributed usage is an estimate. Render usage with a `~`, demoted, labeled "tokens during this skill" — never as an exact figure or a bill. No dollar values anywhere.
- **Mutations are reversible (ADR 0007).** Disable = quarantine move; uninstall = two-phase trash then purge; never hard-delete a skill dir. For plugins prefer the `claude plugin` CLI; snapshot before any direct JSON edit; respect `.in_use/<pid>` before deleting a plugin cache dir. Show the "restart Claude Code to apply" nudge after any change.
- **Token counting (ADR 0005/0006).** Dedup transcript token rows by `message.id`, never by record `uuid` (overcounts up to 11×). Parse transcripts incrementally via byte-offset checkpoints. Cache footprint by content hash (`model_id` is a staleness column, not part of the key — ADR 0006/0018); trust native `attributionSkill`/`attributionPlugin` before reconstructing.
- **Discovery.** Personal skills are discovered depth-1 only; many entries are symlinks managed by other tools — detect and record the target, don't assume skillmon owns them.
- **No superpowers skills in this project.** The `superpowers` plugin is disabled for this repo in `.claude/settings.json`; do not invoke any `superpowers:*` skill here. Use Matt Pocock's skills for planning and domain modeling instead. Decisions are recorded as ADRs (`docs/adr/`); pending work is tracked as GitHub issues (label `ready-for-agent`), not in standalone plan files.

## Verification

A change is done when `cargo build`/`cargo test`, `pnpm check`, and `pnpm test` pass **and** the affected flow is exercised against a real `~/.claude` fixture, not just unit tests: footprint matches `count_tokens`, and mutations round-trip (disable→enable, uninstall→restore).

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
