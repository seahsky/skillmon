use crate::domain::footprint::{LayerCount, TokenSource};
use rusqlite::{params, Connection, OptionalExtension, Result as SqliteResult};
use std::collections::HashSet;
use std::path::Path;

/// Bump when the on-demand signature or the summed-`LayerCount` semantics
/// change, so a value written by an older skillmon is treated as a miss and
/// recomputed rather than served stale. Mirrors `EXTRACT_LOGIC_VERSION`: the
/// signature alone cannot catch a code change that leaves the bundled files
/// byte-identical (ADR 0022, issue #11).
pub const ON_DEMAND_LOGIC_VERSION: i64 = 1;

/// Per-skill memo of the *resolved, summed* on-demand ceiling `LayerCount`,
/// so the interactive scan can defer on-demand tokenization entirely and a
/// warm rescan serves the ceiling from here without re-reading or re-hashing
/// the bundled files (issue #11). Storing the summed value -- not a list of
/// per-file hashes -- is the zero-drift anchor: the background pass writes
/// exactly what the eager path (`compute_on_demand`) would produce, and
/// avoids the stat-set-vs-read-set mismatch a hash list would reintroduce.
///
/// Claude-Code-specific (the on-demand ceiling is an adapter concept), so it
/// lives in the adapter (ADR 0002) in its own sqlite file beside the
/// footprint, listing, and usage caches. It mirrors `SqliteListingCache`'s
/// shape: a concrete struct mutating through `&self` via rusqlite's interior
/// mutability, strict `(signature, logic_version)` equality for freshness.
///
/// Opened WAL with a busy timeout so the background fill's connection and the
/// interactive scan's connection never poison each other on a concurrent
/// write (issue #11's non-poisoning requirement).
pub struct SqliteOnDemandCache {
    conn: Connection,
}

impl SqliteOnDemandCache {
    pub fn open(path: &Path) -> SqliteResult<Self> {
        Self::init(Connection::open(path)?)
    }

