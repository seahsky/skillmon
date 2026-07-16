//! The trash ledger: which entries skillmon moved out of the scan root, where
//! it put them, and which skills are tombstoned (ADR 0029).
//!
//! **This store is authoritative state, not a cache, and that changes its
//! migration rules.** `SqliteListingCache` and `SqliteUsageCache` may drop their
//! tables on a logic bump because everything in them can be re-derived from
//! transcripts on disk (ADR 0022, ADR 0024). Nothing in here can be re-derived
//! from anything: these rows are the *only* record of where a trashed entry came
//! from. Dropping them would leave a gigabyte of the user's files staged under
//! `skillmon/removed/` with no undo and no way to name what they were. Any
//! future schema change must be an additive `ALTER`, or a real migration that
//! carries the rows across -- never a DROP.
//!
//! Harness-neutral (ADR 0002): a `SkillId`, an origin path, and a byte count are
//! not Claude Code facts. The adapter supplies the roots (`paths::removed_dir`);
//! this module never names one.

use rusqlite::types::Type;
use rusqlite::{params, Connection, Error as SqliteError, OptionalExtension, Result as SqliteResult, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::domain::removal::{Retention, Tombstone, TrashUnit, TrashUnitId, TrashedEntry, TrashedSource};
use crate::domain::skill::SkillId;

/// The on-disk encoding of a skill identity.
///
/// Deliberately **its own type, not `SkillRef`**, even though the two mirror the
/// same domain value and `SkillRef` already round-trips losslessly. `SkillRef`
/// is an IPC DTO: its `rename_all`/`tag` attributes exist to serve the panel and
/// are free to change with it. This encoding is pinned forever, for the same
/// reason `Retention::as_str` is hand-written -- a key this store can no longer
/// parse is a trash unit whose files it can no longer give back. Coupling the
/// two would let a panel-driven rename silently orphan every tombstone.
///
/// JSON rather than a separator-joined string because it escapes for free: a
/// `repo_path` is an arbitrary filesystem path, and no hand-picked delimiter is
/// safely absent from one.
///
/// Carries no plugin version, so an upgrade cannot orphan a tombstone
/// (CONTEXT.md "Skill identity").
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum StoredSkillKey {
    Personal { name: String },
    Project { repo_path: String, name: String },
    Plugin { marketplace: String, plugin: String, name: String },
}

impl From<&SkillId> for StoredSkillKey {
    fn from(id: &SkillId) -> Self {
        match id {
            SkillId::Personal { name } => StoredSkillKey::Personal { name: name.clone() },
            SkillId::Project { repo_path, name } => {
                StoredSkillKey::Project { repo_path: repo_path.to_string_lossy().into_owned(), name: name.clone() }
            }
            SkillId::Plugin { marketplace, plugin, name } => StoredSkillKey::Plugin {
                marketplace: marketplace.clone(),
                plugin: plugin.clone(),
                name: name.clone(),
            },
        }
    }
}

impl From<StoredSkillKey> for SkillId {
    fn from(key: StoredSkillKey) -> Self {
        match key {
            StoredSkillKey::Personal { name } => SkillId::Personal { name },
            StoredSkillKey::Project { repo_path, name } => {
                SkillId::Project { repo_path: PathBuf::from(repo_path), name }
            }
            StoredSkillKey::Plugin { marketplace, plugin, name } => {
                SkillId::Plugin { marketplace, plugin, name }
            }
        }
    }
}

fn skill_key(id: &SkillId) -> String {
    serde_json::to_string(&StoredSkillKey::from(id)).expect("a StoredSkillKey is plain data and always serializes")
}

/// Parses a key back, **raising** on one this build cannot understand rather
/// than skipping the row.
///
/// Every key here was written by skillmon against a pinned encoding, so an
/// unparseable one is corruption, not an expected variant. Dropping it quietly
/// would be worst exactly where it matters most: `entries_of` feeds `purge`, so
/// a skipped dependent would have its bytes left on disk while the cascade
/// deleted the only row naming them -- the "gigabyte staged with no undo and no
/// way to name what they were" this module exists to prevent.
fn parse_skill_key(raw: &str, column: usize) -> SqliteResult<SkillId> {
    serde_json::from_str::<StoredSkillKey>(raw)
        .map(SkillId::from)
        .map_err(|e| SqliteError::FromSqlConversionFailure(column, Type::Text, Box::new(e)))
}

/// The columns issue #31 added to `trash_entry`, and the whole of the migration
/// from the shape issue #28 created.
///
/// Every one is nullable, which is what makes adding them additive: a row
/// written by the older build reads back as an entry-only removal, which is
/// exactly what it was. All four move together -- a source is a path, a staged
/// path, a size, and the tool's bookkeeping, and any one of them without the
/// others describes nothing.
/// Carries each column's declared type, so a ledger that reaches this shape by
/// migration is identical to one created at it. SQLite would tolerate the
/// mismatch -- typing is per-value, not per-column -- which is exactly why it is
/// worth being deliberate: a `source_bytes` that is INTEGER on a fresh install
/// and TEXT on an upgraded one is a difference nothing would report until
/// something read it back.
const SOURCE_COLUMNS: &[(&str, &str)] = &[
    ("source_origin_path", "TEXT"),
    ("source_stored_path", "TEXT"),
    ("source_bytes", "INTEGER"),
    ("source_state", "TEXT"),
];

/// Adds any of `columns` the table does not already have.
///
/// `CREATE TABLE IF NOT EXISTS` is a no-op against an existing table, so the
/// widened definition above reaches a *new* ledger only. Anyone who ran the
/// issue #28 build has the old four-column table already, and it must be carried
/// across rather than recreated: this store is authoritative state, and dropping
/// it would strand real files with no undo (see the module docs).
///
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, so the existing columns are read
/// first. Every added column is nullable and has no default, which SQLite can
/// always do in O(1) -- there is no table rewrite here, however large the ledger.
fn add_missing_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> SqliteResult<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing: HashSet<String> = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<SqliteResult<_>>()?;
    for (column, ty) in columns.iter().filter(|(c, _)| !existing.contains(*c)) {
        // Interpolated, not bound: SQLite takes no parameter in DDL. The values
        // are this module's own `const`s, never anything a user or a file
        // supplies, so there is nothing here to inject.
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
    }
    Ok(())
}

