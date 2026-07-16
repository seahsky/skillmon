//! Reversible entry removal: move it out, put it back, or reclaim it (ADR 0029).
//!
//! The seam split with issue #31 is: **that** decides *what* to remove -- entry
//! or source, which dependents cascade, whether a managing tool can make a
//! source removal stick -- and this moves it out reversibly and records it. So
//! nothing here reads a `SKILL.md`, resolves a symlink, or knows what gstack is.
//!
//! Harness-neutral, like `footprint/` and for the same reason (ADR 0002): the
//! caller passes the storage root, and no Claude Code path is named here.

pub mod fs_ops;
pub mod store;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Result as SqliteResult, Transaction};

use crate::domain::removal::{EntryToRemove, Retention, TrashUnit, TrashUnitId, TrashedEntry};
use crate::domain::report::PurgeSummary;
use store::TrashStore;

#[derive(Debug, thiserror::Error)]
pub enum RemovalError {
    #[error("trash unit {0} is not in the ledger")]
    UnknownUnit(i64),
    /// ADR 0027's recorded hazard, reached: a managing tool rebuilt the path
    /// while the entry sat in the trash. Failing loudly is the point -- the
    /// alternative is clobbering what the tool just wrote.
    #[error("{name} has been rebuilt at {path}; restoring the trashed copy would overwrite it")]
    OriginOccupied { name: String, path: PathBuf },
    #[error("the trashed copy of {name} is no longer at {path}")]
    StoredEntryMissing { name: String, path: PathBuf },
    #[error("{name} is disabled, and a disabled entry is retained indefinitely; trash it before purging it")]
    NotPurgeable { name: String },
    #[error("{path} has no final path component to store it under")]
    UnnamedEntry { path: PathBuf },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Ledger(#[from] rusqlite::Error),
}

/// Moves a removal's entries out of the scan root as one reversible unit, and
/// records it.
///
/// `primary` is the entry the user acted on; `dependents` are the skills that
/// resolved into it and therefore cascade (ADR 0027) -- pass none for an
/// ordinary skill removal. `retention` is the whole difference between disabling
/// and deleting.
///
/// Every entry lands under the **primary's** storage root, which is what makes
/// the cross-device fallback more than theory: a cascade spanning a manager root
/// under `~` and a dependent in a repo on another volume crosses filesystems by
/// construction.
///
/// Ordering is load-bearing. The unit id is reserved first because it names the
/// directory the moves write into; the transaction commits last, after every
/// move has landed. A crash between them leaves staged bytes with no unit row --
/// an inert directory -- never a unit row with no bytes, which would offer an
/// undo that restores nothing (ADR 0029).
///
/// The write side of the ledger, and the half issue #28 ships without a caller:
/// the panel can list, restore, and purge a unit, but the mutations that
/// *create* one land with issue #31. `allow(dead_code)` keeps that seam
/// compiling and tested without masking a real regression, as on
/// `DiscoveredSkill::skill_md_path`.
#[allow(dead_code)]
pub fn remove(
    store: &mut TrashStore,
    storage_root: &Path,
    now_millis: i64,
    retention: Retention,
    primary: EntryToRemove,
    dependents: Vec<EntryToRemove>,
) -> Result<TrashUnitId, RemovalError> {
    let to_remove: Vec<EntryToRemove> = std::iter::once(primary).chain(dependents).collect();

    let tx = store.transaction()?;
    let id = TrashStore::insert_unit(&tx, retention, now_millis)?;

    // A directory can already sit here only if an earlier attempt at this id
    // crashed and its transaction rolled back, so no live unit references it and
    // clearing it is safe. (`AUTOINCREMENT` keeps a *committed* id from ever
    // recurring; a rolled-back one can.)
    let storage_dir = storage_root.join(id.0.to_string());
    fs_ops::delete_if_exists(&storage_dir)?;

    let staged = stage_all(&to_remove, &storage_dir)?;

    if let Err(e) = record(tx, id, &staged, retention, now_millis) {
        // The ledger refused the unit, so the entries must not stay staged --
        // they would be bytes nothing knows how to give back.
        unstage(&staged);
        return Err(e.into());
    }
    Ok(id)
}

/// The transaction's final act, factored out so every failure between staging
/// and commit funnels through one rollback in `remove`.
fn record(
    tx: Transaction<'_>,
    id: TrashUnitId,
    staged: &[TrashedEntry],
    retention: Retention,
    now_millis: i64,
) -> SqliteResult<()> {
    TrashStore::insert_entries(&tx, id, staged)?;
    if retention == Retention::Trashed {
        TrashStore::insert_tombstones(&tx, staged, now_millis)?;
    }
    tx.commit()
}

fn stage_all(entries: &[EntryToRemove], storage_dir: &Path) -> Result<Vec<TrashedEntry>, RemovalError> {
    let mut staged: Vec<TrashedEntry> = Vec::new();
    for (ordinal, entry) in entries.iter().enumerate() {
        match stage_one(entry, ordinal, storage_dir) {
            Ok(t) => staged.push(t),
            Err(e) => {
                unstage(&staged);
                return Err(e);
            }
        }
    }
    Ok(staged)
}

fn stage_one(entry: &EntryToRemove, ordinal: usize, storage_dir: &Path) -> Result<TrashedEntry, RemovalError> {
    // Size it before it moves; afterwards there is nothing at `entry_path` to
    // walk.
    let bytes = fs_ops::entry_size(&entry.entry_path)?;
    let name = entry
        .entry_path
        .file_name()
        .ok_or_else(|| RemovalError::UnnamedEntry { path: entry.entry_path.clone() })?;

    // The ordinal prefix is not decoration: two entries in one unit can share a
    // directory name (the same project skill cascaded from two repos), and the
    // ordinal is the only thing that is unique by construction. The name is kept
    // alongside it so a human who opens the trash can read it.
    let stored_path = storage_dir.join(format!("{ordinal}-{}", name.to_string_lossy()));
    fs_ops::move_entry(&entry.entry_path, &stored_path)?;

    Ok(TrashedEntry {
        skill_id: entry.skill_id.clone(),
        declared_name: entry.declared_name.clone(),
        origin_path: entry.entry_path.clone(),
        stored_path,
        bytes,
    })
}

/// Undoes a partial staging: puts entries back where they came from.
fn unstage(staged: &[TrashedEntry]) {
    move_back(staged.iter().rev().map(|e| (e.stored_path.as_path(), e.origin_path.as_path())));
}

/// Undoes a partial restore: puts entries back into the trash.
fn restage(restored: &[&TrashedEntry]) {
    move_back(restored.iter().rev().map(|e| (e.origin_path.as_path(), e.stored_path.as_path())));
}

/// Reverses a run of moves, in reverse order.
///
/// The two directions get named wrappers above rather than being spelled out at
/// each of the three call sites: the only difference between them is which of
/// two same-typed paths comes first, so a transposition would compile, pass a
/// casual read, and move every entry the wrong way at the exact moment the code
/// is already failing.
///
/// Best-effort by necessity: this runs while an error is on its way up, so there
/// is nothing useful to return a second error to. A failure is logged rather
/// than swallowed -- the same channel `lib.rs` uses for a toast that could not
/// be shown, and for the same reason: the operation the user asked for has
/// already reported its own outcome.
fn move_back<'a>(moves: impl Iterator<Item = (&'a Path, &'a Path)>) {
    for (from, to) in moves {
        if let Err(e) = fs_ops::move_entry(from, to) {
            eprintln!("[skillmon] rollback could not move {} back to {}: {e}", from.display(), to.display());
        }
    }
}

