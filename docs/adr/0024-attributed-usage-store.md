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
- **Performance: a `usage_checkpoint(path, mtime_nanos, size, byte_offset, logic_version)` table** decides whether a file is opened at all, on the same strict-equality `(mtime, size)` gate as ADR 0022 (an older mtime is still a miss, so a same-size backwards-clock rewrite re-reads).
A warm rescan reads zero files and the GROUP BY returns byte-identical totals; the checkpoint is a pure perf layer, not the dedup mechanism.
- **A byte-offset tail-reader reads only appended bytes (issue #15).** `read_plan` maps a freshly-stat'd `(mtime, size)` to `Skip` (unchanged), `Tail(offset)` (grew, non-zero offset), or `Full` (new file, shrink, same-size in-place rewrite, version mismatch, or a legacy zero offset). `byte_offset` is the position just past the last newline parsed, so a partial trailing line is never skipped; a tail seeks to `offset - 1`, confirms that boundary byte is a newline (else it falls back to `Full` — the prefix was rewritten), and parses `[offset..EOF]` up to its own last newline.
This is safe because the tail-reader leans on the DESIGN.md **append-only-JSONL invariant**: transcripts grow at the end, and resume/compact write a NEW file rather than editing one in place.
Even when that is violated, conservative detection can never overcount: `ingest` is INSERT OR IGNORE on the immutable `message.id` (the API fixes a message's usage at generation; compaction copies it verbatim), so a rewrite mistaken for an append re-ingests already-present ids as no-ops and reads the genuinely-new tail ids, and any doubt resolves to `Full`, which is idempotent. No prefix hash is needed for correctness.
Adding `byte_offset` needs no logic-version bump: it changes WHICH bytes a scan reads, not how a row derives from bytes. New DBs get the column in `CREATE`; an existing DB gets a guarded `ALTER TABLE ... ADD COLUMN byte_offset INTEGER NOT NULL DEFAULT 0`, preserving all history. Legacy rows default to offset 0, forcing one `Full` re-read on their next growth (self-correcting). Seeding legacy rows to `size` instead would in fact also be safe — a legacy file that ended mid-partial-line has a non-newline boundary byte, so the tail's boundary check forces `Full` and the completed record is never skipped — but offset 0 is the simpler default and is kept for KISS.

## Consequences

- **A logic-version bump WIPES both tables and rebuilds.** This is the load-bearing divergence from `SqliteListingCache`: its per-path `put` OVERWRITES, so a re-read naturally replaces a row, but `message_usage` uses INSERT OR IGNORE and can never overwrite a stale row. A `usage_meta(logic_version)` row is checked on open; on mismatch, `DELETE FROM message_usage; DELETE FROM usage_checkpoint`. Anyone who clones the listing-cache pattern and assumes a re-read refreshes rows is wrong here.
- **Pruning a vanished transcript is a conditional full REBUILD, not a targeted delete (issue #15).** `message_usage` rows carry no per-path provenance, and one `message.id` legitimately lives in many transcripts (resume/compact), so "delete the vanished file's rows" would corrupt totals for any id that still lives in a present file. A provenance/refcount table would double every hot-path write for a rare deletion (rejected, KISS/YAGNI). So when a checkpointed transcript has genuinely vanished, `has_vanished_checkpoint` triggers `wipe` (both tables) and the same scan's per-file loop re-ingests the present set; INSERT OR IGNORE dedup re-derives correct totals (a still-present id survives, an only-in-vanished id drops — the actual prune). This replaces the old `retain`, which pruned only the checkpoint table and could never drop a stale message row; the rebuild is a strict superset. The cost lands only on an actual vanish (rare); in steady state the store is still cumulative all-time and grows monotonically, bounded at roughly the number of unique attributed assistant messages (~70k on the author's machine). A rolling-window is a separate deferred slice, as is the DESIGN.md rolling-24h toast budget.
- **Vanish detection is dir-scoped, to avoid a data-loss wipe on a transient read failure.** A checkpoint counts as vanished only when it is absent from the scan AND its parent dir was actually enumerated (its `read_dir` succeeded this pass); a checkpoint under a dir that failed to enumerate is "unknown", never pruned. Without this, a transient blip on one project dir would make all its transcripts look vanished and wipe the entire cumulative store plus force a full cold re-read of the ~216 MB corpus. This is why `transcript_refs_by_recency` returns the successfully-enumerated dir set alongside the refs, and `refresh_usage` takes it. A total enumeration failure therefore reports nothing vanished (no dir was read), so no separate "empty enumeration" guard is needed. The residual risk is a single-file `metadata()` race within an otherwise-readable dir: that one file looks vanished and can trigger a rebuild, but the rebuild re-ingests every present file and INSERT OR IGNORE makes it a no-op on totals, so it self-heals and never corrupts — a spent cold pass at worst.
- Partial-line safety: `mark` records `byte_offset` at the last newline consumed, never past a partial trailing line, so a truncated final record (no terminator yet, or serde fails) is left unparsed and re-read — as a tail from the same offset — once completed, then counted exactly once.
- A same-name personal skill and active-repo project skill both derive `(None, name)` (attribution carries no repo), so they would show the same figure. Near-zero in practice and documented as a limitation; a `cwd`-based disambiguation is a bounded later fix.

## Options considered

- **A `(path, mtime, size)` per-file memo like ADR 0022** — rejected: correct for an idempotent snapshot, wrong for a sum, because it cannot dedup a `message.id` shared across files.
- **A persisted running per-skill aggregate** — rejected: cannot self-correct once a dedup or a logic change invalidates a contribution; the GROUP BY is cheap and always correct.
- **A byte-offset checkpoint as the dedup mechanism** — rejected: an offset dedups within a file but not the cross-file copies; the message-id primary key is the actual dedup.