/// One `trash_entry` row as SQLite hands it over, before it is a domain value.
///
/// A named struct rather than a nine-wide tuple: every field but two is a
/// `String`, so a transposition in the destructuring would compile and quietly
/// swap a skill's origin with where its bytes are staged -- on the read path
/// that a purge and a restore both go through.
struct StoredEntryRow {
    skill_id: String,
    declared_name: String,
    origin_path: String,
    stored_path: String,
    bytes: i64,
    source_origin_path: Option<String>,
    source_stored_path: Option<String>,
    source_bytes: Option<i64>,
    source_state: Option<String>,
}

impl StoredEntryRow {
    fn into_entry(self) -> SqliteResult<TrashedEntry> {
        let source = self.source()?;
        Ok(TrashedEntry {
            skill_id: parse_skill_key(&self.skill_id, 0)?,
            declared_name: self.declared_name,
            origin_path: PathBuf::from(self.origin_path),
            stored_path: PathBuf::from(self.stored_path),
            bytes: parse_bytes(self.bytes, 4)?,
            source,
        })
    }

    /// The three source columns are written together or not at all, so reading
    /// them back demands the same. A row carrying only some of them is not an
    /// entry-only removal that can be shrugged off -- it is a row describing a
    /// source whose location or size this build cannot name, and treating it as
    /// `None` would leave the staged bytes on disk while the purge deleted the
    /// only row pointing at them. `source_state` is excluded: a tool with
    /// nothing recorded to drop legitimately returns `None` for it.
    fn source(&self) -> SqliteResult<Option<TrashedSource>> {
        match (&self.source_origin_path, &self.source_stored_path, self.source_bytes) {
            (None, None, None) => Ok(None),
            (Some(origin), Some(stored), Some(bytes)) => Ok(Some(TrashedSource {
                origin_path: PathBuf::from(origin),
                stored_path: PathBuf::from(stored),
                bytes: parse_bytes(bytes, 7)?,
                state: self.source_state.clone(),
            })),
            _ => Err(SqliteError::FromSqlConversionFailure(
                5,
                Type::Text,
                format!(
                    "trash entry for {} has a partial source record (origin: {}, stored: {}, bytes: {})",
                    self.declared_name,
                    self.source_origin_path.is_some(),
                    self.source_stored_path.is_some(),
                    self.source_bytes.is_some(),
                )
                .into(),
            )),
        }
    }
}

