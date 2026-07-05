use rusqlite::{params, Connection, OptionalExtension, Result as SqliteResult};
use std::collections::HashSet;
use std::path::Path;

/// Bump when the parse or bucketing logic (`usage::parse_usage_rows` /
/// `reconstruct_usage_rows`) or the `message_usage` SCHEMA changes. Unlike
/// `SqliteListingCache`, whose per-path `put` overwrites, a reconstructed
/// `message_usage` row is written INSERT OR IGNORE and can never overwrite a
/// stale row, so a bump must DROP both data tables and rebuild them at the new
/// schema (handled in `init`). This divergence is load-bearing (ADR 0024).
///
/// v2 (issue #12): added the `attribution_source TEXT NOT NULL` column, so the
/// migration DROPs rather than DELETEs -- a DELETE would leave the old, narrower
/// table and fail every subsequent INSERT against the wider schema.
pub const USAGE_LOGIC_VERSION: i64 = 2;

/// One attributed `assistant` record's usage, the unit written to the store.
/// `message_id` is the dedup key (`message.id`, never the record `uuid`);
/// `work = input + output` only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    pub message_id: String,
    pub attribution_skill: String,
    pub attribution_plugin: Option<String>,
    pub is_subagent: bool,
    pub work: u32,
    pub cache_write: u32,
    pub cache_read: u32,
    /// `true` for a version-gated reconstructed credit (issue #12), `false` for
    /// a native `attributionSkill` credit. Drives which ingest path is taken:
    /// native rows overwrite on conflict, reconstructed rows never do, so native
    /// always wins for a shared `message.id` regardless of ingest order.
    pub reconstructed: bool,
}

/// Per-attribution totals, re-derived by GROUP BY on every read so a dedup can
/// never leave a stale running sum behind. `u64` because these are cumulative
/// all-time sums that must never silently wrap (unlike the per-message
/// `UsageRow`, whose single-turn counts fit `u32`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageTotal {
    pub attribution_skill: String,
    pub attribution_plugin: Option<String>,
    pub work: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    /// `true` if ANY message in this attribution group was reconstructed, so a
    /// mixed skill is honestly downgraded to the lower-confidence label (ADR
    /// 0003). Derived from `MAX(attribution_source)`: "reconstructed" sorts
    /// after "native" under the default BINARY collation, so the group MAX is
    /// "reconstructed" iff at least one row is (this ordering is load-bearing).
    pub reconstructed: bool,
}

/// Persisted, GLOBAL attributed-usage store (issue #5, ADR 0024). Global
/// because resume/branch/compact copy the same `message.id` into different
/// transcript files, so a per-file memo (like `SqliteListingCache`) would
/// double-count; a `message_id PRIMARY KEY` + INSERT OR IGNORE makes any
/// re-read idempotent and dedup a DB constraint, not emergent behaviour. The
/// `(path, mtime, size)` checkpoint is only a perf gate on whether a file is
/// opened. Claude-Code-specific, so it lives in the adapter (ADR 0002) in its
/// own sqlite file.
pub struct SqliteUsageCache {
    conn: Connection,
}

