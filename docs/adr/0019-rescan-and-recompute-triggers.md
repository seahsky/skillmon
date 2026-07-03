# 19. Rescan is filesystem-watch driven; footprint recompute is hash-driven; transcript freshness is lazy

## Context

DESIGN.md lists file-watching as a Rust core responsibility without saying what it watches or what it triggers.
Two different things need a trigger, and they don't have the same freshness requirements: noticing that a skill or plugin was added/removed/toggled, and noticing that a skill's content changed and needs re-tokenizing.
A third, related question: the always-on layer's native-vs-reconstructed upgrade (ADR 0016) depends on a transcript existing, and transcripts are appended far more often than skills or plugins actually change.

## Decision

**Discovery rescan** is driven by watching the registry surfaces directly — `~/.claude/skills/`, each known repo's `.claude/skills/`, `installed_plugins.json`, and the three `enabledPlugins` settings files (global, active repo's `settings.json`, active repo's `settings.local.json`) — debounced (~500ms) so a multi-file operation like a plugin install triggers one rescan, not several.

**Footprint recompute** is not a separate trigger at all: it rides the same rescan, and is purely content-hash-driven — a skill whose hash is unchanged since last time reuses its cached count, only a changed or newly-discovered hash gets re-tokenized.

**Transcript freshness** (upgrading a skill's always-on text from reconstructed to native once a real transcript includes it) is deliberately *not* wired to continuous watching of `~/.claude/projects/**/*.jsonl`. It is checked lazily, whenever the tray panel is actually opened. A reconstructed number is already a reasonable placeholder in the interim, and transcripts change too often relative to skills to justify continuous watching for a confidence upgrade rather than a correctness fix.

A manual "rescan" button (already implied by the empty-state onboarding decision) remains the explicit escape hatch regardless of what any watcher misses.

## Consequences

- The registry watcher's debounce window and exact watched-path set live in the harness adapter (Claude-Code-specific paths), not generic core (ADR 0002).
- A newly-installed skill's always-on number may show as `reconstructed` for a while if the user doesn't reopen the panel after their next Claude Code session — acceptable, since it's a confidence label, not a wrong number.
- Adding closer-to-real-time transcript-driven updates later (e.g. if attributed-usage toasts need it) is a scoped, additive change to this ADR's third leg, not a rearchitecture of the other two.

## Options considered

- **Watch everything continuously, including all transcripts** — simplest mental model, but burns resources reacting to the highest-churn, lowest-urgency data source for a purpose (confidence upgrade) that doesn't need real-time freshness; rejected.
- **Poll everything on a fixed interval, no filesystem watching at all** — simpler to implement but either too slow (long interval) or wasteful (short interval) compared to reacting to real changes; rejected in favor of watching the low-churn registry surfaces and lazily checking the high-churn one.
- **Watch registry surfaces for rescan, hash-drive recompute, lazy on-open transcript freshness check** — chosen.

## Update (grilling the footprint counter plan)

This ADR's recompute trigger — content-hash-driven, riding the registry rescan — has a gap ADR 0018 didn't account for: a skill's content hash doesn't change when skillmon bumps its own internal reference model. Nothing in this ADR's trigger set fires on that event, so a skill whose exact count was measured against a superseded model would keep showing that stale number, still labeled `Exact`, indefinitely — self-healing only if the skill's own content happens to change for an unrelated reason.

A fourth trigger is needed: **reference-model change**, detected by comparing the cache's stored `exact_model_id` per row against skillmon's current `REFERENCE_MODEL_ID` constant, exposed as `TokenCache::stale_exact_hashes(current_model_id) -> Vec<String>` (`src-tauri/src/footprint/cache.rs`). This is a query primitive only, not a scan-and-recompute loop — recomputing requires re-deriving each affected skill's source text (the cache stores hashes, not the original text), which means routing back through `compute_footprint` per skill, which in turn needs the registry rescan / Tauri command layer this ADR already defers as later work. Wiring `stale_exact_hashes` into an actual startup or rescan check is that same later work's responsibility, not this update's.

- **Do nothing; rely on eventual content edits to self-heal stale exact counts** — silently wrong for however long a skill goes unedited after a model bump; rejected, contradicts ADR 0018's stated consequence that a model change "should recount in the background."
- **Build the full sweep-and-recompute now, even with no caller** — the rescan loop and Tauri command layer don't exist yet, so this would be dead code exercised only by its own tests; rejected as premature (YAGNI) until the loop it plugs into exists.
- **Add the detection primitive now, wire the recompute sweep in whenever the rescan loop lands** — chosen.

## Update 2 (the rescan loop landed)

The rescan loop and Tauri command layer this update deferred to now exist, and the reference-model check is wired — but as a startup **observability signal**, not a recompute engine. `ClaudeCodeAdapter::stale_exact_count()` (built on `stale_exact_hashes`) is logged when the app constructs its adapter, so a reference-model bump is visible rather than silent. No dedicated recompute sweep was needed after all: a scan already re-counts any cached exact whose `model_id` differs from the current reference model (`count_text` declines to trust a stale-model exact), so the ordinary scan path *is* the self-heal. The count only reports how many rows predate the current model. This is why the "wire the recompute sweep in later" option above was satisfied without building a separate loop (ADR 0021 covers where the scan orchestration itself lives).
