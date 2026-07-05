use rusqlite::{params, Connection, OptionalExtension, Result as SqliteResult};
use std::path::Path;

/// Content-addressed: one row per `content_hash`, no skill or layer in the
/// key (ADR 0006 + ADR 0018 -- there is only ever one live reference model,
/// so `exact_model_id` is a staleness-check column, not part of the key).
///
/// `tiktoken_count` keeps its name for cache/schema stability, but since the
/// issue #2 swap it holds the `bpe-openai` `o200k` estimate, which is
/// byte-for-byte identical to the old `tiktoken-rs` value (ADR 0006 update),
/// so no migration or cache wipe was needed.
pub struct CachedEntry {
    pub tiktoken_count: u32,
    pub exact: Option<(u32, String)>,
}

pub struct TokenCache {
    conn: Connection,
}

impl TokenCache {
    pub fn open(path: &Path) -> SqliteResult<Self> {
        Self::init(Connection::open(path)?)
    }

    /// Test-only: every caller lives in a `#[cfg(test)]` block, so this is
    /// compiled out of the shipping binary rather than flagged dead there.
    #[cfg(test)]
    pub fn open_in_memory() -> SqliteResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> SqliteResult<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS token_cache (
                content_hash   TEXT PRIMARY KEY,
                tiktoken_count INTEGER NOT NULL,
                exact_tokens   INTEGER,
                exact_model_id TEXT,
                computed_at    TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS calibration (
                model_id     TEXT PRIMARY KEY,
                factor       REAL NOT NULL,
                sample_count INTEGER NOT NULL,
                updated_at   TEXT NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn get(&self, content_hash: &str) -> Option<CachedEntry> {
        self.conn
            .query_row(
                "SELECT tiktoken_count, exact_tokens, exact_model_id FROM token_cache WHERE content_hash = ?1",
                params![content_hash],
                |row| {
                    let tiktoken_count: i64 = row.get(0)?;
                    let exact_tokens: Option<i64> = row.get(1)?;
                    let exact_model_id: Option<String> = row.get(2)?;
                    Ok(CachedEntry {
                        tiktoken_count: tiktoken_count as u32,
                        exact: exact_tokens.zip(exact_model_id).map(|(t, m)| (t as u32, m)),
                    })
                },
            )
            .optional()
            .expect("token_cache lookup should not fail")
    }

    /// Upserts the always-computed tiktoken count. Never touches the
    /// `exact_*` columns, so it's safe to call even when an exact value is
    /// already cached for this hash.
    pub fn put_tiktoken(&self, content_hash: &str, tiktoken_count: u32) {
        self.conn
            .execute(
                "INSERT INTO token_cache (content_hash, tiktoken_count, computed_at)
                 VALUES (?1, ?2, datetime('now'))
                 ON CONFLICT(content_hash) DO UPDATE SET
                     tiktoken_count = excluded.tiktoken_count,
                     computed_at = excluded.computed_at",
                params![content_hash, tiktoken_count],
            )
            .expect("token_cache tiktoken upsert should not fail");
    }

    /// Upserts the exact count and its reference model, then recomputes that
    /// model's calibration factor. Assumes `put_tiktoken` has already been
    /// called for this hash (the compute orchestration always does tiktoken
    /// first); a bare `put_exact` on a never-seen hash stores `0` as a
    /// placeholder tiktoken count until a later `put_tiktoken` call corrects it.
    pub fn put_exact(&self, content_hash: &str, exact_tokens: u32, model_id: &str) {
        self.conn
            .execute(
                "INSERT INTO token_cache (content_hash, tiktoken_count, exact_tokens, exact_model_id, computed_at)
                 VALUES (?1, 0, ?2, ?3, datetime('now'))
                 ON CONFLICT(content_hash) DO UPDATE SET
                     exact_tokens = excluded.exact_tokens,
                     exact_model_id = excluded.exact_model_id,
                     computed_at = excluded.computed_at",
                params![content_hash, exact_tokens, model_id],
            )
            .expect("token_cache exact upsert should not fail");

        self.recompute_calibration(model_id);
    }

    fn recompute_calibration(&self, model_id: &str) {
        let (sum_exact, sum_tiktoken, sample_count): (i64, i64, i64) = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(exact_tokens), 0), COALESCE(SUM(tiktoken_count), 0), COUNT(*)
                 FROM token_cache WHERE exact_model_id = ?1",
                params![model_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("calibration aggregate query should not fail");

        if sum_tiktoken == 0 {
            return;
        }

        let factor = sum_exact as f64 / sum_tiktoken as f64;
        self.conn
            .execute(
                "INSERT INTO calibration (model_id, factor, sample_count, updated_at)
                 VALUES (?1, ?2, ?3, datetime('now'))
                 ON CONFLICT(model_id) DO UPDATE SET
                     factor = excluded.factor,
                     sample_count = excluded.sample_count,
                     updated_at = excluded.updated_at",
                params![model_id, factor, sample_count],
            )
            .expect("calibration upsert should not fail");
    }

    pub fn calibration_factor(&self, model_id: &str) -> Option<f64> {
        self.conn
            .query_row("SELECT factor FROM calibration WHERE model_id = ?1", params![model_id], |row| row.get(0))
            .optional()
            .expect("calibration lookup should not fail")
    }

    /// Hashes whose cached exact value was measured against a reference
    /// model other than `current_model_id` (ADR 0018 -- skillmon bumped its
    /// internal default). Hashes with no exact value at all are not stale,
    /// they're simply un-upgraded estimates.
    pub fn stale_exact_hashes(&self, current_model_id: &str) -> Vec<String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT content_hash FROM token_cache
                 WHERE exact_model_id IS NOT NULL AND exact_model_id != ?1",
            )
            .expect("stale_exact_hashes prepare should not fail");
        stmt.query_map(params![current_model_id], |row| row.get(0))
            .expect("stale_exact_hashes query should not fail")
            .collect::<SqliteResult<Vec<String>>>()
            .expect("stale_exact_hashes row mapping should not fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_miss_returns_none() {
        let cache = TokenCache::open_in_memory().unwrap();
        assert!(cache.get("never-seen-hash").is_none());
    }

    #[test]
    fn opens_a_real_file_and_round_trips_a_tiktoken_count() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("footprint.sqlite");
        let cache = TokenCache::open(&db_path).unwrap();

        cache.put_tiktoken("hash-a", 100);
        let entry = cache.get("hash-a").unwrap();

        assert_eq!(entry.tiktoken_count, 100);
        assert!(entry.exact.is_none());
        assert!(db_path.exists());
    }

