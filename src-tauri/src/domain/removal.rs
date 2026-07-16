use std::path::PathBuf;

use serde::Serialize;

use super::skill::SkillId;

/// What may reclaim an entry that has been moved out of the scan root (ADR
/// 0027). The *whole* difference between disabling a skill and deleting one:
/// both are the same `rename(2)` to the same place, and this label is the only
/// thing that distinguishes them. Keeping the intent in state rather than in the
/// destination path is what let ADR 0027 collapse ADR 0007's two mechanisms into
/// one code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Retention {
    /// Kept indefinitely, and never purged -- not by `empty_trash`, not by a
    /// retention window. A disabled skill is a row you can re-enable, so its
    /// bytes are not garbage waiting to be collected.
    Disabled,
    /// Eligible for purge, on the user's explicit say-so (ADR 0029). Writes a
    /// tombstone, because this is the removal DESIGN #6 is about.
    Trashed,
}

impl Retention {
    /// The **stored** discriminant, deliberately hand-written rather than shared
    /// with the `Serialize` derive above even though the two currently produce
    /// the same strings. They are not the same contract: the wire encoding is
    /// free to change with the panel, while this one is pinned forever, because
    /// this store is authoritative state and a value it can no longer parse is a
    /// trash unit whose files it can no longer give back (see `removal::store`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Retention::Disabled => "disabled",
            Retention::Trashed => "trashed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "disabled" => Some(Retention::Disabled),
            "trashed" => Some(Retention::Trashed),
            _ => None,
        }
    }
}

/// A trash unit's key. A newtype rather than a bare `i64` because it is also the
/// name of a directory on disk (`<storage_root>/<id>/`), so handing the wrong
/// integer to the wrong function deletes the wrong tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrashUnitId(pub i64);

/// One entry the user asked to remove, as it reaches `removal::remove`. The
/// caller (issue #31) decides *what* to remove -- entry vs. source, which
/// dependents cascade -- and this is the whole of what the trash needs to know
/// to move it out and put it back.
#[derive(Debug, Clone)]
pub struct EntryToRemove {
    pub skill_id: SkillId,
    /// Carried so a tombstone can label a row whose files are gone. Once the
    /// entry is purged there is no `SKILL.md` left to read a name out of, and
    /// the directory name in `skill_id` is not always the name the user knows
    /// the skill by (CONTEXT.md "Declared name").
    pub declared_name: String,
    /// The path *under the scan root* -- the entry itself, never what it
    /// resolves to. skillmon removes the entry, never through it (ADR 0027).
    pub entry_path: PathBuf,
}

/// One entry that has been moved out of the scan root and recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedEntry {
    pub skill_id: SkillId,
    pub declared_name: String,
    /// Where it came from, and where a restore puts it back. Recorded rather
    /// than recomputed: the scan root is not enough to reconstruct it, since a
    /// project skill's origin is its own repo.
    pub origin_path: PathBuf,
    /// Where its bytes sit now, under `<storage_root>/<unit id>/`.
    pub stored_path: PathBuf,
    /// Disk bytes, walked once at removal (ADR 0029). Nothing writes into the
    /// trash, so the figure cannot go stale, and re-walking 1.1 GB to render a
    /// number on every panel open is not a trade worth making.
    pub bytes: u64,
}

/// One removal, and one undo (ADR 0027). The primary is a field rather than the
/// first element of an entry list so that "a unit always has exactly one entry
/// the user acted on" is a fact about the type instead of an invariant a reader
/// has to be told about and a writer has to remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashUnit {
    pub id: TrashUnitId,
    pub retention: Retention,
    /// Unix epoch millis. "Now" is injected at the command boundary, as
    /// everywhere else in the core (issue #14), so age is the panel's
    /// subtraction to do, not this layer's.
    pub removed_at_millis: i64,
    /// The entry the user acted on.
    pub primary: TrashedEntry,
    /// Every skill that resolved into the primary, cascaded as part of the same
    /// unit (ADR 0027). Non-empty exactly when this is a tool uninstall.
    pub dependents: Vec<TrashedEntry>,
}

impl TrashUnit {
    /// Primary first, then dependents -- the ordinal order the store round-trips
    /// and the order a restore replays.
    pub fn entries(&self) -> impl Iterator<Item = &TrashedEntry> {
        std::iter::once(&self.primary).chain(self.dependents.iter())
    }