/// Restores a whole unit to the paths it came from -- 47 entries or one.
///
/// Atomic by precheck, then rollback (ADR 0029), which is not the same as a
/// transaction and is not claimed to be: the filesystem offers nothing across 47
/// paths. The precheck does the real work by refusing before anything moves; the
/// rollback covers the races it cannot close.
pub fn restore(store: &mut TrashStore, id: TrashUnitId) -> Result<(), RemovalError> {
    let unit = store.get(id)?.ok_or(RemovalError::UnknownUnit(id.0))?;

    for entry in unit.entries() {
        if fs::symlink_metadata(&entry.stored_path).is_err() {
            return Err(RemovalError::StoredEntryMissing {
                name: entry.declared_name.clone(),
                path: entry.stored_path.clone(),
            });
        }
        // `symlink_metadata`, never `Path::exists()`: `exists()` follows links
        // and so reports a *dangling* symlink as absent. That is precisely the
        // shape a managing tool leaves behind when it rebuilds a shim whose
        // target is not there yet, and `rename` would silently replace it.
        if fs::symlink_metadata(&entry.origin_path).is_ok() {
            return Err(RemovalError::OriginOccupied {
                name: entry.declared_name.clone(),
                path: entry.origin_path.clone(),
            });
        }
    }

    let mut moved: Vec<&TrashedEntry> = Vec::new();
    for entry in unit.entries() {
        match fs_ops::move_entry(&entry.stored_path, &entry.origin_path) {
            Ok(()) => moved.push(entry),
            Err(e) => {
                restage(&moved);
                return Err(e.into());
            }
        }
    }

    if let Some(dir) = unit.storage_dir() {
        fs_ops::remove_dir_if_empty(dir);
    }
    store.forget_restored(&unit)?;
    Ok(())
}