    #[test]
    fn put_exact_preserves_the_tiktoken_count_already_on_the_row() {
        let cache = TokenCache::open_in_memory().unwrap();
        cache.put_tiktoken("hash-a", 100);
        cache.put_exact("hash-a", 120, "model-a");

        let entry = cache.get("hash-a").unwrap();

        assert_eq!(entry.tiktoken_count, 100);
        assert_eq!(entry.exact, Some((120, "model-a".to_string())));
    }

    #[test]
    fn a_later_exact_call_under_a_new_model_id_overwrites_the_stale_one() {
        let cache = TokenCache::open_in_memory().unwrap();
        cache.put_tiktoken("hash-a", 100);
        cache.put_exact("hash-a", 120, "model-a");
        cache.put_exact("hash-a", 130, "model-b");

        let entry = cache.get("hash-a").unwrap();

        assert_eq!(entry.exact, Some((130, "model-b".to_string())));
    }

    #[test]
    fn calibration_factor_is_none_until_an_exact_sample_exists_then_updates() {
        let cache = TokenCache::open_in_memory().unwrap();
        assert!(cache.calibration_factor("model-a").is_none());

        cache.put_tiktoken("hash-a", 100);
        cache.put_exact("hash-a", 120, "model-a");
        let factor = cache.calibration_factor("model-a").unwrap();
        assert!((factor - 1.2).abs() < 1e-9, "expected ~1.2, got {factor}");

        cache.put_tiktoken("hash-b", 200);
        cache.put_exact("hash-b", 220, "model-a");
        let factor = cache.calibration_factor("model-a").unwrap();
        // (120 + 220) / (100 + 200) = 340 / 300
        assert!((factor - (340.0 / 300.0)).abs() < 1e-9, "expected ~1.1333, got {factor}");
    }

    #[test]
    fn stale_exact_hashes_is_empty_when_no_rows_have_an_exact_value() {
        let cache = TokenCache::open_in_memory().unwrap();
        cache.put_tiktoken("hash-a", 100);
        assert!(cache.stale_exact_hashes("model-current").is_empty());
    }

    #[test]
    fn stale_exact_hashes_is_empty_when_every_exact_row_matches_the_current_model() {
        let cache = TokenCache::open_in_memory().unwrap();
        cache.put_tiktoken("hash-a", 100);
        cache.put_exact("hash-a", 120, "model-current");
        assert!(cache.stale_exact_hashes("model-current").is_empty());
    }

    #[test]
    fn stale_exact_hashes_returns_rows_measured_against_a_different_model() {
        let cache = TokenCache::open_in_memory().unwrap();
        cache.put_tiktoken("hash-a", 100);
        cache.put_exact("hash-a", 120, "model-old");
        cache.put_tiktoken("hash-b", 200);
        cache.put_exact("hash-b", 240, "model-current");

        assert_eq!(cache.stale_exact_hashes("model-current"), vec!["hash-a".to_string()]);
    }
}