    /// Test-only, like `SqliteListingCache::open_in_memory`. WAL is a no-op on
    /// an in-memory database, which is harmless: the concurrency the pragma
    /// guards against only exists across the real file's two connections.
    #[cfg(test)]
    pub fn open_in_memory() -> SqliteResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> SqliteResult<Self> {
        // WAL + a busy timeout so the interactive and background connections
        // to the same file tolerate each other's writes instead of erroring
        // out (which the `.expect()` call sites below would turn into a panic
        // that poisons a mutex). Set before any table DDL.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS on_demand_index (
                skill_key     TEXT PRIMARY KEY,
                signature     TEXT NOT NULL,
                tokens        INTEGER NOT NULL,
                source        TEXT NOT NULL,
                logic_version INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    /// The stored ceiling for `skill_key`, but only on an exact match of both
    /// `signature` AND the current `ON_DEMAND_LOGIC_VERSION`. Any difference
    /// is a miss -- the bundled files changed, or the logic that summed them
    /// did. A miss on the interactive path means "pending" (issue #11).
    pub fn get(&self, skill_key: &str, signature: &str) -> Option<LayerCount> {
        let row: Option<(String, i64, String, i64)> = self
            .conn
            .query_row(
                "SELECT signature, tokens, source, logic_version FROM on_demand_index WHERE skill_key = ?1",
                params![skill_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .expect("on_demand_index lookup should not fail");

        let (stored_sig, tokens, source, stored_version) = row?;
        if stored_sig != signature || stored_version != ON_DEMAND_LOGIC_VERSION {
            return None;
        }
        Some(LayerCount { tokens: tokens as u32, source: source_from_str(&source) })
    }

    /// Upserts the resolved ceiling for `skill_key`. Callers (the background
    /// fill) pass exactly the `LayerCount` the eager `compute_on_demand`
    /// produces, so a later `get` on a fresh signature round-trips it byte for
    /// byte.
    pub fn put(&self, skill_key: &str, signature: &str, count: &LayerCount) {
        self.conn
            .execute(
                "INSERT INTO on_demand_index (skill_key, signature, tokens, source, logic_version)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(skill_key) DO UPDATE SET
                     signature = excluded.signature,
                     tokens = excluded.tokens,
                     source = excluded.source,
                     logic_version = excluded.logic_version",
                params![skill_key, signature, count.tokens, source_to_str(count.source), ON_DEMAND_LOGIC_VERSION],
            )
            .expect("on_demand_index upsert should not fail");
    }

    /// Drops rows for skills no longer discovered, so the memo can't grow
    /// without bound as skills come and go. Called only from the full
    /// `scan_all` path with every discovered skill's key.
    pub fn retain(&self, keep: &HashSet<String>) {
        let existing: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT skill_key FROM on_demand_index")
                .expect("on_demand_index key scan prepare should not fail");
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .expect("on_demand_index key scan should not fail");
            rows.collect::<SqliteResult<Vec<String>>>().expect("on_demand_index key row mapping should not fail")
        };

        for key in existing {
            if !keep.contains(&key) {
                self.conn
                    .execute("DELETE FROM on_demand_index WHERE skill_key = ?1", params![key])
                    .expect("on_demand_index prune delete should not fail");
            }
        }
    }
}

/// The on-demand sum is only as trustworthy as its least-exact component
/// (ADR 0017), so the memo persists exactly `Exact` or `Estimate`; an unknown
/// string can only mean a corrupt row, which is treated as the safer
/// `Estimate` rather than silently claiming an exact count.
fn source_to_str(source: TokenSource) -> &'static str {
    match source {
        TokenSource::Exact => "exact",
        TokenSource::Estimate => "estimate",
    }
}

fn source_from_str(s: &str) -> TokenSource {
    match s {
        "exact" => TokenSource::Exact,
        _ => TokenSource::Estimate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(tokens: u32, source: TokenSource) -> LayerCount {
        LayerCount { tokens, source }
    }

    #[test]
    fn fresh_miss_returns_none() {
        let cache = SqliteOnDemandCache::open_in_memory().unwrap();
        assert!(cache.get("never-seen", "sig").is_none());
    }

    #[test]
    fn round_trips_a_layer_count_through_a_real_sqlite_file() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("on_demand_index.sqlite");
        let cache = SqliteOnDemandCache::open(&db).unwrap();

        cache.put("/s/grilling", "sig-a", &layer(1234, TokenSource::Estimate));
        assert_eq!(cache.get("/s/grilling", "sig-a"), Some(layer(1234, TokenSource::Estimate)));

        // Exact source survives round-trip distinctly from estimate.
        cache.put("/s/deploy", "sig-b", &layer(42, TokenSource::Exact));
        assert_eq!(cache.get("/s/deploy", "sig-b"), Some(layer(42, TokenSource::Exact)));
        assert!(db.exists());
    }

    #[test]
    fn a_different_signature_is_a_miss() {
        let cache = SqliteOnDemandCache::open_in_memory().unwrap();
        cache.put("/s/grilling", "sig-a", &layer(10, TokenSource::Exact));

        assert!(cache.get("/s/grilling", "sig-a").is_some(), "matching signature hits");
        assert!(cache.get("/s/grilling", "sig-changed").is_none(), "changed signature misses");
    }

    #[test]
    fn a_stale_logic_version_is_a_miss() {
        let cache = SqliteOnDemandCache::open_in_memory().unwrap();
        cache
            .conn
            .execute(
                "INSERT INTO on_demand_index (skill_key, signature, tokens, source, logic_version)
                 VALUES ('/s/a', 'sig', 5, 'estimate', ?1)",
                params![ON_DEMAND_LOGIC_VERSION - 1],
            )
            .unwrap();

        assert!(cache.get("/s/a", "sig").is_none(), "stale logic_version must be recomputed");
    }

    #[test]
    fn put_upserts_a_changed_signature_in_place() {
        let cache = SqliteOnDemandCache::open_in_memory().unwrap();
        cache.put("/s/a", "sig-old", &layer(5, TokenSource::Estimate));
        cache.put("/s/a", "sig-new", &layer(9, TokenSource::Exact));

        assert!(cache.get("/s/a", "sig-old").is_none(), "the old signature no longer resolves");
        assert_eq!(cache.get("/s/a", "sig-new"), Some(layer(9, TokenSource::Exact)));
    }

    #[test]
    fn retain_prunes_absent_skill_keys_and_keeps_present_ones() {
        let cache = SqliteOnDemandCache::open_in_memory().unwrap();
        cache.put("/s/gone", "sig", &layer(1, TokenSource::Exact));
        cache.put("/s/here", "sig", &layer(2, TokenSource::Exact));

        let keep: HashSet<String> = ["/s/here".to_string()].into_iter().collect();
        cache.retain(&keep);

        assert!(cache.get("/s/here", "sig").is_some(), "present key is kept");
        assert!(cache.get("/s/gone", "sig").is_none(), "absent key is pruned");
    }
}
