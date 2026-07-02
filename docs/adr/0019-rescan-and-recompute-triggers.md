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