impl SqliteUsageCache {
    pub fn open(path: &Path) -> SqliteResult<Self> {
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> SqliteResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> SqliteResult<Self> {
        // `usage_meta` must exist before the stored logic version can be read.
        // It survives a logic bump (only the two data tables are dropped), so
        // the version marker itself is never lost.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_meta (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );",
        )?;

        let stored: Option<i64> = conn
            .query_row("SELECT value FROM usage_meta WHERE key = 'logic_version'", [], |r| r.get(0))
            .optional()?;

        // A logic bump can change the SCHEMA (v2 added `attribution_source NOT
        // NULL`), so the migration must DROP the data tables BEFORE the CREATE
        // below, not DELETE their rows: a DELETE keeps the old, narrower columns
        // and every later INSERT against the wider schema would fail. DROP also
        // resets every checkpoint, so all transcripts re-ingest under the new
        // logic (required, else reconstructed rows would never land). ADR 0024.
        if stored != Some(USAGE_LOGIC_VERSION) {
            conn.execute_batch("DROP TABLE IF EXISTS message_usage; DROP TABLE IF EXISTS usage_checkpoint;")?;
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS message_usage (
                message_id         TEXT PRIMARY KEY,
                attribution_skill  TEXT NOT NULL,
                attribution_plugin TEXT,
                is_subagent        INTEGER NOT NULL,
                work               INTEGER NOT NULL,
                cache_write        INTEGER NOT NULL,
                cache_read         INTEGER NOT NULL,
                attribution_source TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS usage_checkpoint (
                path          TEXT PRIMARY KEY,
                mtime_nanos   INTEGER NOT NULL,
                size          INTEGER NOT NULL,
                logic_version INTEGER NOT NULL
            );",
        )?;

        if stored != Some(USAGE_LOGIC_VERSION) {
            conn.execute(
                "INSERT INTO usage_meta (key, value) VALUES ('logic_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![USAGE_LOGIC_VERSION],
            )?;
        }

        Ok(Self { conn })
    }

    /// Whether `path` is already parsed at exactly `(mtime_nanos, size)` under
    /// the current logic version. Strict equality in both directions (an older
    /// mtime is still a miss), so a same-size in-place rewrite with a backwards
    /// clock still re-reads (ADR 0022).
    pub fn is_fresh(&self, path: &str, mtime_nanos: i64, size: i64) -> bool {
        let row: Option<(i64, i64, i64)> = self
            .conn
            .query_row(
                "SELECT mtime_nanos, size, logic_version FROM usage_checkpoint WHERE path = ?1",
                params![path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .expect("usage_checkpoint lookup should not fail");
        matches!(row, Some((m, s, v)) if m == mtime_nanos && s == size && v == USAGE_LOGIC_VERSION)
    }

    /// Records `path` as parsed at `(mtime_nanos, size)`. Call only after a
    /// successful whole-file parse, so a truncated trailing line is re-read
    /// next scan rather than marked complete.
    pub fn mark(&self, path: &str, mtime_nanos: i64, size: i64) {
        self.conn
            .execute(
                "INSERT INTO usage_checkpoint (path, mtime_nanos, size, logic_version)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET
                     mtime_nanos = excluded.mtime_nanos,
                     size = excluded.size,
                     logic_version = excluded.logic_version",
                params![path, mtime_nanos, size, USAGE_LOGIC_VERSION],
            )
            .expect("usage_checkpoint upsert should not fail");
    }

    /// Inserts each row keyed on `message_id`, so a message already seen (in
    /// this file or any other, via a resume/compact copy) is counted exactly
    /// once. Two paths make native-wins a STRUCTURAL, order-independent property
    /// (SHOULD-FIX 5, issue #12), because `refresh_usage` iterates transcripts
    /// by recency, not native-first:
    ///
    /// - a **native** row is written `ON CONFLICT DO UPDATE`, unconditionally
    ///   overwriting whatever is there (a duplicate native carries identical
    ///   usage, so the overwrite is a no-op; a prior reconstructed row is
    ///   upgraded to native).
    /// - a **reconstructed** row is written `INSERT OR IGNORE`, so it never
    ///   displaces a native (or an earlier reconstructed) row.
    ///
    /// Whichever order the two arrive in, the message ends up native if a native
    /// row for it exists anywhere, honestly reconstructed only if none does.
    pub fn ingest(&self, rows: &[UsageRow]) {
        for row in rows {
            if row.reconstructed {
                self.conn
                    .execute(
                        "INSERT OR IGNORE INTO message_usage
                         (message_id, attribution_skill, attribution_plugin, is_subagent, work, cache_write, cache_read, attribution_source)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'reconstructed')",
                        params![
                            row.message_id,
                            row.attribution_skill,
                            row.attribution_plugin,
                            row.is_subagent as i64,
                            row.work,
                            row.cache_write,
                            row.cache_read,
                        ],
                    )
                    .expect("message_usage reconstructed insert should not fail");
            } else {
                self.conn
                    .execute(
                        "INSERT INTO message_usage
                         (message_id, attribution_skill, attribution_plugin, is_subagent, work, cache_write, cache_read, attribution_source)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'native')
                         ON CONFLICT(message_id) DO UPDATE SET
                             attribution_skill  = excluded.attribution_skill,
                             attribution_plugin = excluded.attribution_plugin,
                             is_subagent        = excluded.is_subagent,
                             work               = excluded.work,
                             cache_write        = excluded.cache_write,
                             cache_read         = excluded.cache_read,
                             attribution_source = excluded.attribution_source",
                        params![
                            row.message_id,
                            row.attribution_skill,
                            row.attribution_plugin,
                            row.is_subagent as i64,
                            row.work,
                            row.cache_write,
                            row.cache_read,
                        ],
                    )
                    .expect("message_usage native insert should not fail");
            }
        }
    }

    /// Per-attribution totals. With `include_subagents` false (the default
    /// metric, ADR 0005) only main-thread rows (`is_subagent = 0`) are summed;
    /// with it true, sub-agent rows are folded in as well (issue #13's toggle).
    /// Two explicit SQL strings rather than a dynamic predicate so the exact
    /// query for each mode is legible at a glance. Always a fresh GROUP BY,
    /// never a persisted aggregate, so a dedup can't leave a stale running sum.
    pub fn totals(&self, include_subagents: bool) -> Vec<UsageTotal> {
        let sql = if include_subagents {
            "SELECT attribution_skill, attribution_plugin,
                    SUM(work), SUM(cache_write), SUM(cache_read)
             FROM message_usage
             GROUP BY attribution_skill, attribution_plugin"
        } else {
            "SELECT attribution_skill, attribution_plugin,
                    SUM(work), SUM(cache_write), SUM(cache_read)
             FROM message_usage WHERE is_subagent = 0
             GROUP BY attribution_skill, attribution_plugin"
        };
        let mut stmt = self.conn.prepare(sql).expect("usage totals prepare should not fail");
        let rows = stmt
            .query_map([], |r| {
                // SUM() is i64 in SQLite; counts are non-negative, so widen to
                // u64 without truncating (the `as u32` here would silently wrap
                // a cumulative total past ~4.29e9).
                let work: i64 = r.get(2)?;
                let cache_write: i64 = r.get(3)?;
                let cache_read: i64 = r.get(4)?;
                // "reconstructed" > "native" under BINARY collation, so the
                // group MAX is "reconstructed" iff at least one message in it
                // was reconstructed (the sticky downgrade, ADR 0003).
                let source: String = r.get(5)?;
                Ok(UsageTotal {
                    attribution_skill: r.get(0)?,
                    attribution_plugin: r.get(1)?,
                    work: work.max(0) as u64,
                    cache_write: cache_write.max(0) as u64,
                    cache_read: cache_read.max(0) as u64,
                    reconstructed: source == "reconstructed",
                })
            })
            .expect("usage totals query should not fail");
        rows.collect::<SqliteResult<Vec<UsageTotal>>>().expect("usage totals mapping should not fail")
    }

    /// Prunes checkpoint rows for transcripts no longer present, so the gate
    /// table can't grow unbounded. Only the checkpoint table is pruned; the
    /// `message_usage` history is intentionally cumulative in the MVP (ADR 0024).
    pub fn retain(&self, keep_paths: &HashSet<String>) {
        let existing: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path FROM usage_checkpoint")
                .expect("usage_checkpoint path scan prepare should not fail");
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .expect("usage_checkpoint path scan should not fail");
            rows.collect::<SqliteResult<Vec<String>>>().expect("usage_checkpoint path mapping should not fail")
        };
        for path in existing {
            if !keep_paths.contains(&path) {
                self.conn
                    .execute("DELETE FROM usage_checkpoint WHERE path = ?1", params![path])
                    .expect("usage_checkpoint prune delete should not fail");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(message_id: &str, skill: &str, plugin: Option<&str>, work: u32) -> UsageRow {
        UsageRow {
            message_id: message_id.to_string(),
            attribution_skill: skill.to_string(),
            attribution_plugin: plugin.map(|p| p.to_string()),
            is_subagent: false,
            work,
            cache_write: 0,
            cache_read: 0,
            reconstructed: false,
        }
    }

    fn reconstructed_row(message_id: &str, skill: &str, plugin: Option<&str>, work: u32) -> UsageRow {
        UsageRow { reconstructed: true, ..row(message_id, skill, plugin, work) }
    }

    #[test]
    fn dedups_by_message_id_so_a_repeated_id_counts_once() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&[row("msg_A", "grilling", None, 30)]);
        cache.ingest(&[row("msg_A", "grilling", None, 30)]); // resume/compact copy
        let totals = cache.totals(false);
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 30, "a repeated message.id must count once, not twice");
    }

