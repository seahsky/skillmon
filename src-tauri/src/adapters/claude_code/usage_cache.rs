use rusqlite::{params, Connection, OptionalExtension, Result as SqliteResult};
use std::collections::HashSet;
use std::path::Path;

/// Bump when the parse or bucketing logic (`usage::parse_usage_rows`) changes.
/// Unlike `SqliteListingCache`, whose per-path `put` overwrites, `message_usage`
/// is written INSERT OR IGNORE and can never overwrite a stale row, so a bump
/// must WIPE both tables and rebuild (handled in `init`). This divergence is
/// load-bearing (ADR 0024).
pub const USAGE_LOGIC_VERSION: i64 = 1;

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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS message_usage (
                message_id         TEXT PRIMARY KEY,
                attribution_skill  TEXT NOT NULL,
                attribution_plugin TEXT,
                is_subagent        INTEGER NOT NULL,
                work               INTEGER NOT NULL,
                cache_write        INTEGER NOT NULL,
                cache_read         INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS usage_checkpoint (
                path          TEXT PRIMARY KEY,
                mtime_nanos   INTEGER NOT NULL,
                size          INTEGER NOT NULL,
                logic_version INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS usage_meta (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );",
        )?;

        // Because INSERT OR IGNORE never overwrites, a logic-version change
        // cannot refresh stored rows in place; the only correct migration is a
        // wipe-and-rebuild (ADR 0024).
        let stored: Option<i64> = conn
            .query_row("SELECT value FROM usage_meta WHERE key = 'logic_version'", [], |r| r.get(0))
            .optional()?;
        if stored != Some(USAGE_LOGIC_VERSION) {
            conn.execute_batch("DELETE FROM message_usage; DELETE FROM usage_checkpoint;")?;
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

    /// Inserts each row, INSERT OR IGNORE keyed on `message_id`, so a message
    /// already seen (in this file or any other, via a resume/compact copy) is
    /// counted exactly once. Duplicate `message.id`s carry identical usage, so
    /// first-wins is safe.
    pub fn ingest(&self, rows: &[UsageRow]) {
        for row in rows {
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO message_usage
                     (message_id, attribution_skill, attribution_plugin, is_subagent, work, cache_write, cache_read)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
                .expect("message_usage insert should not fail");
        }
    }

    /// Per-attribution totals over the main-thread rows only (`is_subagent =
    /// 0`); sub-agent rows are excluded from the default metric (ADR 0005).
    /// Always a fresh GROUP BY, never a persisted aggregate.
    pub fn totals(&self) -> Vec<UsageTotal> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT attribution_skill, attribution_plugin,
                        SUM(work), SUM(cache_write), SUM(cache_read)
                 FROM message_usage WHERE is_subagent = 0
                 GROUP BY attribution_skill, attribution_plugin",
            )
            .expect("usage totals prepare should not fail");
        let rows = stmt
            .query_map([], |r| {
                // SUM() is i64 in SQLite; counts are non-negative, so widen to
                // u64 without truncating (the `as u32` here would silently wrap
                // a cumulative total past ~4.29e9).
                let work: i64 = r.get(2)?;
                let cache_write: i64 = r.get(3)?;
                let cache_read: i64 = r.get(4)?;
                Ok(UsageTotal {
                    attribution_skill: r.get(0)?,
                    attribution_plugin: r.get(1)?,
                    work: work.max(0) as u64,
                    cache_write: cache_write.max(0) as u64,
                    cache_read: cache_read.max(0) as u64,
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
        }
    }

    #[test]
    fn dedups_by_message_id_so_a_repeated_id_counts_once() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&[row("msg_A", "grilling", None, 30)]);
        cache.ingest(&[row("msg_A", "grilling", None, 30)]); // resume/compact copy
        let totals = cache.totals();
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
        let mut totals = cache.totals();
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
        let totals = cache.totals();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 10, "sub-agent work is excluded from the default totals");
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
        assert_eq!(cache.totals()[0].work, 10, "message history is never pruned by retain");
    }

    #[test]
    fn a_logic_version_bump_wipes_both_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.sqlite");
        {
            let cache = SqliteUsageCache::open(&db).unwrap();
            cache.ingest(&[row("m1", "grilling", None, 10)]);
            cache.mark("/p/a.jsonl", 1, 1);
            assert_eq!(cache.totals().len(), 1);
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
        assert!(cache.totals().is_empty(), "a logic bump must wipe message_usage");
        assert!(!cache.is_fresh("/p/a.jsonl", 1, 1), "a logic bump must wipe usage_checkpoint");
    }
}
