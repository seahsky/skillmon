use rusqlite::{params, Connection, OptionalExtension, Result as SqliteResult};
use std::collections::HashSet;
use std::path::Path;

/// Bump whenever the stored bullet format or the extraction that produces it
/// (`parse_transcript_bullets` / `extract_all_bullets`) changes, so warm rows
/// written by an older skillmon are treated as misses and re-extracted rather
/// than served stale. `(mtime, size)` alone cannot catch a code change that
/// leaves the transcript file byte-identical (ADR 0022).
pub const EXTRACT_LOGIC_VERSION: i64 = 1;

/// Per-transcript memo of the `skill_listing` bullets extracted from each file,
/// so a warm rescan skips re-reading unchanged transcripts (issue #3, ADR
/// 0022). This is Claude-Code-specific (`skill_listing` is a transcript
/// format), so it lives in the adapter (ADR 0002) in its own sqlite file
/// beside the harness-neutral footprint cache. It mirrors `TokenCache`'s
/// shape deliberately: a concrete struct that mutates through `&self` via
/// rusqlite's interior mutability, no trait and no boxed `dyn` -- there is no
/// second implementation and no OS boundary to fake (unlike the keychain
/// store or the HTTP client).
pub struct SqliteListingCache {
    conn: Connection,
}

impl SqliteListingCache {
    pub fn open(path: &Path) -> SqliteResult<Self> {
        Self::init(Connection::open(path)?)
    }

