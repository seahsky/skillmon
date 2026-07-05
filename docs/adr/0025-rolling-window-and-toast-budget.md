# 25. Rolling-24h usage window and the attributed-work toast budget

## Status

Accepted.

Number provisional: whichever of the #12–#15 follow-up PRs lands first takes ADR 0025 and the rest bump.
This PR (#14) claims it; if it lands second, renumber to the next free ADR and update the `USAGE_LOGIC_VERSION` cross-reference.

## Context

Issue #5 shipped cumulative all-time attributed usage (ADR 0024).
DESIGN.md UX decision #4 asks for a rolling-24h budget toast (on by default) and per-skill anomaly toasts (off by default), plus a windowed view of the per-skill figures.
Three problems had to be solved without breaking #5's honesty rules (ADR 0003): usage stays an estimate framed "during," never "by," never a dollar figure.

- The `message_usage` store had no per-message time, so no query could restrict to the last 24h.
- The evaluation is time-relative, but the pure core holds no wall-clock (ADR 0008 keeps "now" out of the domain).
- A toast that re-fires on every panel open would be spam, so crossing the budget must fire once and debounce across scans and restarts.

## Decision

- **A `timestamp INTEGER NOT NULL` column on `message_usage`, parsed from each record's top-level RFC3339 `timestamp`** (`OffsetDateTime::parse` via the `time` crate's `parsing` feature, already a transitive dep).
Missing or malformed timestamps degrade to `0` (oldest) and the row is never dropped, so a timestamp-less record still counts all-time but never falsely lands in a recent window.
First-wins on a `message_id` collision: resume/compact copies of one message diverge sub-second, and INSERT OR IGNORE keeps the first-ingested timestamp, which is deterministic and negligible against a 24h window.
- **`USAGE_LOGIC_VERSION` 1 → 2, with a reordered migration.**
`usage_meta` is created first (it holds the version AND the budget config, so it must survive the wipe); the version is read before any message-table CREATE; on a mismatch both `message_usage` and `usage_checkpoint` are DROPPED (not DELETEd, because a bump can add a column); the tables are recreated unconditionally at the new schema after the drop; the version is written last so a crash mid-migration re-runs it idempotently.
Dropping the checkpoint forces a full re-ingest that backfills the new column.
- **Windowed queries that preserve every #5 guarantee.**
`totals_since`, `attributed_work_since`, and `work_by_key_and_day_since` each keep the `is_subagent = 0` filter and the `message_id` PK dedup; only a `timestamp >= cutoff` bound is added (`>=`, so a row exactly at the cutoff is inside the window).
`totals()` is now `totals_since(i64::MIN)`, so the all-time and windowed paths share one query.
The budget scalar is `attributed_work_since`, deliberately named "attributed," not "global": `message_usage` holds only skill-attributed rows, so it is total work across skills, not all work the account spent (this is also what makes the 250k default sane; an all-work budget would spam).
- **A pure `domain/budget.rs`: `evaluate_budget` and `detect_anomaly`, no I/O, no wall-clock, no `AppHandle`.**
`evaluate_budget` fires once when the rolling work strictly exceeds the limit, stays silent while a persisted `budget_alerted` flag is set, and re-arms when the window falls back to or under the limit (the 24h figure drops on its own as messages age out).
`detect_anomaly` flags a skill whose current-day work runs above a multiplier of its trailing daily average, gated by a floor.
"Now" is injected only at the `lib.rs` command boundary via `ScanParams`; the domain receives a cutoff, never a clock.
- **Scan threading with the `HarnessAdapter` trait untouched (ADR 0002).**
The former `scan_all` body moved into an inherent `ClaudeCodeAdapter::scan(&ScanParams) -> ScanOutcome`; the trait `scan_all` is now the clockless all-time shim `self.scan(&ScanParams::all_time()).report`.
`ScanReport` gained `usage_window_hours: Option<u32>` (`None` = all-time).
The MVP runtime path (`list_skills`) calls the inherent `scan` directly for the window + toasts, so the generic trait methods are no longer dispatched from the cdylib entry point; they are kept as the harness seam and exercised by tests (annotated `allow(dead_code)`).
- **Product taste: panel defaults to the all-time view** (preserving #5's shipped numbers; 24h is an opt-in toggle), **the budget toast is always evaluated on a fixed 24h window regardless of the view**, the **default budget is 250,000 attributed work tokens / 24h** (a single named, user-configurable const), and **anomaly alerts are toasts only, off by default** (no panel badge).
- **Toast copy is data, not a rendered string** (`ToastRequest::copy()`), so the pure core never touches the notification API.
Copy carries no `$` and no em dash (Sky's rule), keeps the `~` estimate framing, says tokens spent "during" skills / "not a bill" (budget) and "not by it" (anomaly).

## Consequences

- **Toasts are emitted in `lib.rs`, outside the scan mutex, after the core persisted the debounce flag.**
A failed `.show()` (a headless Linux daemon, or Windows without a registered AppUserModelID) is logged and dropped: a lost nudge, never a stuck flag that suppresses the next real crossing.
- **The budget re-evaluates only on `list_skills` (panel open / `registry-changed`), not in real time.**
The watcher watches registry surfaces, not transcripts (ADR 0019), so the honest promise is "next time you open the panel you're told you went over," not a live alarm.
- **Changing the limit or the enabled flag re-arms the debounce** (`set_usage_settings` resets `budget_alerted`), so a lowered limit re-evaluates on the next scan instead of staying silent until the 24h window resets on its own.
- The budget config and debounce live in `usage_meta`, which survives the schema wipe, so a user's settings outlive a logic-version bump.

## Options considered

- **A separate rolling-usage table** — rejected: the timestamp column plus a `>=` bound reuses the one deduped store, so the window and the all-time figure can never disagree.
- **Reading `SystemTime::now()` inside the scan** — rejected: it would put a wall-clock in the pure core and make the budget logic untestable without mocking time; injecting a cutoff keeps `evaluate_budget` a pure function.
- **A panel badge for anomalies** — deferred: DESIGN.md UX #4 only promises "available, off by default," and a toast is the lighter surface for a fuzzy, default-off signal.
- **Re-toasting on every over-budget scan** — rejected as spam; the persisted debounce is the whole point.
