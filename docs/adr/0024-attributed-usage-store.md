# 24. Attributed-usage store: a global message-id table, INSERT OR IGNORE, GROUP BY totals

## Status

Accepted.

## Context

Issue #5 adds attributed session usage (ADR 0005): per-skill token totals summed from transcript `assistant` records.
Unlike the always-on listing (ADR 0022), which is an idempotent per-file snapshot, usage SUMS, and two forces make a naive per-file memo wrong:

- resume, branch, and compact copy the same `message.id` into different transcript files, so summing per-file aggregates double-counts shared messages (the up-to-11x overcount ADR 0005 warns about). A `(path, mtime, size)` memo like `SqliteListingCache` cannot dedup across files.
- one logical message is split across content blocks (thinking, text, tool_use) into several records sharing one `message.id`, so summing per-record double-counts within a file too.

## Decision

Two persisted jobs, split so dedup is a database constraint, not emergent behaviour, in a dedicated `usage.sqlite` beside the footprint and listing caches (Claude-Code-specific, so it lives in the adapter, ADR 0002).

- **Correctness: a global `message_usage(message_id PRIMARY KEY, attribution_skill, attribution_plugin, is_subagent, work, cache_write, cache_read)` table, written INSERT OR IGNORE.**
Because the key is `message.id`, a message copied into a second file is ignored on its second sighting, so both the intra-file split and the cross-file copy collapse to one count.
Per-skill totals are ALWAYS re-derived by `SELECT ... SUM(...) GROUP BY attribution_skill, attribution_plugin WHERE is_subagent = 0`, never stored as running aggregates, so a dedup can never leave a stale sum behind.
`work = input + output` only; `cache_read` is a separate bucket and is never in the headline (it dominates 10-100x).
- **Performance: a `usage_checkpoint(path, mtime_nanos, size, logic_version)` table** decides whether a file is opened at all, on the same strict-equality `(mtime, size)` gate as ADR 0022 (an older mtime is still a miss, so a same-size backwards-clock rewrite re-reads).
A warm rescan reads zero files and the GROUP BY returns byte-identical totals; the checkpoint is a pure perf layer, not the dedup mechanism.
- **A byte-offset tail-reader is deferred.** INSERT OR IGNORE makes a whole-file re-read of a changed file idempotent, so a tail-reader only saves re-reading the single active file, and it fights compaction rewrites (a shrink-and-replace is not an append). Not worth it for the MVP.

## Consequences

- **A logic-version bump WIPES both tables and rebuilds.** This is the load-bearing divergence from `SqliteListingCache`: its per-path `put` OVERWRITES, so a re-read naturally replaces a row, but `message_usage` uses INSERT OR IGNORE and can never overwrite a stale row. A `usage_meta(logic_version)` row is checked on open; on mismatch, `DELETE FROM message_usage; DELETE FROM usage_checkpoint`. Anyone who clones the listing-cache pattern and assumes a re-read refreshes rows is wrong here.
- Totals are cumulative all-time and grow monotonically (the message table is never pruned on file deletion in the MVP; `retain` prunes only the lightweight checkpoint table). Bounded at roughly the number of unique attributed assistant messages (~70k on the author's machine); a rolling-window and pruning are separate deferred slices, as is the DESIGN.md rolling-24h toast budget.
- Partial-line safety: a file is `mark`ed only after a successful whole-file parse, so a truncated trailing line (serde fails) is skipped and re-read next scan when complete, never counted.
- A same-name personal skill and active-repo project skill both derive `(None, name)` (attribution carries no repo), so they would show the same figure. Near-zero in practice and documented as a limitation; a `cwd`-based disambiguation is a bounded later fix.

## Options considered

- **A `(path, mtime, size)` per-file memo like ADR 0022** — rejected: correct for an idempotent snapshot, wrong for a sum, because it cannot dedup a `message.id` shared across files.
- **A persisted running per-skill aggregate** — rejected: cannot self-correct once a dedup or a logic change invalidates a contribution; the GROUP BY is cheap and always correct.
- **A byte-offset checkpoint as the dedup mechanism** — rejected: an offset dedups within a file but not the cross-file copies; the message-id primary key is the actual dedup.