    #[test]
    fn totals_group_by_attribution_and_sum_distinct_messages() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&[
            row("m1", "grilling", None, 10),
            row("m2", "grilling", None, 20),
            row("m3", "executing-plans", Some("superpowers"), 5),
        ]);
        let mut totals = cache.totals(false);
        totals.sort_by(|a, b| a.attribution_skill.cmp(&b.attribution_skill));
        assert_eq!(totals.len(), 2);
        let grilling = totals.iter().find(|t| t.attribution_skill == "grilling").unwrap();
        assert_eq!(grilling.work, 30);
        let ep = totals.iter().find(|t| t.attribution_skill == "executing-plans").unwrap();
        assert_eq!(ep.attribution_plugin.as_deref(), Some("superpowers"));
        assert_eq!(ep.work, 5);
    }

    #[test]
    fn subagent_rows_are_excluded_from_totals() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        let mut sub = row("m_sub", "grilling", None, 99);
        sub.is_subagent = true;
        cache.ingest(&[row("m_main", "grilling", None, 10), sub]);
        let totals = cache.totals(false);
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 10, "sub-agent work is excluded from the default totals");
    }

    #[test]
    fn totals_include_subagents_true_includes_subagent_rows() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        let mut sub = row("m_sub", "grilling", None, 99);
        sub.is_subagent = true;
        cache.ingest(&[row("m_main", "grilling", None, 10), sub]);

        let default = cache.totals(false);
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].work, 10, "the default metric still excludes the sub-agent row");

        let with_sub = cache.totals(true);
        assert_eq!(with_sub.len(), 1);
        assert_eq!(with_sub[0].work, 109, "toggle on folds the sub-agent's 99 into the main 10");
    }

    #[test]
    fn checkpoint_freshness_is_strict_equality_in_both_directions() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        let m = 1_000i64;
        cache.mark("/p/a.jsonl", m, 500);
        assert!(cache.is_fresh("/p/a.jsonl", m, 500));
        assert!(!cache.is_fresh("/p/a.jsonl", m, 501), "grown file is a miss");
        assert!(!cache.is_fresh("/p/a.jsonl", m + 1, 500), "newer mtime is a miss");
        assert!(!cache.is_fresh("/p/a.jsonl", m - 1, 500), "OLDER mtime is a miss (no newer-than compare)");
        assert!(!cache.is_fresh("/p/never.jsonl", m, 500), "unseen path is a miss");
    }

    #[test]
    fn retain_prunes_only_the_checkpoint_table_not_the_message_history() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.mark("/p/gone.jsonl", 1, 1);
        cache.mark("/p/here.jsonl", 1, 1);
        cache.ingest(&[row("m1", "grilling", None, 10)]);

        let keep: HashSet<String> = ["/p/here.jsonl".to_string()].into_iter().collect();
        cache.retain(&keep);

        assert!(cache.is_fresh("/p/here.jsonl", 1, 1));
        assert!(!cache.is_fresh("/p/gone.jsonl", 1, 1), "absent path pruned from the gate");
        assert_eq!(cache.totals(false)[0].work, 10, "message history is never pruned by retain");
    }

    #[test]
    fn a_logic_version_bump_wipes_both_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.sqlite");
        {
            let cache = SqliteUsageCache::open(&db).unwrap();
            cache.ingest(&[row("m1", "grilling", None, 10)]);
            cache.mark("/p/a.jsonl", 1, 1);
            assert_eq!(cache.totals(false).len(), 1);
        }
        // Simulate an old-version DB: rewrite the stored logic_version.
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute("UPDATE usage_meta SET value = ?1 WHERE key = 'logic_version'", params![USAGE_LOGIC_VERSION - 1])
                .unwrap();
        }
        // Reopening detects the mismatch and wipes: INSERT OR IGNORE can't
        // overwrite stale rows, so the only correct migration is a wipe.
        let cache = SqliteUsageCache::open(&db).unwrap();
        assert!(cache.totals(false).is_empty(), "a logic bump must wipe message_usage");
        assert!(!cache.is_fresh("/p/a.jsonl", 1, 1), "a logic bump must wipe usage_checkpoint");
    }

    #[test]
    fn reconstructed_row_stored_and_grouped_with_source() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&[reconstructed_row("m1", "grilling", None, 42)]);
        let totals = cache.totals();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 42);
        assert!(totals[0].reconstructed, "a reconstructed row's total is flagged reconstructed");
    }

    #[test]
    fn native_wins_after_reconstructed_same_msgid() {
        // Reconstructed lands first, native second: the native row must
        // overwrite it (source flips to native, usage becomes the native usage).
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&[reconstructed_row("m1", "grilling", None, 5)]);
        cache.ingest(&[row("m1", "grilling", None, 30)]);
        let totals = cache.totals();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 30, "native usage overwrites the reconstructed guess");
        assert!(!totals[0].reconstructed, "a native contribution wins the source label");
    }

    #[test]
    fn native_wins_before_reconstructed_same_msgid() {
        // Native lands first, reconstructed second: INSERT OR IGNORE must leave
        // the native row untouched (the mirror of the case above).
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&[row("m1", "grilling", None, 30)]);
        cache.ingest(&[reconstructed_row("m1", "grilling", None, 5)]);
        let totals = cache.totals();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 30, "the reconstructed row must not displace the native one");
        assert!(!totals[0].reconstructed);
    }

    #[test]
    fn skill_with_both_flagged_reconstructed() {
        // One skill, two distinct messages: one native, one reconstructed. The
        // group is downgraded to reconstructed (sticky), and both sums count.
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&[row("m_native", "grilling", None, 10)]);
        cache.ingest(&[reconstructed_row("m_recon", "grilling", None, 7)]);
        let totals = cache.totals();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 17);
        assert!(totals[0].reconstructed, "any reconstructed contribution downgrades the whole skill");
    }

    #[test]
    fn dedup_by_message_id_holds_for_reconstructed() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&[reconstructed_row("m1", "grilling", None, 12)]);
        cache.ingest(&[reconstructed_row("m1", "grilling", None, 12)]); // resume/compact copy
        let totals = cache.totals();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 12, "a repeated reconstructed message.id counts once");
    }

    #[test]
    fn logic_version_bump_drops_and_rebuilds_with_new_column() {
        // Stand up a v1-SHAPE table by hand: the old 7-column message_usage with
        // NO attribution_source, plus a row and the v1 logic marker. This is the
        // exact defect the DELETE-based migration would miss -- a DELETE keeps
        // these narrow columns and the v2 INSERT (which supplies attribution_source)
        // would fail at runtime.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.sqlite");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE message_usage (
                    message_id         TEXT PRIMARY KEY,
                    attribution_skill  TEXT NOT NULL,
                    attribution_plugin TEXT,
                    is_subagent        INTEGER NOT NULL,
                    work               INTEGER NOT NULL,
                    cache_write        INTEGER NOT NULL,
                    cache_read         INTEGER NOT NULL
                );
                CREATE TABLE usage_checkpoint (
                    path          TEXT PRIMARY KEY,
                    mtime_nanos   INTEGER NOT NULL,
                    size          INTEGER NOT NULL,
                    logic_version INTEGER NOT NULL
                );
                CREATE TABLE usage_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO message_usage
                 (message_id, attribution_skill, attribution_plugin, is_subagent, work, cache_write, cache_read)
                 VALUES ('old', 'grilling', NULL, 0, 99, 0, 0)",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO usage_meta (key, value) VALUES ('logic_version', 1)", []).unwrap();
        }

        // Opening at v2 must DROP the narrow table, rebuild it with the new
        // column, and drop the pre-migration row.
        let cache = SqliteUsageCache::open(&db).unwrap();
        assert!(cache.totals().is_empty(), "the v1 row must be gone after the drop-and-rebuild");

        // The new column must exist and accept a reconstructed insert -- the
        // whole point of the DROP-before-CREATE ordering.
        cache.ingest(&[reconstructed_row("new", "grilling", None, 8)]);
        let totals = cache.totals();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 8);
        assert!(totals[0].reconstructed);
    }
}
