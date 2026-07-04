# 22. Incremental listing index via a persisted per-file (mtime, size) memo

## Status

Accepted.

## Context

`scan_all` builds a `ListingIndex` from the always-on skill-listing bullets rendered in transcripts (ADR 0016).
Before this decision, `ListingIndex::build` re-read and re-parsed every in-scope transcript on every scan with no gating, so a warm rescan (a registry-change rescan, a panel reopen, or the first scan after launch) paid the full read even when no transcript had changed.
skillmon is a login-launched daemon, so that cost recurs at every login and every registry change, not just once.

Measured in release on a real `~/.claude` (144 top-level transcripts): a cold listing-index build is about 280ms and a warm rebuild that re-reads nothing is about 7ms.
The "~7s warm scan" in issue #3 is partly a misdiagnosis, mirroring issue #2's "~120s cold": the transcript and index re-read is only a few hundred ms on this corpus, and the dominant warm-scan cost is the on-demand ceiling (reading and hashing the ~216 MB of bundled reference files every scan, ADR 0017), which this decision does not address and which is tracked as the on-demand amortization follow-up.
The index re-read still scales with transcript volume, so eliminating it is a real scalability win independent of that residual, and it is the same incremental-read foundation the attributed-usage parser (issue #5) will need.

## Decision

Add a persisted per-transcript memo, `SqliteListingCache`, in its own sqlite file (`listing_index.sqlite`) beside the footprint cache.
It maps a transcript's `path` to its extracted `skill_listing` bullets plus the `(mtime_nanos, size, logic_version)` that produced them.
`ListingIndex::build_incremental` reads a transcript from disk only on a miss (a new path, a changed `mtime` or `size`, or a bumped extraction-logic version); an unchanged file's bullets are reused, so a warm rescan re-reads zero transcripts and rebuilds the index in a few ms.

Design points, each forced by the grilling:

- Persisted, not in-memory: `scan_all` is `&self` and already mutates the token cache through `&self` via rusqlite, so a persisted store needs no new interior-mutability primitive, and persistence is what makes the first scan after a login fast too (an in-memory memo would not).
- It lives in the adapter (ADR 0002): `skill_listing` is a Claude Code transcript format, so its memo is not put in the harness-neutral `TokenCache`; it is a concrete struct mirroring `TokenCache` (no trait, no fake), since there is no second implementation and no OS boundary to stub.
- Negative caching: a transcript with no `skill_listing` line (the vast majority) stores an empty bullet list, so it is recorded as processed and skipped next scan rather than re-read forever. Absence of a row means never-processed, never processed-with-no-bullets.
- Strict-equality change detection on `(mtime_nanos, size)`, in both directions: a same-size in-place rewrite that lands an older mtime (clock skew, an rsync `--times` restore) still re-reads. mtime is stored as exact nanoseconds since the epoch, never routed through whole seconds or text; a pre-1970 mtime forces a re-read rather than risking a false match.
- The memo stores every bullet in the file unfiltered by the discovered-skill set, and filtering happens at merge, so installing a new skill (which grows the wanted set) resolves from the memo with no re-read.
- Intra-file first-occurrence-per-name and inter-file most-recent-first ordering are preserved exactly, so `resolve()` output is byte-identical to the pre-memo build, and the existing per-repo-scoping tests pass unchanged through the new path.
- `logic_version` (a bumped const) invalidates every row wholesale when the extraction logic or the stored blob shape changes, the one thing `(mtime, size)` cannot catch.
- Pruning: `scan_all` drops memo rows for transcripts no longer enumerated, so the store cannot grow without bound as sessions come and go.

## Consequences

- A warm rescan re-reads no transcripts and rebuilds the listing index in a few ms, proven by a deterministic zero-reads test and an isolated real-corpus timing test (about 7ms warm vs about 280ms cold).
- The full warm `scan_all` is still gated by the on-demand ceiling read and hash, a separate cost tracked as a follow-up; this decision removes the transcript-index component and is a prerequisite for the incremental transcript reads issue #5 needs.
- Transcripts remain un-watched (ADR 0019): freshness stays lazy, evaluated on the next `scan_all`, and the memo only makes that lazy check cheap. The Reconstructed-to-Native upgrade still fires on the next scan after a render, because a render grows the file and the size change forces a re-read.
- `(mtime, size)` is not a content hash: a same-byte-size in-place rewrite inside one mtime tick is undetectable. Accepted, because transcript JSONL is append-only and any real change (including a render append) grows the size; content-hashing every file would re-read it, the exact cost being removed.

## Options considered

- **In-memory per-scan memo on the adapter**: satisfies the "warm rescan" criterion but does nothing for the first scan after login and forces a new interior-mutability field. Rejected in favor of persistence.
- **A table inside the footprint `TokenCache`**: rejected on the ADR 0002 boundary, since transcript data is Claude-Code-specific and the token cache is harness-neutral.
- **Byte-offset incremental parsing (DESIGN.md)**: deferred as over-scope; re-reading a changed file in full is correct, cheap for the few growing active files, and more robust against a mid-session compaction that rewrites the file.
