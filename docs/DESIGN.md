# skillmon — Design

The furnished plan.
Terminology is defined per context via [`../CONTEXT-MAP.md`](../CONTEXT-MAP.md) (the domain glossary is [`../src-tauri/CONTEXT.md`](../src-tauri/CONTEXT.md)); decisions are recorded in [`adr/`](./adr/).

## What skillmon is

skillmon is a cross-platform desktop utility that lives in the macOS menu bar and the Windows system tray.
It watches the skills installed for an AI coding agent, reports what each one costs in context tokens and how much token usage it is associated with, and lets you sort, disable, or uninstall them without leaving the tray.
The first and only shipping adapter targets Claude Code; everything Claude-specific sits behind a harness-adapter boundary so other agents can be added later.

The product answers two questions a heavy skill user cannot answer today.
"Which skills are quietly taxing my context on every request?" is deterministic and trustworthy.
"Which skills am I actually burning tokens around?" is an honest estimate that must be visibly framed as one.

## Architecture

skillmon is a Tauri v2 app: a Rust core holds all domain logic, and a web UI renders the tray dropdown/flyout.

- **Rust core** — skill discovery, footprint counting, transcript parsing and usage attribution, mutation operations (disable/uninstall/plugin removal), persistence, file-watching (registry surfaces drive rescans, content hash drives recompute, transcript freshness is checked lazily on panel open — ADR 0019), and threshold evaluation for toasts.
- **Web UI (Svelte + TypeScript)** — the menu-bar/tray panel: the installed-skill list, footprint and usage columns, the ascending/descending sort control, and the disable/uninstall actions.
- **Harness-adapter trait** — a Rust trait that abstracts everything agent-specific: where skills live, how to read a skill's footprint, how to parse transcripts, and how to mutate enable/disable state. v1 ships exactly one implementation, the Claude Code adapter.
- **Data layer** — SQLite via `rusqlite` (bundled) inside the core for typed synchronous access, plus three persisted stores that keep a warm scan cheap and correct: a content-hash → token-count cache (tagged exact or estimated, per ADR 0006) so footprint counting is offline in steady state; a per-transcript `(mtime, size)` → listing-bullets memo (ADR 0022) so a warm rescan skips re-reading unchanged transcripts; and a global `message.id`-keyed attributed-usage table (ADR 0024) written INSERT OR IGNORE so usage dedups across resume/compact copies and re-reads stay idempotent, carrying a per-message `timestamp` (ADR 0025) so the same table answers both all-time and rolling-24h queries, with the budget config and its debounce flag in a sibling `usage_meta` table that survives a schema-version wipe.

## The domain: Claude Code skills

A skill is a `SKILL.md` file that Claude Code can load into context and invoke as a tool.
Skills reach a machine three ways, and the distinction drives the whole UI.

- **Personal skills** — `~/.claude/skills/<name>/SKILL.md`, discovered depth-1 only (immediate children of the scan root). There is no native enable/disable flag for these.
- **Project skills** — `<repo>/.claude/skills/<name>/SKILL.md`, scoped to one repository and only co-resident in context when you are working in that repo.
- **Plugin skills** — installed at the path recorded in the plugin's `installed_plugins.json` record (`installPath`), never reconstructed from `cache/<marketplace>/<plugin>/<version>/` — a version directory can literally be named `unknown`. Plugin-locked: you cannot remove one skill, only the whole plugin. A plugin's own `plugin.json` may relocate its skills dir (for example `./.claude/skills`), so lock detection must read that field, not assume `skills/`.

Plugin state is registry-driven, and enablement is repo-scoped, not a single global flag (ADR 0015).