    /// Test-only, like `TokenCache::open_in_memory`: the `build` convenience
    /// wrapper and the store's own tests are the only callers, so it compiles
    /// out of the shipping binary.
    #[cfg(test)]
    pub fn open_in_memory() -> SqliteResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> SqliteResult<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS listing_index (
                path          TEXT PRIMARY KEY,
                mtime_nanos   INTEGER NOT NULL,
                size          INTEGER NOT NULL,
                logic_version INTEGER NOT NULL,
                bullets       TEXT NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    /// The stored bullets for `path`, but only if the row is fresh: an exact
    /// match on `mtime_nanos` AND `size` AND the current `EXTRACT_LOGIC_VERSION`.
    /// Any difference is a miss -- including an *older* stored mtime, so a
    /// same-size in-place rewrite that lands a backwards clock (rsync
    /// `--times` restore, clock skew) still forces a re-read. Strict equality,
    /// never a "file is newer than the memo" comparison (ADR 0022).
    pub fn get(&self, path: &str, mtime_nanos: i64, size: i64) -> Option<Vec<(String, String)>> {
        let row: Option<(i64, i64, i64, String)> = self
            .conn
            .query_row(
                "SELECT mtime_nanos, size, logic_version, bullets FROM listing_index WHERE path = ?1",
                params![path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .expect("listing_index lookup should not fail");

        let (stored_mtime, stored_size, stored_version, bullets_json) = row?;
        if stored_mtime != mtime_nanos || stored_size != size || stored_version != EXTRACT_LOGIC_VERSION {
            return None;
        }
        serde_json::from_str(&bullets_json).ok()
    }

    /// Upserts the extracted bullets for `path`, unconditionally -- including
    /// an empty list for a transcript with no `skill_listing` line. Recording
    /// even empty results is what makes the memo a *negative* cache: without
    /// it, the vast majority of transcripts (which never carry a listing)
    /// would miss on every scan and be re-read forever, and the warm-scan win
    /// would evaporate (ADR 0022).
    pub fn put(&self, path: &str, mtime_nanos: i64, size: i64, bullets: &[(String, String)]) {
        let bullets_json = serde_json::to_string(bullets).expect("bullet list serializes");
        self.conn
            .execute(
                "INSERT INTO listing_index (path, mtime_nanos, size, logic_version, bullets)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(path) DO UPDATE SET
                     mtime_nanos = excluded.mtime_nanos,
                     size = excluded.size,
                     logic_version = excluded.logic_version,
                     bullets = excluded.bullets",
                params![path, mtime_nanos, size, EXTRACT_LOGIC_VERSION, bullets_json],
            )
            .expect("listing_index upsert should not fail");
    }

    /// Drops memo rows for transcripts no longer in `keep_paths`, so the store
    /// can't grow without bound as sessions come and go. Called only from the
    /// full `scan_all` path with every enumerated transcript, never a
    /// partial-scope build -- so a row is only evicted when its file is
    /// genuinely gone from the scan, not merely out of a narrowed view.
    pub fn retain(&self, keep_paths: &HashSet<String>) {
        let existing: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path FROM listing_index")
                .expect("listing_index path scan prepare should not fail");
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .expect("listing_index path scan should not fail");
            rows.collect::<SqliteResult<Vec<String>>>().expect("listing_index path row mapping should not fail")
        };

        for path in existing {
            if !keep_paths.contains(&path) {
                self.conn
                    .execute("DELETE FROM listing_index WHERE path = ?1", params![path])
                    .expect("listing_index prune delete should not fail");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_miss_returns_none() {
        let cache = SqliteListingCache::open_in_memory().unwrap();
        assert!(cache.get("never-seen", 1, 1).is_none());
    }

    #[test]
    fn round_trips_through_a_real_sqlite_file_at_nanosecond_precision() {
        // The test that catches lossy SystemTime serialization: a sub-second
        // nanos mtime and an empty bullet list must survive the real
        // connection byte-identically, or warm hits silently never happen.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("listing_index.sqlite");
        let cache = SqliteListingCache::open(&db).unwrap();

        let mtime_nanos = 1_783_000_000_123_456_789; // sub-second component present
        cache.put("/p/session.jsonl", mtime_nanos, 4096, &[]);
        assert_eq!(cache.get("/p/session.jsonl", mtime_nanos, 4096), Some(vec![]));

        let bullets = vec![
            ("grilling".to_string(), "- grilling: Interview relentlessly.".to_string()),
            ("deploy".to_string(), "- deploy".to_string()),
        ];
        cache.put("/p/other.jsonl", mtime_nanos, 10, &bullets);
        assert_eq!(cache.get("/p/other.jsonl", mtime_nanos, 10), Some(bullets));
        assert!(db.exists());
    }

    #[test]
    fn freshness_gate_is_strict_equality_in_both_directions() {
        let cache = SqliteListingCache::open_in_memory().unwrap();
        let m = 1_000_000_000_000i64;
        cache.put("/p/a.jsonl", m, 500, &[("x".to_string(), "- x".to_string())]);

        assert!(cache.get("/p/a.jsonl", m, 500).is_some(), "exact match hits");
        assert!(cache.get("/p/a.jsonl", m, 501).is_none(), "different size misses");
        assert!(cache.get("/p/a.jsonl", m + 1, 500).is_none(), "newer mtime misses");
        assert!(cache.get("/p/a.jsonl", m - 1, 500).is_none(), "OLDER mtime misses (no newer-than compare)");
    }

    #[test]
    fn a_row_written_under_an_older_logic_version_is_a_miss() {
        let cache = SqliteListingCache::open_in_memory().unwrap();
        let m = 42i64;
        // Simulate a row written before an extraction-logic change.
        cache
            .conn
            .execute(
                "INSERT INTO listing_index (path, mtime_nanos, size, logic_version, bullets)
                 VALUES ('/p/a.jsonl', ?1, 10, ?2, '[]')",
                params![m, EXTRACT_LOGIC_VERSION - 1],
            )
            .unwrap();

        assert!(cache.get("/p/a.jsonl", m, 10).is_none(), "stale logic_version must be re-extracted");
    }

    #[test]
    fn retain_prunes_absent_paths_and_keeps_present_ones() {
        let cache = SqliteListingCache::open_in_memory().unwrap();
        cache.put("/p/gone.jsonl", 1, 1, &[]);
        cache.put("/p/here.jsonl", 1, 1, &[]);

        let keep: HashSet<String> = ["/p/here.jsonl".to_string()].into_iter().collect();
        cache.retain(&keep);

        assert!(cache.get("/p/here.jsonl", 1, 1).is_some(), "present path is kept");
        assert!(cache.get("/p/gone.jsonl", 1, 1).is_none(), "absent path is pruned");
    }
}
