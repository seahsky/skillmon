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

/// What a scan should do with one transcript, decided from its checkpoint
/// (issue #15). `Tail(off)` reads only the bytes appended past the last parsed
/// newline; any doubt collapses to `Full`, which is always safe because
/// `ingest` is INSERT OR IGNORE on the immutable `message.id` (ADR 0024).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPlan {
    /// `(mtime, size)` unchanged: the file is already fully parsed.
    Skip,
    /// Re-read the whole file from byte 0 (new file, shrink, in-place rewrite,
    /// version mismatch, or a legacy zero-offset row).
    Full,
    /// Read only `[offset..EOF]`; the file grew and the prefix is intact.
    Tail(u64),
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
                byte_offset   INTEGER NOT NULL DEFAULT 0,
                logic_version INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS usage_meta (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );",
        )?;

        // Guarded migration for `byte_offset` (issue #15). It changes only WHICH
        // bytes a scan reads, not how a row derives from bytes, so it needs no
        // logic-version bump: a DB predating it keeps its history and its
        // existing rows default to offset 0, forcing one Full re-read on their
        // next growth (self-correcting, ADR 0024).
        let has_byte_offset = conn
            .prepare("PRAGMA table_info(usage_checkpoint)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<SqliteResult<Vec<String>>>()?
            .iter()
            .any(|name| name == "byte_offset");
        if !has_byte_offset {
            conn.execute(
                "ALTER TABLE usage_checkpoint ADD COLUMN byte_offset INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }

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

    /// Decides how to read `path` given its freshly-stat'd `(mtime, size)`,
    /// from the stored checkpoint (issue #15). The `Skip` branch is the exact
    /// strict-equality freshness gate of ADR 0022 (an older mtime is still a
    /// miss, so a same-size backwards-clock rewrite re-reads); a genuine growth
    /// with a non-zero stored offset is the only case that tails.
    pub fn read_plan(&self, path: &str, new_mtime: i64, new_size: i64) -> ReadPlan {
        let row: Option<(i64, i64, i64, i64)> = self
            .conn
            .query_row(
                "SELECT mtime_nanos, size, byte_offset, logic_version FROM usage_checkpoint WHERE path = ?1",
                params![path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .expect("usage_checkpoint lookup should not fail");
        let Some((mtime, size, byte_offset, version)) = row else {
            return ReadPlan::Full; // never parsed
        };
        if version != USAGE_LOGIC_VERSION {
            return ReadPlan::Full; // parsed under stale logic
        }
        if mtime == new_mtime && size == new_size {
            return ReadPlan::Skip; // fully parsed already
        }
        if new_size <= size {
            // A shrink, or a same-size in-place rewrite (a different mtime): the
            // append assumption is void, so re-read from the top. Strict on
            // size equality preserves ADR 0022's both-ways clock gate.
            return ReadPlan::Full;
        }
        // Grew. Tail from the stored offset when we have a real line boundary to
        // resume from; a legacy zero offset (or a file that never completed a
        // line) has no boundary, so it must be re-read whole.
        if byte_offset > 0 {
            ReadPlan::Tail(byte_offset as u64)
        } else {
            ReadPlan::Full
        }
    }

    /// Records `path` as parsed up to `byte_offset` (the byte position just past
    /// the last newline consumed) at `(mtime_nanos, size)`. Call only after a
    /// successful parse of the consumed prefix, so a truncated trailing line is
    /// re-read next scan rather than marked complete.
    pub fn mark(&self, path: &str, mtime_nanos: i64, size: i64, byte_offset: i64) {
        self.conn
            .execute(
                "INSERT INTO usage_checkpoint (path, mtime_nanos, size, byte_offset, logic_version)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(path) DO UPDATE SET
                     mtime_nanos = excluded.mtime_nanos,
                     size = excluded.size,
                     byte_offset = excluded.byte_offset,
                     logic_version = excluded.logic_version",
                params![path, mtime_nanos, size, byte_offset, USAGE_LOGIC_VERSION],
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

    /// Whether any checkpointed transcript has genuinely vanished: it is absent
    /// from `seen` AND its parent dir is in `enumerated_dirs` (a dir whose
    /// `read_dir` actually succeeded this scan). A checkpoint under a dir that
    /// failed to enumerate is "unknown", never a vanish, so a transient blip on
    /// one project dir can't be mistaken for every transcript disappearing and
    /// trigger a needless wipe of the cumulative store (issue #15 data-loss
    /// guard, ADR 0024). Usage rows carry no per-path provenance and a
    /// `message.id` can live in many transcripts, so a true vanish forces a
    /// full rebuild (`wipe` + re-ingest), never a targeted delete.
    pub fn has_vanished_checkpoint(
        &self,
        seen: &HashSet<String>,
        enumerated_dirs: &HashSet<String>,
    ) -> bool {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM usage_checkpoint")
            .expect("usage_checkpoint path scan prepare should not fail");
        let paths = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("usage_checkpoint path scan should not fail");
        for path in paths {
            let path = path.expect("usage_checkpoint path mapping should not fail");
            if seen.contains(&path) {
                continue; // still present this scan
            }
            // Absent from the enumeration -- a vanish ONLY if its dir was
            // actually read. A dir whose read_dir failed is "unknown": the
            // residual risk is a single-file metadata() race within an
            // otherwise-readable dir, which self-heals via INSERT OR IGNORE on
            // the next scan and never corrupts totals (ADR 0024).
            if let Some(parent) = Path::new(&path).parent() {
                if enumerated_dirs.contains(parent.to_string_lossy().as_ref()) {
                    return true;
                }
            }
        }
        false
    }

    /// Drops all attributed usage AND every checkpoint. Called only on a
    /// detected vanish, so the per-file loop re-ingests the present set from
    /// scratch; INSERT OR IGNORE dedup makes the rebuild re-derive correct
    /// totals (a still-present `message.id` survives, an only-in-vanished one
    /// drops -- the actual prune).
    pub fn wipe(&self) {
        self.conn
            .execute_batch("DELETE FROM message_usage; DELETE FROM usage_checkpoint;")
            .expect("usage store wipe should not fail");
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
    fn read_plan_skips_a_fresh_unchanged_file() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.mark("/p/a.jsonl", 1000, 500, 500);
        assert_eq!(cache.read_plan("/p/a.jsonl", 1000, 500), ReadPlan::Skip);
    }

    #[test]
    fn read_plan_tails_a_grown_file_from_the_stored_offset() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.mark("/p/a.jsonl", 1000, 500, 500);
        // grown (600 > 500) with a non-zero stored offset: read only [500..EOF].
        assert_eq!(cache.read_plan("/p/a.jsonl", 2000, 600), ReadPlan::Tail(500));
    }

    #[test]
    fn read_plan_full_reads_a_shrunk_file() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.mark("/p/a.jsonl", 1000, 500, 500);
        assert_eq!(cache.read_plan("/p/a.jsonl", 2000, 400), ReadPlan::Full, "a shrink is never a tail");
    }

    #[test]
    fn read_plan_full_reads_a_same_size_inplace_rewrite() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.mark("/p/a.jsonl", 1000, 500, 500);
        // Same size, newer mtime: an in-place rewrite, not an append.
        assert_eq!(cache.read_plan("/p/a.jsonl", 2000, 500), ReadPlan::Full);
        // Same size, OLDER mtime: strict equality both ways still re-reads.
        assert_eq!(cache.read_plan("/p/a.jsonl", 999, 500), ReadPlan::Full);
    }

    #[test]
    fn read_plan_full_reads_an_unknown_path() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        assert_eq!(cache.read_plan("/p/never.jsonl", 1, 1), ReadPlan::Full);
    }

    #[test]
    fn read_plan_full_reads_a_grown_file_with_zero_offset() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        // A legacy/whole-file-no-newline row at offset 0: growth forces a Full.
        cache.mark("/p/a.jsonl", 1000, 500, 0);
        assert_eq!(cache.read_plan("/p/a.jsonl", 2000, 600), ReadPlan::Full);
    }

    #[test]
    fn wipe_clears_both_data_tables() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&[row("m1", "grilling", None, 10)]);
        cache.mark("/p/a.jsonl", 1, 1, 1);
        assert_eq!(cache.totals().len(), 1);

        cache.wipe();

        assert!(cache.totals().is_empty(), "wipe clears message_usage");
        assert_eq!(cache.read_plan("/p/a.jsonl", 1, 1), ReadPlan::Full, "wipe clears usage_checkpoint");
    }

    #[test]
    fn has_vanished_checkpoint_flags_an_absent_path_only_when_its_dir_was_enumerated() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.mark("/p/gone.jsonl", 1, 1, 1);
        cache.mark("/p/here.jsonl", 1, 1, 1);
        let dirs: HashSet<String> = ["/p".to_string()].into_iter().collect();

        let both: HashSet<String> =
            ["/p/gone.jsonl".to_string(), "/p/here.jsonl".to_string()].into_iter().collect();
        assert!(!cache.has_vanished_checkpoint(&both, &dirs), "both present -> nothing vanished");

        let only_here: HashSet<String> = ["/p/here.jsonl".to_string()].into_iter().collect();
        assert!(cache.has_vanished_checkpoint(&only_here, &dirs), "gone.jsonl absent under an enumerated dir -> vanished");

        // gone.jsonl absent, but its dir was NOT enumerated this scan: unknown.
        let no_dirs: HashSet<String> = HashSet::new();
        assert!(!cache.has_vanished_checkpoint(&only_here, &no_dirs), "an un-enumerated dir is never pruned");

        // Total enumeration failure: nothing enumerated -> nothing vanished.
        assert!(!cache.has_vanished_checkpoint(&HashSet::new(), &HashSet::new()));
    }

    #[test]
    fn byte_offset_migrates_onto_an_existing_v1_db_without_wiping_history() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.sqlite");
        // Build a pre-#15 DB by hand: usage_checkpoint WITHOUT byte_offset, one
        // message_usage row, logic_version already current (so no version wipe).
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE message_usage (
                    message_id TEXT PRIMARY KEY, attribution_skill TEXT NOT NULL,
                    attribution_plugin TEXT, is_subagent INTEGER NOT NULL,
                    work INTEGER NOT NULL, cache_write INTEGER NOT NULL, cache_read INTEGER NOT NULL
                 );
                 CREATE TABLE usage_checkpoint (
                    path TEXT PRIMARY KEY, mtime_nanos INTEGER NOT NULL,
                    size INTEGER NOT NULL, logic_version INTEGER NOT NULL
                 );
                 CREATE TABLE usage_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
            )
            .unwrap();
            conn.execute("INSERT INTO usage_meta (key, value) VALUES ('logic_version', ?1)", params![USAGE_LOGIC_VERSION]).unwrap();
            conn.execute("INSERT INTO message_usage VALUES ('m1','grilling',NULL,0,30,0,0)", []).unwrap();
            conn.execute("INSERT INTO usage_checkpoint (path, mtime_nanos, size, logic_version) VALUES ('/p/a.jsonl', 1000, 500, ?1)", params![USAGE_LOGIC_VERSION]).unwrap();
        }

        // Reopen through the real init: the guarded ALTER adds byte_offset and,
        // because the logic_version already matches, history is preserved.
        let cache = SqliteUsageCache::open(&db).unwrap();
        assert_eq!(cache.totals().len(), 1, "an ALTER migration must not wipe message history");
        assert_eq!(cache.totals()[0].work, 30);
        // The migrated row reads back with byte_offset defaulted to 0.
        assert_eq!(cache.read_plan("/p/a.jsonl", 1000, 500), ReadPlan::Skip, "an unchanged migrated file is still fresh");
        assert_eq!(cache.read_plan("/p/a.jsonl", 2000, 600), ReadPlan::Full, "legacy offset 0 forces one full re-read on first growth");
    }

    #[test]
    fn a_logic_version_bump_wipes_both_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.sqlite");
        {
            let cache = SqliteUsageCache::open(&db).unwrap();
            cache.ingest(&[row("m1", "grilling", None, 10)]);
            cache.mark("/p/a.jsonl", 1, 1, 0);
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
        assert_eq!(cache.read_plan("/p/a.jsonl", 1, 1), ReadPlan::Full, "a logic bump must wipe usage_checkpoint");
    }
}