/// Reclaims a trashed unit's bytes, on the user's explicit say-so, and returns
/// what it freed. The tombstones stay (ADR 0029).
pub fn purge(store: &mut TrashStore, id: TrashUnitId) -> Result<u64, RemovalError> {
    let unit = store.get(id)?.ok_or(RemovalError::UnknownUnit(id.0))?;
    purge_unit(store, &unit)
}

fn purge_unit(store: &mut TrashStore, unit: &TrashUnit) -> Result<u64, RemovalError> {
    // The entire content of the retention intent (ADR 0027): a disabled entry is
    // a row you can re-enable, not garbage awaiting collection.
    if unit.retention == Retention::Disabled {
        return Err(RemovalError::NotPurgeable { name: unit.primary.declared_name.clone() });
    }
    let bytes = unit.bytes();
    for entry in unit.entries() {
        // Forgiving of an already-missing entry: a user who deleted the staged
        // copy by hand should still be able to clear the row that points at it.
        fs_ops::delete_if_exists(&entry.stored_path)?;
    }
    if let Some(dir) = unit.storage_dir() {
        fs_ops::remove_dir_if_empty(dir);
    }
    store.forget_purged(unit)?;
    Ok(bytes)
}

/// Reclaims every **trashed** unit, and reports what was actually freed.
///
/// Skips `Disabled` units rather than reading "empty the trash" as "remove
/// everything staged". Nothing here runs on a timer or a retention window: a
/// trash unit is reclaimed when the user says so and never otherwise (ADR 0029).
///
/// One unit's failure does not abort the rest, and does not discard the total.
/// Bailing on the first error would throw away the count for units already
/// purged -- whose rows are already gone, so the figure could never be recovered
/// -- and would strand the remaining units behind one unremovable tree. Failures
/// are reported as a count and logged individually, never swallowed: `failed > 0`
/// is what stops the panel claiming a clean sweep.
pub fn empty_trash(store: &mut TrashStore) -> Result<PurgeSummary, RemovalError> {
    let units = store.list()?;
    let mut summary = PurgeSummary::default();
    for unit in units.iter().filter(|u| u.retention == Retention::Trashed) {
        match purge_unit(store, unit) {
            Ok(bytes) => {
                summary.bytes += bytes;
                summary.units += 1;
            }
            Err(e) => {
                summary.failed += 1;
                eprintln!("[skillmon] could not purge trash unit {}: {e}", unit.id.0);
            }
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::skill::SkillId;
    use std::fs::File;
    use std::io::Write;
    use tempfile::{tempdir, TempDir};

    struct Fixture {
        tmp: TempDir,
        store: TrashStore,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture { tmp: tempdir().unwrap(), store: TrashStore::open_in_memory().unwrap() }
        }

        fn skills_dir(&self) -> PathBuf {
            self.tmp.path().join("skills")
        }

        fn storage_root(&self) -> PathBuf {
            self.tmp.path().join("skillmon/removed")
        }

        /// A real skill directory under the scan root.
        fn install(&self, name: &str, body: &str) -> EntryToRemove {
            let dir = self.skills_dir().join(name);
            write_file(&dir.join("SKILL.md"), body);
            self.entry(name)
        }

        fn entry(&self, name: &str) -> EntryToRemove {
            EntryToRemove {
                skill_id: SkillId::Personal { name: name.to_string() },
                declared_name: name.to_string(),
                entry_path: self.skills_dir().join(name),
            }
        }

        /// Threads the storage root through, so a caller never has to borrow the
        /// fixture immutably and mutably in one call.
        fn remove(
            &mut self,
            now_millis: i64,
            retention: Retention,
            primary: EntryToRemove,
            dependents: Vec<EntryToRemove>,
        ) -> Result<TrashUnitId, RemovalError> {
            let root = self.tmp.path().join("skillmon/removed");
            super::remove(&mut self.store, &root, now_millis, retention, primary, dependents)
        }
    }

    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        File::create(path).unwrap().write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn removing_a_skill_moves_its_entry_out_of_the_scan_root_and_records_it() {
        let mut f = Fixture::new();
        let entry = f.install("vercel-react", "0123456789");
        let origin = entry.entry_path.clone();

        let id = f.remove(1_000, Retention::Trashed, entry, vec![]).unwrap();

        assert!(!origin.exists(), "un-discovered: the entry left the depth-1 scan root");
        let unit = f.store.get(id).unwrap().unwrap();
        assert_eq!(unit.retention, Retention::Trashed);
        assert_eq!(unit.removed_at_millis, 1_000);
        assert_eq!(unit.primary.origin_path, origin);
        assert_eq!(unit.bytes(), 10);
        assert!(unit.primary.stored_path.join("SKILL.md").exists(), "the bytes are staged, not deleted");
        assert!(!unit.is_tool_uninstall());
    }

    /// gstack's shape (ADR 0027): removing the row that *is* the checkout
    /// cascades to the 46 shims resolving into it, as one unit with one undo.
    #[test]
    fn a_tool_uninstall_stages_every_dependent_into_one_unit() {
        let mut f = Fixture::new();
        let primary = f.install("gstack", "checkout");
        let dependents: Vec<EntryToRemove> = (0..3)
            .map(|i| f.install(&format!("shim-{i}"), "s"))
            .collect();
        let origins: Vec<PathBuf> = std::iter::once(primary.entry_path.clone())
            .chain(dependents.iter().map(|d| d.entry_path.clone()))
            .collect();

        let id = f.remove(1_000, Retention::Trashed, primary, dependents).unwrap();

        let unit = f.store.get(id).unwrap().unwrap();
        assert!(unit.is_tool_uninstall());
        assert_eq!(unit.entry_count(), 4);
        for origin in &origins {
            assert!(!origin.exists(), "{} should have been staged", origin.display());
        }
        // Ordinals disambiguate, and all four live under one unit directory.
        let dirs: Vec<PathBuf> = unit.entries().map(|e| e.stored_path.parent().unwrap().to_path_buf()).collect();
        assert!(dirs.windows(2).all(|w| w[0] == w[1]), "one unit, one storage directory");
    }

    #[cfg(unix)]
    #[test]
    fn removing_a_managed_skill_takes_the_entry_and_leaves_the_managers_content() {
        let mut f = Fixture::new();
        // The `.agents` shape: the entry is a link, and the target is the only copy.
        let target = f.tmp.path().join("agents/skills/tdd");
        write_file(&target.join("SKILL.md"), "the only copy");
        fs::create_dir_all(f.skills_dir()).unwrap();
        std::os::unix::fs::symlink(&target, f.skills_dir().join("tdd")).unwrap();

        let id = f.remove(1_000, Retention::Trashed, f.entry("tdd"), vec![]).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "the only copy",
            "skillmon removes the entry, never through it"
        );
        let unit = f.store.get(id).unwrap().unwrap();
        assert!(fs::symlink_metadata(&unit.primary.stored_path).unwrap().is_symlink());
    }

    #[test]
    fn removing_rolls_back_every_earlier_move_when_one_entry_fails() {
        let mut f = Fixture::new();
        let good = f.install("shim-a", "s");
        let good_origin = good.entry_path.clone();
        // Never installed, so sizing it fails and the unit cannot be staged.
        let missing = f.entry("was-never-there");

        let err = f.remove(1_000, Retention::Trashed, good, vec![missing]).unwrap_err();

        assert!(matches!(err, RemovalError::Io(_)));
        assert!(good_origin.join("SKILL.md").exists(), "the earlier entry was put back");
        assert!(f.store.list().unwrap().is_empty(), "the transaction rolled back with it");
        assert!(f.store.tombstones().unwrap().is_empty());
    }

    #[test]
    fn removing_clears_a_storage_directory_left_by_a_crashed_attempt() {
        let mut f = Fixture::new();
        // The id a fresh ledger hands out first; a prior crashed attempt at it
        // rolled back its row but left its bytes.
        write_file(&f.storage_root().join("1/0-ghost/SKILL.md"), "stale");
        let entry = f.install("vercel-react", "fresh");

        let id = f.remove(1_000, Retention::Trashed, entry, vec![]).unwrap();

        assert_eq!(id, TrashUnitId(1));
        assert!(!f.storage_root().join("1/0-ghost").exists(), "the stale tree is cleared, not inherited");
        let unit = f.store.get(id).unwrap().unwrap();
        assert_eq!(fs::read_to_string(unit.primary.stored_path.join("SKILL.md")).unwrap(), "fresh");
    }

    #[test]
    fn restoring_puts_every_entry_back_and_clears_its_tombstones() {
        let mut f = Fixture::new();
        let primary = f.install("gstack", "checkout");
        let dependents = vec![f.install("ship", "s"), f.install("review", "r")];
        let origins: Vec<PathBuf> = std::iter::once(primary.entry_path.clone())
            .chain(dependents.iter().map(|d| d.entry_path.clone()))
            .collect();
        let id = f.remove(1_000, Retention::Trashed, primary, dependents).unwrap();
        let storage_dir = f.store.get(id).unwrap().unwrap().storage_dir().unwrap().to_path_buf();

        restore(&mut f.store, id).unwrap();

        assert_eq!(fs::read_to_string(origins[0].join("SKILL.md")).unwrap(), "checkout");
        assert_eq!(fs::read_to_string(origins[1].join("SKILL.md")).unwrap(), "s");
        assert_eq!(fs::read_to_string(origins[2].join("SKILL.md")).unwrap(), "r");
        assert!(f.store.get(id).unwrap().is_none(), "the unit is spent");
        assert!(f.store.tombstones().unwrap().is_empty(), "the skills are listed again");
        assert!(!storage_dir.exists(), "the storage directory is cleaned up behind it");
    }

    /// ADR 0027's hazard: gstack rebuilt the shim while it sat in the trash.
    #[test]
    fn restoring_refuses_when_a_managing_tool_rebuilt_the_origin() {
        let mut f = Fixture::new();
        let entry = f.install("ship", "trashed copy");
        let origin = entry.entry_path.clone();
        let id = f.remove(1_000, Retention::Trashed, entry, vec![]).unwrap();
        write_file(&origin.join("SKILL.md"), "what gstack rebuilt");

        let err = restore(&mut f.store, id).unwrap_err();

        assert!(matches!(err, RemovalError::OriginOccupied { .. }), "got {err:?}");
        assert_eq!(
            fs::read_to_string(origin.join("SKILL.md")).unwrap(),
            "what gstack rebuilt",
            "the tool's rebuild is never clobbered"
        );
        assert!(f.store.get(id).unwrap().is_some(), "the unit survives a refused restore");
    }

    /// The exact reason the precheck cannot use `Path::exists()`: a shim
    /// rebuilt ahead of its target is a *dangling* link, which `exists()`
    /// reports as absent and `rename` would silently replace.
    #[cfg(unix)]
    #[test]
    fn restoring_refuses_when_the_origin_holds_a_dangling_symlink() {
        let mut f = Fixture::new();
        let entry = f.install("ship", "trashed copy");
        let origin = entry.entry_path.clone();
        let id = f.remove(1_000, Retention::Trashed, entry, vec![]).unwrap();
        std::os::unix::fs::symlink(f.tmp.path().join("not-cloned-yet"), &origin).unwrap();

        assert!(!origin.exists(), "the fixture really is the case exists() gets wrong");
        let err = restore(&mut f.store, id).unwrap_err();

        assert!(matches!(err, RemovalError::OriginOccupied { .. }), "got {err:?}");
        assert!(fs::symlink_metadata(&origin).unwrap().is_symlink(), "the tool's link is still there");
    }

    /// Precheck before mutate: one unrestorable entry must not leave the other
    /// 46 half-restored.
    #[test]
    fn restoring_moves_nothing_when_any_entry_fails_its_precheck() {
        let mut f = Fixture::new();
        let primary = f.install("gstack", "checkout");
        let dependent = f.install("ship", "s");
        let primary_origin = primary.entry_path.clone();
        let id = f.remove(1_000, Retention::Trashed, primary, vec![dependent]).unwrap();
        let unit = f.store.get(id).unwrap().unwrap();
        // The *second* entry's staged copy vanishes, so the failure is only
        // discovered after the first would already have moved.
        fs::remove_dir_all(&unit.dependents[0].stored_path).unwrap();

        let err = restore(&mut f.store, id).unwrap_err();

        assert!(matches!(err, RemovalError::StoredEntryMissing { .. }), "got {err:?}");
        assert!(!primary_origin.exists(), "the primary never moved");
        assert!(unit.primary.stored_path.exists(), "and is still staged");
        assert!(f.store.get(id).unwrap().is_some());
    }

    #[test]
    fn restoring_an_unknown_unit_is_an_error() {
        let mut f = Fixture::new();
        assert!(matches!(restore(&mut f.store, TrashUnitId(42)), Err(RemovalError::UnknownUnit(42))));
    }

    #[test]
    fn purging_reclaims_the_bytes_and_keeps_the_tombstone() {
        let mut f = Fixture::new();
        let entry = f.install("vercel-react", "0123456789");
        let id = f.remove(1_000, Retention::Trashed, entry, vec![]).unwrap();
        let storage_dir = f.store.get(id).unwrap().unwrap().storage_dir().unwrap().to_path_buf();

        let freed = purge(&mut f.store, id).unwrap();

        assert_eq!(freed, 10);
        assert!(!storage_dir.exists(), "the bytes are gone");
        assert!(f.store.get(id).unwrap().is_none(), "and so is the undo");
        let names: Vec<String> = f.store.tombstones().unwrap().iter().map(|t| t.declared_name.clone()).collect();
        assert_eq!(names, vec!["vercel-react"], "the history outlives the bytes");
    }

    #[cfg(unix)]
    #[test]
    fn purging_a_managed_entry_unlinks_the_shim_and_spares_the_managers_content() {
        let mut f = Fixture::new();
        let target = f.tmp.path().join("agents/skills/tdd");
        write_file(&target.join("SKILL.md"), "the only copy");
        fs::create_dir_all(f.skills_dir()).unwrap();
        std::os::unix::fs::symlink(&target, f.skills_dir().join("tdd")).unwrap();
        let id = f.remove(1_000, Retention::Trashed, f.entry("tdd"), vec![]).unwrap();

        purge(&mut f.store, id).unwrap();

        assert_eq!(fs::read_to_string(target.join("SKILL.md")).unwrap(), "the only copy");
    }

    #[test]
    fn purging_a_disabled_unit_is_refused() {
        let mut f = Fixture::new();
        let entry = f.install("switched-off", "s");
        let id = f.remove(1_000, Retention::Disabled, entry, vec![]).unwrap();

        let err = purge(&mut f.store, id).unwrap_err();

        assert!(matches!(err, RemovalError::NotPurgeable { .. }), "got {err:?}");
        assert!(f.store.get(id).unwrap().is_some(), "a disabled entry is retained indefinitely");
    }

    #[test]
    fn emptying_the_trash_reclaims_trashed_units_and_leaves_disabled_ones_alone() {
        let mut f = Fixture::new();
        let a = f.install("deleted-a", "0123456789"); // 10
        let b = f.install("deleted-b", "01234"); // 5
        let kept = f.install("switched-off", "s");
        let kept_id = f.remove(1_000, Retention::Disabled, kept, vec![]).unwrap();
        f.remove(2_000, Retention::Trashed, a, vec![]).unwrap();
        f.remove(3_000, Retention::Trashed, b, vec![]).unwrap();

        let summary = empty_trash(&mut f.store).unwrap();

        assert_eq!(summary.units, 2);
        assert_eq!(summary.bytes, 15);
        let left = f.store.list().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, kept_id, "the disabled unit is still restorable");
        assert!(left[0].primary.stored_path.exists());
    }

    #[test]
    fn emptying_an_empty_trash_reclaims_nothing() {
        let mut f = Fixture::new();
        let summary = empty_trash(&mut f.store).unwrap();
        assert_eq!(summary.units, 0);
        assert_eq!(summary.bytes, 0);
        assert_eq!(summary.failed, 0);
    }

    /// One unremovable tree must not abort the sweep or discard the figure. The
    /// units already purged have had their rows deleted, so a bail-out would
    /// throw away a number nothing could ever recompute.
    #[cfg(unix)]
    #[test]
    fn emptying_the_trash_keeps_going_and_still_reports_what_it_freed_when_one_unit_fails() {
        use std::os::unix::fs::PermissionsExt;

        let mut f = Fixture::new();
        let doomed = f.install("locked", "0123456789"); // 10
        let fine = f.install("removable", "01234"); // 5
        let locked_id = f.remove(1_000, Retention::Trashed, doomed, vec![]).unwrap();
        f.remove(2_000, Retention::Trashed, fine, vec![]).unwrap();

        // Deleting an entry needs write permission on the directory holding it.
        let locked_dir = f.store.get(locked_id).unwrap().unwrap().storage_dir().unwrap().to_path_buf();
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::File::create(locked_dir.join(".probe")).is_ok() {
            eprintln!("skipping: this process ignores directory permissions (running as root?)");
            return;
        }

        let summary = empty_trash(&mut f.store).unwrap();

        assert_eq!(summary.units, 1, "the removable unit was still purged");
        assert_eq!(summary.bytes, 5, "and its bytes are still reported");
        assert_eq!(summary.failed, 1, "the failure is surfaced, not swallowed");
        let left = f.store.list().unwrap();
        assert_eq!(left.len(), 1, "the failed unit keeps its row, so its bytes stay nameable");
        assert_eq!(left[0].id, locked_id);

        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// CLAUDE.md's verification bar for issue #28: exercise the flow against
    /// this machine's real `~/.claude` -- real entry shapes, real content, real
    /// byte figures -- rather than only over synthetic tempdirs, and assert that
    /// "uninstall -> restore" round-trips.
    ///
    /// It replicates each real entry into a temp scan root instead of pointing
    /// the removal at the live install. That is not timidity: removal is
    /// destructive and `~/.claude/skills` is the user's only copy, so a test
    /// that mutated it would be the exact overreach this module exists to
    /// prevent. What matters is preserved -- a managed entry is reproduced as a
    /// link to its **real** target, so the central claim (removing an entry
    /// never touches the manager's content) is asserted against real content on
    /// the real filesystem.
    ///
    /// Run by hand:
    /// `cargo test --manifest-path src-tauri/Cargo.toml
    /// removal::tests::real_claude_home_removal_round_trip -- --ignored --exact --nocapture`
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn real_claude_home_removal_round_trip() {
        use crate::adapters::claude_code::paths::default_claude_home;
        use crate::adapters::claude_code::ClaudeCodeAdapter;

        /// Reproduces an entry's *shape*, so a fixture entry is managed exactly
        /// where the real one is, and points at the same real target.
        fn replicate(from: &Path, to: &Path) {
            let meta = fs::symlink_metadata(from).unwrap();
            if meta.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(from).unwrap(), to).unwrap();
            } else if meta.is_dir() {
                fs::create_dir_all(to).unwrap();
                for child in fs::read_dir(from).unwrap() {
                    let child = child.unwrap();
                    replicate(&child.path(), &to.join(child.file_name()));
                }
            } else {
                fs::copy(from, to).unwrap();
            }
        }

        fn read_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
            let mut out = Vec::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(path) = stack.pop() {
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_symlink() {
                    out.push((path.strip_prefix(root).unwrap().to_path_buf(), fs::read_link(&path).unwrap().into_os_string().into_encoded_bytes()));
                } else if meta.is_dir() {
                    stack.extend(fs::read_dir(&path).unwrap().map(|c| c.unwrap().path()));
                } else {
                    out.push((path.strip_prefix(root).unwrap().to_path_buf(), fs::read(&path).unwrap()));
                }
            }
            out.sort();
            out
        }

        let discovery = ClaudeCodeAdapter::for_discovery_only(default_claude_home()).discover_skills();
        assert!(!discovery.skills.is_empty(), "no skills discovered -- is this machine's ~/.claude populated?");

        let f = Fixture::new();
        let mut store = f.store;
        let scan_root = f.tmp.path().join("skills");
        let storage_root = f.tmp.path().join("skillmon/removed");
        fs::create_dir_all(&scan_root).unwrap();

        // Every real skill, reproduced as a MANAGED entry linking at the real
        // content -- the gstack/`.agents` shape, cascaded as one tool uninstall.
        let mut entries: Vec<EntryToRemove> = Vec::new();
        let mut real_targets: Vec<PathBuf> = Vec::new();
        for skill in &discovery.skills {
            let name = skill.directory_name();
            let entry_path = scan_root.join(name);
            if entry_path.exists() {
                continue; // two repos can ship the same project-skill name
            }
            std::os::unix::fs::symlink(&skill.dir_path, &entry_path).unwrap();
            entries.push(EntryToRemove {
                skill_id: skill.id.clone(),
                declared_name: skill.frontmatter.declared_name.clone(),
                entry_path,
            });
            real_targets.push(skill.dir_path.clone());
        }
        let before: Vec<Vec<(PathBuf, Vec<u8>)>> = real_targets.iter().map(|t| read_tree(t)).collect();

        let primary = entries.remove(0);
        let id = remove(&mut store, &storage_root, 1_000, Retention::Trashed, primary, entries).unwrap();
        let unit = store.get(id).unwrap().unwrap();
        eprintln!(
            "\n=== managed entries: {} staged as one tool uninstall, {} bytes of links ===",
            unit.entry_count(),
            unit.bytes()
        );
        purge(&mut store, id).unwrap();

        for (target, expected) in real_targets.iter().zip(&before) {
            assert_eq!(
                &read_tree(target),
                expected,
                "purging the entry damaged the manager's real content at {}",
                target.display()
            );
        }
        eprintln!("=== {} real manager roots byte-identical after purge ===", real_targets.len());

        // Those 17 purged units left 17 tombstones behind, which is the point of
        // them: the bytes were the undo, the history survives it. So the second
        // phase counts against that baseline rather than against zero.
        let baseline = store.tombstones().unwrap().len();
        assert_eq!(baseline, real_targets.len(), "every purged skill kept its history");

        // One real skill reproduced as an UNMANAGED entry -- real files, real
        // bytes -- must survive uninstall -> restore byte-for-byte (CLAUDE.md).
        // Its own identity, since it is a distinct entry from the linked one
        // above and must not collide with that tombstone.
        let sample = &discovery.skills[0];
        let name = format!("unmanaged-{}", sample.directory_name());
        let entry_path = scan_root.join(&name);
        replicate(&sample.dir_path, &entry_path);
        let expected = read_tree(&entry_path);
        let real_bytes = fs_ops::entry_size(&entry_path).unwrap();
        assert!(real_bytes > 0, "the sample skill has no bytes to reclaim");

        let entry = EntryToRemove {
            skill_id: SkillId::Personal { name: name.clone() },
            declared_name: sample.frontmatter.declared_name.clone(),
            entry_path: entry_path.clone(),
        };
        let id = remove(&mut store, &storage_root, 2_000, Retention::Trashed, entry.clone(), vec![]).unwrap();
        assert!(!entry_path.exists(), "un-discovered while trashed");
        assert_eq!(store.tombstones().unwrap().len(), baseline + 1, "and tombstoned");

        restore(&mut store, id).unwrap();
        assert_eq!(read_tree(&entry_path), expected, "restore was not byte-identical");
        assert_eq!(store.tombstones().unwrap().len(), baseline, "restoring un-tombstones it");

        // And again, through to the purge that reclaims it for real.
        let id = remove(&mut store, &storage_root, 3_000, Retention::Trashed, entry, vec![]).unwrap();
        let freed = purge(&mut store, id).unwrap();
        assert_eq!(freed, real_bytes);
        assert!(!entry_path.exists());
        assert_eq!(store.tombstones().unwrap().len(), baseline + 1, "the history outlives the bytes");
        assert!(
            sample.dir_path.join("SKILL.md").exists(),
            "the real skill this fixture was copied from must be untouched"
        );
        eprintln!(
            "=== unmanaged round-trip on real content: {} ({} bytes) reclaimed, real install intact ===\n",
            sample.directory_name(),
            freed
        );
    }

    /// A disabled skill is still a row: restoring it is re-enabling it, and it
    /// was never tombstoned.
    #[test]
    fn disabling_then_restoring_round_trips_without_ever_tombstoning() {
        let mut f = Fixture::new();
        let entry = f.install("grilling", "body");
        let origin = entry.entry_path.clone();

        let id = f.remove(1_000, Retention::Disabled, entry, vec![]).unwrap();
        assert!(!origin.exists());
        assert!(f.store.tombstones().unwrap().is_empty());

        restore(&mut f.store, id).unwrap();
        assert_eq!(fs::read_to_string(origin.join("SKILL.md")).unwrap(), "body");
        assert!(f.store.tombstones().unwrap().is_empty());
    }
}