/// A negative size is corruption, not a big number: `as u64` would turn -1 into
/// 18 exabytes and offer to reclaim it.
fn parse_bytes(raw: i64, column: usize) -> SqliteResult<u64> {
    u64::try_from(raw).map_err(|e| SqliteError::FromSqlConversionFailure(column, Type::Integer, Box::new(e)))
}

pub struct TrashStore {
    conn: Connection,
}

impl TrashStore {
    pub fn open(path: &Path) -> SqliteResult<Self> {
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> SqliteResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> SqliteResult<Self> {
        // No logic-version wipe, unlike every other store in this crate -- see
        // the module docs. CREATE IF NOT EXISTS only, so an existing ledger is
        // never touched.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS trash_unit (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                retention  TEXT NOT NULL,
                removed_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS trash_entry (
                unit_id       INTEGER NOT NULL REFERENCES trash_unit(id) ON DELETE CASCADE,
                ordinal       INTEGER NOT NULL,
                skill_id      TEXT NOT NULL,
                declared_name TEXT NOT NULL,
                origin_path   TEXT NOT NULL,
                stored_path   TEXT NOT NULL,
                bytes         INTEGER NOT NULL,
                -- The managing tool's own copy, when the user opted into
                -- removing it too (issue #31, ADR 0027). All four are NULL for
                -- entry-only removal, which is the rule and the default.
                -- `source_state` is the tool's bookkeeping, stored verbatim and
                -- never parsed here: this ledger is harness- AND tool-neutral,
                -- and hands the blob back to whoever wrote it.
                source_origin_path TEXT,
                source_stored_path TEXT,
                source_bytes       INTEGER,
                source_state       TEXT,
                PRIMARY KEY (unit_id, ordinal)
            );
            CREATE TABLE IF NOT EXISTS tombstone (
                skill_id      TEXT PRIMARY KEY,
                declared_name TEXT NOT NULL,
                removed_at    INTEGER NOT NULL
            );",
        )?;
        // The ON DELETE CASCADE above is inert without this: SQLite disables
        // foreign keys per-connection by default, so forgetting a unit would
        // silently strand its entries.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        add_missing_columns(&conn, "trash_entry", SOURCE_COLUMNS)?;
        Ok(Self { conn })
    }

    /// Opens the transaction a removal runs inside. The caller commits only once
    /// every filesystem move has landed, so a crash can leave staged bytes with
    /// no unit row (an inert directory) but never a unit row with no bytes (an
    /// undo that restores nothing) -- ADR 0029's deliberate bias.
    pub fn transaction(&mut self) -> SqliteResult<Transaction<'_>> {
        self.conn.transaction()
    }

    /// Reserves a unit id inside `tx`, before its entries exist -- the id names
    /// the storage directory the moves are about to write into, so it has to
    /// come first.
    ///
    /// `AUTOINCREMENT` (not a bare rowid) so an id is not handed out again after
    /// a delete. Reuse would point a fresh unit at a purged unit's directory
    /// path; a rolled-back id *can* still recur, which is why `remove` clears the
    /// storage directory before staging into it.
    pub fn insert_unit(tx: &Transaction<'_>, retention: Retention, now_millis: i64) -> SqliteResult<TrashUnitId> {
        tx.execute(
            "INSERT INTO trash_unit (retention, removed_at) VALUES (?1, ?2)",
            params![retention.as_str(), now_millis],
        )?;
        Ok(TrashUnitId(tx.last_insert_rowid()))
    }