    /// Whether this removal was a tool uninstall rather than a skill removal
    /// (ADR 0027). Derived, not stored: a unit has dependents *because* the
    /// removed row was a manager root, and a plain delete is only offered when
    /// there are none -- so a stored flag would be a second source of truth that
    /// could contradict the entry list.
    pub fn is_tool_uninstall(&self) -> bool {
        !self.dependents.is_empty()
    }

    pub fn entry_count(&self) -> usize {
        1 + self.dependents.len()
    }

    /// What purging this unit reclaims.
    ///
    /// A **floor**, for the reason ADR 0027 gives about `provides_for`: skillmon
    /// scans only Claude Code's paths, so a managing tool's entries for other
    /// agents are neither cascaded nor counted here. This is not the tool's disk
    /// footprint and must not be presented as one.
    pub fn bytes(&self) -> u64 {
        self.entries().map(|e| e.bytes).sum()
    }

    /// The directory holding this unit's staged entries, derived from where the
    /// primary actually sits rather than stored again. One less column that
    /// could disagree with the paths a purge is about to delete.
    pub fn storage_dir(&self) -> Option<&std::path::Path> {
        self.primary.stored_path.parent()
    }
}

/// The retained "(removed)" marker for an uninstalled skill (DESIGN.md UX #6).
///
/// Deliberately thin, because the history it exists for is not in here: usage is
/// global and keyed by `message.id` (ADR 0024), and removing a skill deletes
/// none of it. So this row only answers "is this skill listed?", and continuity
/// on reinstall is the *absence* of a deletion rather than a recovery.
///
/// Outlives the bytes. A purge drops the unit and keeps this, so a user can
/// reclaim a gigabyte and still have honest totals (ADR 0029).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub skill_id: SkillId,
    pub declared_name: String,
    pub removed_at_millis: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, bytes: u64) -> TrashedEntry {
        TrashedEntry {
            skill_id: SkillId::Personal { name: name.to_string() },
            declared_name: name.to_string(),
            origin_path: PathBuf::from(format!("/home/me/.claude/skills/{name}")),
            stored_path: PathBuf::from(format!("/home/me/.claude/skillmon/removed/1/0-{name}")),
            bytes,
        }
    }

    fn unit(primary: TrashedEntry, dependents: Vec<TrashedEntry>) -> TrashUnit {
        TrashUnit {
            id: TrashUnitId(1),
            retention: Retention::Trashed,
            removed_at_millis: 1_700_000_000_000,
            primary,
            dependents,
        }
    }

    #[test]
    fn retention_round_trips_through_its_stored_discriminant() {
        for r in [Retention::Disabled, Retention::Trashed] {
            assert_eq!(Retention::parse(r.as_str()), Some(r));
        }
        assert_eq!(Retention::parse("quarantined"), None);
    }

    /// A lone entry is a skill removal: ADR 0027 offers a plain delete only
    /// where `provides_for == 0`, so nothing cascaded.
    #[test]
    fn a_unit_with_no_dependents_is_a_skill_removal() {
        let u = unit(entry("vercel-react", 12_000), vec![]);
        assert!(!u.is_tool_uninstall());
        assert_eq!(u.entry_count(), 1);
        assert_eq!(u.bytes(), 12_000);
    }

    /// gstack's shape: the entry that *is* the checkout, plus the 46 shims that
    /// resolve into it, as one unit with one undo (ADR 0027).
    #[test]
    fn a_unit_with_dependents_is_a_tool_uninstall_and_sums_every_entry() {
        let shims: Vec<TrashedEntry> = (0..46).map(|i| entry(&format!("shim-{i}"), 100)).collect();
        let u = unit(entry("gstack", 1_100_000_000), shims);

        assert!(u.is_tool_uninstall());
        assert_eq!(u.entry_count(), 47);
        assert_eq!(u.bytes(), 1_100_000_000 + 46 * 100);
    }

    #[test]
    fn entries_yields_the_primary_first_then_dependents_in_order() {
        let u = unit(entry("gstack", 1), vec![entry("ship", 2), entry("review", 3)]);
        let names: Vec<&str> = u.entries().map(|e| e.skill_id.name()).collect();
        assert_eq!(names, vec!["gstack", "ship", "review"]);
    }

    #[test]
    fn storage_dir_is_the_parent_of_the_primarys_stored_path() {
        let u = unit(entry("gstack", 1), vec![]);
        assert_eq!(u.storage_dir(), Some(std::path::Path::new("/home/me/.claude/skillmon/removed/1")));
    }
}