- `~/.claude/plugins/installed_plugins.json` (schema `version: 2`) maps `"<plugin>@<marketplace>"` to an **array** of install records (scope, installPath, version, installedAt, gitCommitSha) — one plugin can be installed at more than one scope. `scope` here is install provenance; every scope's files live in the same shared cache, there is no repo-local cache directory.
- A plugin is **live** if `enabledPlugins["<plugin>@<marketplace>"]` is `true` in any of: `~/.claude/settings.json` (global), the active repo's `<repo>/.claude/settings.json` (project, git-committed), or the active repo's `<repo>/.claude/settings.local.json` (local, gitignored). Project- and local-scoped entries are gated by the same "active repo" concept as project skills. A plugin that is live nowhere applicable contributes zero live footprint.
- Marketplaces live in `known_marketplaces.json` and, for user-added ones, `settings.json → extraKnownMarketplaces`. The two registries are not 1:1, and built-in marketplaces must never be removed.

## The two metrics

**Context footprint** (headline) is the token size of a skill's content as it enters context — exact when the user has supplied their own Anthropic API key, a clearly labeled calibrated estimate otherwise (see below; ADR 0006).
It splits into three layers, each measuring what actually enters context rather than assuming raw source bytes (ADR 0016, ADR 0017): always-on (the rendered skill-listing line Claude Code actually injects — read from a live transcript when one exists, since it can include more than the frontmatter `name` + `description`, e.g. a plugin's own custom decorations; reconstructed from raw frontmatter only for a skill no transcript has ever included), on-invoke (`"Base directory for this skill: {path}\n\n" + body`, a confirmed stable template computed directly, loaded once when the skill fires), and on-demand (bundled references, loaded only if the body tells Claude to read them, measured as raw bytes and shown as a ceiling — deliberately not chasing which tool-specific wrapper an agent might read them through — never folded into the headline).
There is no exact offline Claude tokenizer for current models, and the only exact method — the `count_tokens` REST endpoint — requires a real Anthropic API key; Claude Code's own OAuth credential can never be reused for this, since Anthropic's Consumer Terms of Service restrict it to Claude Code and claude.ai (ADR 0006).
So the exact number, cached by content hash plus model id, is available only to users who add a key; everyone else's headline is a calibrated `tiktoken` estimate, which is the real default, not a rare fallback, and is always labeled as such.

**Attributed session usage** (secondary, fuzzy) is the tokens spent in sessions while a skill was holding the wheel.
Claude Code already computes this natively — main-thread `assistant` records carry `attributionSkill` and `attributionPlugin` — so skillmon trusts those fields where present and reconstructs a skill stack only where they are absent (sub-agent files, pre-attribution builds).
This is a proxy for causation, not a bill: the tokens are dominated by whatever task the user was doing while the skill was open, so it is labeled "tokens during this skill," never "tokens used by it."
Usage is split into work tokens (`input + output`, the headline), cache-write, and cache-read (which dominates 10–100× and is shown separately as "context tax / mostly cached").
The issue #5 MVP shipped native-first only, over main-thread transcripts, as cumulative all-time per-skill usage.
Issue #14 (ADR 0025) then added the rolling-24h window of UX decision #4: a per-message `timestamp` column drives windowed queries, so the panel can show the last 24h (opt-in; the default view stays all-time), and a fixed-24h attributed-work budget fires a once-per-crossing toast that debounces via a persisted flag.
Per-skill anomaly toasts ship alongside it, off by default.
The `parentUuid` reconstruction walk and the sub-agent include toggle are named follow-ups (ADR 0005 Update), consistent with the #2/#3 scope splits.

## Key data sources (under `~/.claude`, mirrored per-repo under `<repo>/.claude`)

- `skills/<name>/SKILL.md` — personal skills (many are symlinks managed by other tools such as gstack and `.agents`).
- `plugins/cache/<marketplace>/<plugin>/<version>/…` — plugin skills; `.in_use/<pid>` files reference-count live sessions and gate safe deletion.
- `plugins/installed_plugins.json`, `settings.json`, `plugins/known_marketplaces.json` — the mutation surface.
- `projects/<encoded-cwd>/<session-uuid>.jsonl` — transcripts (append-only JSONL); sub-agents write separate `…/<session-uuid>/subagents/agent-<id>.jsonl` files that carry `isSidechain: true` and inherit the parent session id but never a native `attributionSkill`.

Transcripts are parsed incrementally: each file carries a byte-offset checkpoint so a refresh reads only appended bytes, and usage rows are deduped by `message.id` because resume/branch/compact copy prior history into new files (summing by record `uuid` overcounts tokens up to 11×).

## Mutation model

Plugin operations go through the `claude plugin {disable,enable,uninstall,marketplace remove}` CLI — the format-stable interface for the evolving `version: 2` JSON — with an atomic JSON rewrite as the documented fallback.
Personal skills have no native toggle, so skillmon supplies its own reversible one: quarantine the skill directory out of the depth-1 scan root into `~/.claude/skillmon/disabled/…` (recording origin, including any symlink target), and delete via a two-phase move to `skillmon/trash/` then purge.
Every mutation applies to new sessions only, because enablement is read at session start, so the UI surfaces a "restart Claude Code to apply" nudge after any change.

## Cross-platform parity (macOS ↔ Windows)

| Concern | macOS | Windows |
| --- | --- | --- |
| Host surface | Menu bar status item (`Accessory` activation policy hides the Dock icon) | System tray / notification area; first-run coach mark to pin the icon out of overflow |
| Panel | Borderless vibrancy (`Effect::Popover`) window anchored under the menu-bar item | Borderless Mica (Win 11) / acrylic (Win 10) flyout anchored above the tray |
| Click model | Left-click opens the panel | Left-click opens the panel (`show_menu_on_left_click(true)`); right-click → context menu |
| Global hotkeys | `RegisterEventHotKey` via `tauri-plugin-global-shortcut` | `RegisterHotKey` via the same plugin |
| Toasts | User Notifications | Windows toast (requires a valid AppUserModelID / Start-menu shortcut) |
| Autostart | LaunchAgent | Registry Run key |
| Plugin-locked prompt | Native alert | Native TaskDialog |

A true `NSPopover` arrow and a system-owned Windows 11 flyout need custom native code and are deferred past v1 (see ADR 0004).

## UX decisions (resolved)

Settled in the grilling pass.

1. **Footprint display** — every row shows the full three-layer breakdown: always-on, on-invoke, and on-demand. No single blended number hides where the cost lives.
2. **Sort & grouping** — default sort is always-on footprint descending; every layer column is click-to-sort. Flat list, plugin shown as a badge with an opt-in "group by plugin" toggle.
3. **Attributed-usage labeling** — `~` prefix, muted, rendered below the footprint; tooltip "session tokens during this skill, not by it"; cache-read excluded from the number. Sub-agent tokens excluded by default, with an include toggle.
4. **Toast model** — one global rolling-24h budget on attributed usage, on by default. Per-skill anomaly alerts (a skill running N× its trailing average) available but off by default. The toast names the metric as an estimate.
5. **Project skills** — every repo's project skills are listed as collapsed per-repo inventory sections. The global always-on total counts personal + enabled-plugin skills + the **active repo's** project skills only (what is actually co-resident now); other repos' project skills are shown but not summed.
6. **Uninstall history** — tombstone: the skill leaves the active list, its history is retained under a "removed" view, and reinstalling restores continuity.
7. **Onboarding / empty state** — the empty state names the exact scanned paths (`~/.claude/skills`, plugin cache, active repo) with a rescan button; Windows shows a one-time "drag the icon onto the taskbar to pin it" coach mark.
8. **Dollar cost** — tokens only. No dollar figures anywhere; footprint is not a recurring bill and usage dollars would be an estimate on an estimate.
9. **API-key settings** — a full-panel view-swap behind a topbar gear (ADR 0023). Saving a key validates it with one `count_tokens` probe, so a rejected key is reported rather than silently yielding estimates, and a rejected key is never stored. Removing a key stops new exact counts but leaves already-computed ones exact until their skill content changes. Only the key's presence, never its value, crosses to the UI.