    /// Records the entries of a staged unit, in `entries()` order: ordinal 0 is
    /// the primary, the rest are its cascaded dependents (ADR 0027).
    pub fn insert_entries(tx: &Transaction<'_>, id: TrashUnitId, entries: &[TrashedEntry]) -> SqliteResult<()> {
        for (ordinal, e) in entries.iter().enumerate() {
            let source = e.source.as_ref();
            tx.execute(
                "INSERT INTO trash_entry (unit_id, ordinal, skill_id, declared_name, origin_path, stored_path, bytes,
                                          source_origin_path, source_stored_path, source_bytes, source_state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    id.0,
                    ordinal as i64,
                    skill_key(&e.skill_id),
                    e.declared_name,
                    e.origin_path.to_string_lossy(),
                    e.stored_path.to_string_lossy(),
                    e.bytes as i64,
                    source.map(|s| s.origin_path.to_string_lossy().into_owned()),
                    source.map(|s| s.stored_path.to_string_lossy().into_owned()),
                    source.map(|s| s.bytes as i64),
                    source.and_then(|s| s.state.clone()),
                ],
            )?;
        }
        Ok(())
    }

    /// Tombstones every skill in a unit. Called only for `Trashed` units:
    /// disabling is not removing, and filing a re-enableable row under "removed"
    /// would be a lie (ADR 0029).
    ///
    /// `ON CONFLICT DO UPDATE` because a skill can be removed, reinstalled, and
    /// removed again; the latest removal is the one the row should describe.
    pub fn insert_tombstones(tx: &Transaction<'_>, entries: &[TrashedEntry], now_millis: i64) -> SqliteResult<()> {
        for e in entries {
            tx.execute(
                "INSERT INTO tombstone (skill_id, declared_name, removed_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(skill_id) DO UPDATE SET declared_name = excluded.declared_name,
                                                     removed_at = excluded.removed_at",
                params![skill_key(&e.skill_id), e.declared_name, now_millis],
            )?;
        }
        Ok(())
    }

    pub fn get(&self, id: TrashUnitId) -> SqliteResult<Option<TrashUnit>> {
        let header: Option<(String, i64)> = self
            .conn
            .query_row("SELECT retention, removed_at FROM trash_unit WHERE id = ?1", params![id.0], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        // `None` means no such unit, which is an ordinary answer. A row that
        // exists but cannot be read is not -- it is corruption of authoritative
        // state, and must surface rather than masquerade as "already gone",
        // which would invite a caller to conclude there is nothing to give back.
        let Some((retention, removed_at_millis)) = header else { return Ok(None) };
        let retention = Retention::parse(&retention).ok_or_else(|| {
            SqliteError::FromSqlConversionFailure(
                0,
                Type::Text,
                format!("trash unit {} has unknown retention {retention:?}", id.0).into(),
            )
        })?;

        let entries = self.entries_of(id)?;
        let mut entries = entries.into_iter();
        let primary = entries.next().ok_or_else(|| {
            SqliteError::FromSqlConversionFailure(
                0,
                Type::Integer,
                format!("trash unit {} has no entries", id.0).into(),
            )
        })?;

        Ok(Some(TrashUnit {
            id,
            retention,
            removed_at_millis,
            primary,
            dependents: entries.collect(),
        }))
    }

    fn entries_of(&self, id: TrashUnitId) -> SqliteResult<Vec<TrashedEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT skill_id, declared_name, origin_path, stored_path, bytes,
                    source_origin_path, source_stored_path, source_bytes, source_state
             FROM trash_entry WHERE unit_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![id.0], |r| {
            Ok(StoredEntryRow {
                skill_id: r.get(0)?,
                declared_name: r.get(1)?,
                origin_path: r.get(2)?,
                stored_path: r.get(3)?,
                bytes: r.get(4)?,
                source_origin_path: r.get(5)?,
                source_stored_path: r.get(6)?,
                source_bytes: r.get(7)?,
                source_state: r.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.into_entry()?);
        }
        Ok(out)
    }

    /// Every unit, newest first -- the order the removed view reads in, and the
    /// order a user reasons about an undo in.
    pub fn list(&self) -> SqliteResult<Vec<TrashUnit>> {
        let mut stmt = self.conn.prepare("SELECT id FROM trash_unit ORDER BY removed_at DESC, id DESC")?;
        let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.collect::<SqliteResult<_>>()?;
        let mut out = Vec::new();
        for id in ids {
            if let Some(unit) = self.get(TrashUnitId(id))? {
                out.push(unit);
            }
        }
        Ok(out)
    }

    /// Drops a restored unit and clears the tombstone of every skill in it: the
    /// entries are back under the scan root, so the rows are listed again.
    pub fn forget_restored(&mut self, unit: &TrashUnit) -> SqliteResult<()> {
        let tx = self.conn.transaction()?;
        for e in unit.entries() {
            tx.execute("DELETE FROM tombstone WHERE skill_id = ?1", params![skill_key(&e.skill_id)])?;
        }
        tx.execute("DELETE FROM trash_unit WHERE id = ?1", params![unit.id.0])?;
        tx.commit()
    }

    /// Drops a purged unit and **keeps every tombstone**. The bytes were the
    /// undo; the tombstone is the history. Reclaiming the first must not touch
    /// the second, or a user could not both free a gigabyte and keep their
    /// totals honest (DESIGN.md UX #6, ADR 0029).
    pub fn forget_purged(&mut self, unit: &TrashUnit) -> SqliteResult<()> {
        self.conn.execute("DELETE FROM trash_unit WHERE id = ?1", params![unit.id.0]).map(|_| ())
    }

    /// Every removed-but-not-reinstalled skill, newest first.
    ///
    /// Reads the rows DESIGN.md UX #6 exists for, and the only handle the panel
    /// has on a skill whose bytes are already reclaimed.
    pub fn tombstones(&self) -> SqliteResult<Vec<Tombstone>> {
        let mut stmt =
            self.conn.prepare("SELECT skill_id, declared_name, removed_at FROM tombstone ORDER BY removed_at DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (skill_id, declared_name, removed_at_millis) = row?;
            out.push(Tombstone { skill_id: parse_skill_key(&skill_id, 0)?, declared_name, removed_at_millis });
        }
        Ok(out)
    }

    /// Clears the tombstone of every skill a scan can currently see, and reports
    /// how many it cleared.
    ///
    /// This -- not restore -- is where DESIGN #6's "reinstalling restores
    /// continuity" actually happens, because rediscovery covers the case the
    /// ledger cannot see: a user reinstalling by hand, past skillmon entirely.
    /// Restore's own tombstone clear is then a special case of one rule rather
    /// than a second mechanism.
    ///
    /// Usage history needs no restoring; it was never deleted (ADR 0024). The
    /// tombstone gates only whether the row is listed.
    pub fn reconcile_tombstones(&mut self, discovered: &[SkillId]) -> SqliteResult<usize> {
        if discovered.is_empty() {
            return Ok(0);
        }
        let live: HashSet<String> = discovered.iter().map(skill_key).collect();
        let tx = self.conn.transaction()?;
        let mut cleared = 0;
        for key in live {
            cleared += tx.execute("DELETE FROM tombstone WHERE skill_id = ?1", params![key])?;
        }
        tx.commit()?;
        Ok(cleared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `trash_entry` exactly as the issue #28 build created it, with no source
    /// columns. Spelled out rather than derived from the current DDL, because
    /// the point is to reproduce a shape that no longer exists in this file --
    /// generating it from today's schema would test nothing.
    const ISSUE_28_SCHEMA: &str = "CREATE TABLE trash_unit (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            retention  TEXT NOT NULL,
            removed_at INTEGER NOT NULL
        );
        CREATE TABLE trash_entry (
            unit_id       INTEGER NOT NULL REFERENCES trash_unit(id) ON DELETE CASCADE,
            ordinal       INTEGER NOT NULL,
            skill_id      TEXT NOT NULL,
            declared_name TEXT NOT NULL,
            origin_path   TEXT NOT NULL,
            stored_path   TEXT NOT NULL,
            bytes         INTEGER NOT NULL,
            PRIMARY KEY (unit_id, ordinal)
        );
        CREATE TABLE tombstone (
            skill_id      TEXT PRIMARY KEY,
            declared_name TEXT NOT NULL,
            removed_at    INTEGER NOT NULL
        );";

    /// The upgrade path, and the one this store's docs forbid getting wrong: a
    /// ledger is authoritative state, so issue #31's columns have to be added to
    /// the existing table rather than arrive by recreating it. `CREATE TABLE IF
    /// NOT EXISTS` is a no-op against a table that is already there, so without
    /// the migration the new columns would simply never appear -- and every
    /// insert would fail on a machine that had ever run the older build.
    #[test]
    fn opening_a_ledger_from_the_issue_28_build_adds_the_source_columns_and_keeps_its_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("removal.sqlite");

        let old = Connection::open(&path).unwrap();
        old.execute_batch(ISSUE_28_SCHEMA).unwrap();
        old.execute("INSERT INTO trash_unit (id, retention, removed_at) VALUES (1, 'trashed', 1000)", []).unwrap();
        old.execute(
            "INSERT INTO trash_entry (unit_id, ordinal, skill_id, declared_name, origin_path, stored_path, bytes)
             VALUES (1, 0, ?1, 'vercel-react', '/home/me/.claude/skills/vercel-react', '/staged/0-vercel-react', 12)",
            params![skill_key(&SkillId::Personal { name: "vercel-react".to_string() })],
        )
        .unwrap();
        drop(old);

        let store = TrashStore::open(&path).unwrap();
        let unit = store.get(TrashUnitId(1)).unwrap().expect("the pre-existing unit must survive the upgrade");

        assert_eq!(unit.primary.declared_name, "vercel-react");
        assert_eq!(unit.primary.origin_path, PathBuf::from("/home/me/.claude/skills/vercel-react"));
        assert_eq!(unit.bytes(), 12);
        assert_eq!(unit.primary.source, None, "a row written before source removal existed had no source");
    }

    /// Opening is what runs the migration, and the panel opens the ledger on
    /// every launch -- so it has to be safe to run against an already-migrated
    /// file, forever.
    #[test]
    fn opening_an_already_migrated_ledger_again_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("removal.sqlite");

        drop(TrashStore::open(&path).unwrap());
        let mut store = TrashStore::open(&path).unwrap();

        let tx = store.transaction().unwrap();
        let id = TrashStore::insert_unit(&tx, Retention::Trashed, 1_000).unwrap();
        TrashStore::insert_entries(&tx, id, &[entry("tdd", 5)]).unwrap();
        tx.commit().unwrap();
        assert!(store.get(id).unwrap().is_some(), "a twice-opened ledger still takes writes");
    }

    /// A source is a path, a staged path, and a size, written together. A row
    /// carrying only some of them describes a directory this build cannot find
    /// or size -- reading it as "no source" would leave those bytes on disk
    /// while the purge deleted the only row naming them.
    #[test]
    fn a_partially_written_source_is_corruption_rather_than_no_source() {
        let mut store = TrashStore::open_in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let id = TrashStore::insert_unit(&tx, Retention::Trashed, 1_000).unwrap();
        TrashStore::insert_entries(&tx, id, &[entry("tdd", 5)]).unwrap();
        tx.commit().unwrap();
        store
            .conn
            .execute("UPDATE trash_entry SET source_origin_path = '/home/me/.agents/skills/tdd'", [])
            .unwrap();

        assert!(store.get(id).is_err(), "a half-written source must surface, not be shrugged off as absent");
    }

    fn entry(name: &str, bytes: u64) -> TrashedEntry {
        TrashedEntry {
            skill_id: SkillId::Personal { name: name.to_string() },
            declared_name: name.to_string(),
            origin_path: PathBuf::from(format!("/home/me/.claude/skills/{name}")),
            stored_path: PathBuf::from(format!("/home/me/.claude/skillmon/removed/1/0-{name}")),
            bytes,
            source: None,
        }
    }

    fn record(store: &mut TrashStore, retention: Retention, entries: &[TrashedEntry], now: i64) -> TrashUnitId {
        let tx = store.transaction().unwrap();
        let id = TrashStore::insert_unit(&tx, retention, now).unwrap();
        TrashStore::insert_entries(&tx, id, entries).unwrap();
        if retention == Retention::Trashed {
            TrashStore::insert_tombstones(&tx, entries, now).unwrap();
        }
        tx.commit().unwrap();
        id
    }

    #[test]
    fn a_unit_round_trips_with_its_primary_and_dependents_in_ordinal_order() {
        let mut store = TrashStore::open_in_memory().unwrap();
        let entries = vec![entry("gstack", 1_100_000_000), entry("ship", 40), entry("review", 60)];
        let id = record(&mut store, Retention::Trashed, &entries, 1_000);

        let unit = store.get(id).unwrap().unwrap();
        assert_eq!(unit.retention, Retention::Trashed);
        assert_eq!(unit.removed_at_millis, 1_000);
        assert_eq!(unit.primary.skill_id.name(), "gstack");
        assert_eq!(unit.dependents.iter().map(|e| e.skill_id.name()).collect::<Vec<_>>(), vec!["ship", "review"]);
        assert!(unit.is_tool_uninstall());
        assert_eq!(unit.bytes(), 1_100_000_100);
    }

    /// Every `SkillId` kind has to survive the text key, or a project skill's
    /// trash entry would come back naming the wrong row -- or no row.
    #[test]
    fn every_skill_id_kind_round_trips_through_the_stored_key() {
        let ids = [
            SkillId::Personal { name: "grilling".to_string() },
            SkillId::Project { repo_path: PathBuf::from("/home/me/repo"), name: "deploy".to_string() },
            SkillId::Plugin {
                marketplace: "official".to_string(),
                plugin: "superpowers".to_string(),
                name: "brainstorming".to_string(),
            },
            // A path with the characters a hand-picked delimiter would choke on;
            // JSON escaping is why the encoding does not have to care.
            SkillId::Project {
                repo_path: PathBuf::from("/home/me/weird \"repo\"/a,b\\c"),
                name: "deploy".to_string(),
            },
        ];
        for id in ids {
            assert_eq!(parse_skill_key(&skill_key(&id), 0).unwrap(), id, "key did not round-trip: {id:?}");
        }
    }

    /// The stored encoding is pinned forever, so it is asserted literally rather
    /// than only round-tripped: a round-trip test passes happily while both ends
    /// drift together, which is exactly the change that orphans every row a
    /// previous build wrote. `SkillRef`'s camelCase wire format is free to move
    /// with the panel; this must not follow it.
    #[test]
    fn the_stored_key_encoding_is_pinned() {
        assert_eq!(
            skill_key(&SkillId::Personal { name: "grilling".to_string() }),
            r#"{"kind":"Personal","name":"grilling"}"#
        );
        assert_eq!(
            skill_key(&SkillId::Project { repo_path: PathBuf::from("/home/me/repo"), name: "deploy".to_string() }),
            r#"{"kind":"Project","repo_path":"/home/me/repo","name":"deploy"}"#
        );
        assert_eq!(
            skill_key(&SkillId::Plugin {
                marketplace: "official".to_string(),
                plugin: "superpowers".to_string(),
                name: "brainstorming".to_string(),
            }),
            r#"{"kind":"Plugin","marketplace":"official","plugin":"superpowers","name":"brainstorming"}"#
        );
    }

    /// Corruption of authoritative state must surface. A skipped entry would
    /// leave its bytes on disk while the cascade deleted the only row naming
    /// them.
    #[test]
    fn an_unreadable_entry_raises_rather_than_vanishing_from_its_unit() {
        let mut store = TrashStore::open_in_memory().unwrap();
        let id = record(&mut store, Retention::Trashed, &[entry("gstack", 1), entry("ship", 2)], 1_000);
        store
            .conn
            .execute("UPDATE trash_entry SET skill_id = 'not json' WHERE ordinal = 1", [])
            .unwrap();

        assert!(store.get(id).is_err(), "a corrupt dependent must not silently drop out of the unit");
        assert!(store.list().is_err());
    }

    #[test]
    fn an_unknown_retention_raises_rather_than_reading_as_absent() {
        let mut store = TrashStore::open_in_memory().unwrap();
        let id = record(&mut store, Retention::Trashed, &[entry("gstack", 1)], 1_000);
        store.conn.execute("UPDATE trash_unit SET retention = 'quarantined'", []).unwrap();

        assert!(store.get(id).is_err(), "'already gone' would invite a caller to stop looking for the bytes");
    }

    #[test]
    fn get_returns_none_for_an_unknown_unit() {
        let store = TrashStore::open_in_memory().unwrap();
        assert!(store.get(TrashUnitId(99)).unwrap().is_none());
    }

    #[test]
    fn list_returns_units_newest_first() {
        let mut store = TrashStore::open_in_memory().unwrap();
        record(&mut store, Retention::Trashed, &[entry("old", 1)], 1_000);
        record(&mut store, Retention::Trashed, &[entry("new", 1)], 5_000);

        let names: Vec<String> = store.list().unwrap().iter().map(|u| u.primary.skill_id.name().to_string()).collect();
        assert_eq!(names, vec!["new", "old"]);
    }

    #[test]
    fn trashing_writes_a_tombstone_and_disabling_does_not() {
        let mut store = TrashStore::open_in_memory().unwrap();
        record(&mut store, Retention::Trashed, &[entry("deleted", 1)], 1_000);
        record(&mut store, Retention::Disabled, &[entry("switched-off", 1)], 2_000);

        let names: Vec<String> = store.tombstones().unwrap().iter().map(|t| t.declared_name.clone()).collect();
        assert_eq!(names, vec!["deleted"], "a disabled skill is not removed, so it is not tombstoned");
    }

    /// The asymmetry ADR 0029 turns on: the bytes are the undo, the tombstone is
    /// the history, and reclaiming the first must not touch the second.
    #[test]
    fn forgetting_a_purged_unit_keeps_its_tombstones() {
        let mut store = TrashStore::open_in_memory().unwrap();
        let id = record(&mut store, Retention::Trashed, &[entry("gstack", 1), entry("ship", 2)], 1_000);
        let unit = store.get(id).unwrap().unwrap();

        store.forget_purged(&unit).unwrap();

        assert!(store.get(id).unwrap().is_none(), "the unit and its undo are gone");
        assert_eq!(store.tombstones().unwrap().len(), 2, "the history survives the purge");
    }

    #[test]
    fn forgetting_a_restored_unit_clears_its_tombstones() {
        let mut store = TrashStore::open_in_memory().unwrap();
        let id = record(&mut store, Retention::Trashed, &[entry("gstack", 1), entry("ship", 2)], 1_000);
        let unit = store.get(id).unwrap().unwrap();

        store.forget_restored(&unit).unwrap();

        assert!(store.get(id).unwrap().is_none());
        assert!(store.tombstones().unwrap().is_empty(), "the skills are listed again");
    }

    #[test]
    fn forgetting_a_unit_cascades_to_its_entries() {
        let mut store = TrashStore::open_in_memory().unwrap();
        let id = record(&mut store, Retention::Trashed, &[entry("gstack", 1), entry("ship", 2)], 1_000);
        let unit = store.get(id).unwrap().unwrap();

        store.forget_purged(&unit).unwrap();

        let orphans: i64 =
            store.conn.query_row("SELECT COUNT(*) FROM trash_entry", [], |r| r.get(0)).unwrap();
        assert_eq!(orphans, 0, "ON DELETE CASCADE needs PRAGMA foreign_keys = ON to fire");
    }

    /// DESIGN #6: reinstalling by hand, past skillmon entirely, restores
    /// continuity on the next scan.
    #[test]
    fn rediscovering_a_tombstoned_skill_clears_its_tombstone() {
        let mut store = TrashStore::open_in_memory().unwrap();
        record(&mut store, Retention::Trashed, &[entry("gstack", 1), entry("ship", 2)], 1_000);

        let cleared = store
            .reconcile_tombstones(&[SkillId::Personal { name: "ship".to_string() }])
            .unwrap();

        assert_eq!(cleared, 1);
        let left: Vec<String> = store.tombstones().unwrap().iter().map(|t| t.declared_name.clone()).collect();
        assert_eq!(left, vec!["gstack"], "only the rediscovered skill is un-tombstoned");
    }

    #[test]
    fn reconciling_against_skills_that_were_never_removed_clears_nothing() {
        let mut store = TrashStore::open_in_memory().unwrap();
        record(&mut store, Retention::Trashed, &[entry("gstack", 1)], 1_000);

        let cleared = store.reconcile_tombstones(&[SkillId::Personal { name: "unrelated".to_string() }]).unwrap();

        assert_eq!(cleared, 0);
        assert_eq!(store.tombstones().unwrap().len(), 1);
    }

    /// An empty scan is not evidence that every skill was removed. Reconciling
    /// against nothing must clear nothing, never "everything is gone".
    #[test]
    fn reconciling_against_an_empty_scan_clears_nothing() {
        let mut store = TrashStore::open_in_memory().unwrap();
        record(&mut store, Retention::Trashed, &[entry("gstack", 1)], 1_000);

        assert_eq!(store.reconcile_tombstones(&[]).unwrap(), 0);
        assert_eq!(store.tombstones().unwrap().len(), 1);
    }

    /// The ledger is the only record of where a trashed entry came from, so it
    /// must survive a reopen intact -- there is nothing to re-derive it from.
    #[test]
    fn the_ledger_survives_a_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("removal.sqlite");
        let id = {
            let mut store = TrashStore::open(&path).unwrap();
            record(&mut store, Retention::Trashed, &[entry("gstack", 1_100_000_000)], 1_000)
        };

        let store = TrashStore::open(&path).unwrap();
        let unit = store.get(id).unwrap().unwrap();
        assert_eq!(unit.primary.origin_path, PathBuf::from("/home/me/.claude/skills/gstack"));
        assert_eq!(unit.bytes(), 1_100_000_000);
        assert_eq!(store.tombstones().unwrap().len(), 1);
    }

    /// A rolled-back id can recur, but a committed one must never be handed out
    /// again -- it names a directory on disk.
    #[test]
    fn a_purged_units_id_is_never_reissued() {
        let mut store = TrashStore::open_in_memory().unwrap();
        let first = record(&mut store, Retention::Trashed, &[entry("a", 1)], 1_000);
        let unit = store.get(first).unwrap().unwrap();
        store.forget_purged(&unit).unwrap();

        let second = record(&mut store, Retention::Trashed, &[entry("b", 1)], 2_000);
        assert_ne!(first, second, "AUTOINCREMENT, not a bare rowid");
    }
}
