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

## Update 3 (the incremental listing memo keeps transcript freshness lazy)

The incremental listing index (ADR 0022) does not change this ADR's stance that transcripts are deliberately *not* watched.
Freshness stays lazy, evaluated on the next `scan_all` (a panel reopen or a registry-change rescan); the persisted `(mtime, size)` memo only makes that lazy check cheap by skipping the re-read of unchanged transcripts.
No continuous transcript watcher is added, so the third leg of the rescan model is intact.

## Update 4 (a rescan trigger excludes skillmon's own writes)

This ADR's first leg watches the personal skills dir **recursively**, and never asked who was writing.
That was free while skillmon only read, and stops being free the moment it mutates: ADR 0027's removals move entries *out of* that root and restores move them back *in*, so skillmon's own mutations trigger rescans of skillmon's own work.
A tool uninstall moves 47 entries, and the debouncer flushes mid-cascade, while the ledger recording the unit has not committed.
The panel renders a tree that is genuinely half-removed at that instant (issue #29).

**A watcher event is suppressed while skillmon is writing, and the mutation announces its own completion instead.**
A mutation holds a `SelfWriteWindow` guard (`src-tauri/src/self_write.rs`) across its writes; the debouncer callback drops any batch arriving while that latch is open; the mutation emits `registry-changed` itself once its ledger write has settled.
The result is one rescan of a finished tree rather than N of an unfinished one.
`purge`/`empty_trash` need neither, and that is not an oversight: they only ever touch `skillmon/removed/`, which no watcher watches.

Two things about the mechanism are load-bearing and non-obvious.

**Suppression must outlive the write.**
A guard is released when `rename(2)` returns, but the event that rename raised has not been *delivered* yet: the debouncer holds it for a further `DEBOUNCE_WINDOW`.
A latch that closed with the write would therefore suppress nothing at all, every self-write event landing just after it closed.
Hence a tail, derived from the debounce (`SELF_WRITE_TAIL = DEBOUNCE_WINDOW * 2`) rather than picked, so the two cannot drift.

**The staging root is outside every watched tree, and that is a separate fact from this one.**
`~/.claude/skillmon/removed/` sits under `~/.claude` but not under `~/.claude/skills` (`paths::removed_dir`, asserted by `the_removal_staging_root_sits_outside_every_recursively_watched_tree`), so the *destination* of a removal is not self-triggering.
It answers only half of issue #29, since taking the entry out of the scan root is still a write inside a watched tree, which is why the latch exists as well.

### Consequences

**An external change landing inside the window is attributed to skillmon and dropped.**
This is the real price, stated plainly rather than buried: the latch knows only that skillmon was writing, never which write an event came from.

**And it drops that batch across every watched surface, not merely the one being written.**
All the watched paths share one debouncer and one callback, which consults the latch once per batch, so a removal in `~/.claude/skills` also swallows a concurrent `installed_plugins.json` event it could in principle have attributed exactly.
The blast radius is therefore wider than "the tree the mutation touched", and is recorded here rather than left to be discovered.
It is accepted because it changes nothing in practice: the rescan the mutation itself emits reads *all* the surfaces fresh, so a plugin install landing during the mutation is picked up regardless of whose event announced it.

The exposure is a sub-second tail after a mutation the user themselves just triggered, and three things make it proportionate.
ADR 0027 measured that neither managing tool runs a daemon, so nothing races skillmon in the background; a collision needs the user to run `/gstack-upgrade` in the same second they clicked uninstall.
A change landing *during* the mutation is caught anyway by the rescan the mutation itself emits, which reads the whole tree fresh, leaving only the tail after it exposed.
And this ADR already holds that the watcher is a "your list may be stale" nudge rather than a live-state mirror, with a lazy check on panel open and a manual rescan button as the escape hatch, so the failure mode is one this design has always accepted.

**A guard brackets the writes, never the wait for a lock.**
Taking it before the store mutex would look more cautious and be strictly worse: that wait is unbounded (an `Empty trash` holds the store for as long as deleting 1.1 GB takes), and every second of it would suppress the watcher while the queued command writes nothing.
A mutation that is already writing holds its own guard, so it never needs a queued one's.

The latch is deliberately **not** the watcher's `Mutex` to acquire: a mutation must never queue behind a scan's `sync_watcher` merely to say "this write is mine".
It has a lock of its own, which the notify callback does take, but only ever to read or stamp one `Instant`, never across a scan, a move, or any other real work.

## Options considered (Update 4)

- **Per-path attribution**: register the paths a mutation is about to touch and drop only their events. Finer in principle, and it would not swallow a concurrent external change. Rejected as unreliable rather than merely costly: moving `skills/foo` out also fires an event on `skills/` itself, and that directory is the parent of *every* skill, so suppressing it swallows exactly the changes the watcher exists for, while not suppressing it lets every self-write through the parent's event. The existing callback also resolves no paths at all (it reacts to "any event in the batch"), so path attribution would be a larger change than the problem warrants, sold on a precision it cannot deliver.
- **Coalesce rather than drop**: remember that events were suppressed and flush one rescan when the window expires, losing nothing. Rejected: the flush needs a timer thread, because the last event of a mutation arrives *after* the mutation has returned and there is nothing left to fire it. That is real machinery to close a sub-second hole that a panel reopen already closes, and it is the same over-engineering ADR 0027 rejected when it declined desired-state reconciliation on the same measured evidence.
- **Have mutations stop the watcher and restart it**, with no latch and no tail. Rejected: `unwatch`/`watch` cycles drop events genuinely unrelated to the mutation for the whole duration (a strictly wider blind spot than the tail), and a mutation that panicked between the two would leave the watcher permanently off, where a dropped RAII guard cannot.
- **A latch held across the write, with a tail sized from the debounce**: chosen.
