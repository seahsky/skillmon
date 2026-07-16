use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::discovery::transcript::RepoInfo;
use super::paths;
use crate::self_write::SelfWriteWindow;

/// ADR 0019: "debounced (~500ms) so a multi-file operation like a plugin
/// install triggers one rescan, not several." Matches
/// `notify_debouncer_mini::Config`'s own 500ms default; named explicitly
/// here so the ADR's number and the code stay visibly in sync.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

/// How long self-write suppression outlives the write it is suppressing (issue
/// #29, ADR 0019 Update 4).
///
/// A mutation's guard drops when `rename(2)` returns, but the event that rename
/// raised has not been *delivered* yet: the debouncer holds a batch for a whole
/// `DEBOUNCE_WINDOW` after its last event. So suppression that ended with the
/// write would suppress nothing whatsoever -- every self-write event would land
/// just after the latch closed.
///
/// Derived from the debounce rather than picked, so the two cannot drift: one
/// window for the debouncer to deliver the batch holding our last write, and one
/// more for the platform's own delivery latency and scheduler slop. Overshooting
/// costs a slightly wider blind spot; undershooting costs the entire mechanism.
const SELF_WRITE_TAIL: Duration = Duration::from_millis(DEBOUNCE_WINDOW.as_millis() as u64 * 2);

/// Watches the registry surfaces ADR 0019 names -- the personal skills dir,
/// `installed_plugins.json`, the global `settings.json`, each known repo's
/// `.claude/skills/`, and the active repo's `settings.json` /
/// `settings.local.json` -- and calls back (debounced) when any of them
/// change. Claude-Code-specific paths live here, not generic core (ADR
/// 0002). Carries no opinion about what a "rescan" does; that's the
/// caller's job (see `ClaudeCodeAdapter::discover_skills`).
///
/// It does carry one opinion about what a rescan is *not*: a reaction to
/// skillmon's own writes. See `self_writes`.
pub struct RegistryWatcher {
    debouncer: Debouncer<notify::RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    self_writes: SelfWriteWindow,
}

impl RegistryWatcher {
    /// Starts watching nothing yet -- call `sync` right after construction
    /// (with whatever `known_repos`/`active_repo` a first `discover_skills`
    /// call produced) to pick up the static global paths and any repos
    /// already known at startup.
    pub fn new(on_change: impl Fn() + Send + 'static) -> notify::Result<Self> {
        let self_writes = SelfWriteWindow::new(SELF_WRITE_TAIL);

        let suppressed = self_writes.clone();
        let debouncer = new_debouncer(DEBOUNCE_WINDOW, move |result: DebounceEventResult| {
            if matches!(result, Ok(events) if !events.is_empty()) && !suppressed.is_open() {
                on_change();
            }
        })?;
        Ok(Self { debouncer, watched: HashSet::new(), self_writes })
    }

    /// A handle on the latch every skillmon mutation must hold while it writes
    /// inside a watched tree (issue #29).
    ///
    /// The watcher hands this out rather than accepting one because sizing the
    /// tail needs `DEBOUNCE_WINDOW`, which is this module's own fact -- a
    /// composition root passing in a duration would be guessing at it.
    ///
    /// A suppressed mutation owes the panel the refresh the watcher no longer
    /// sends: it emits `registry-changed` itself when its ledger write is
    /// settled. One rescan of a finished tree, rather than 47 of a half-removed
    /// one.
    pub fn self_writes(&self) -> SelfWriteWindow {
        self.self_writes.clone()
    }

    /// Reconciles the watched path set against the registry surfaces ADR
    /// 0019 names for the given `claude_home`/`known_repos`/`active_repo`.
    /// Adds newly-relevant paths that exist on disk; drops previously-
    /// watched paths that are no longer relevant (most commonly: the old
    /// active repo's settings files, once a different repo becomes
    /// active). Idempotent and cheap to call repeatedly -- an already-
    /// watched path, or one that doesn't exist yet, is left alone.
    pub fn sync(&mut self, claude_home: &Path, known_repos: &[RepoInfo], active_repo: Option<&RepoInfo>) {
        let mut desired: HashMap<PathBuf, RecursiveMode> = HashMap::new();
        desired.insert(paths::personal_skills_dir(claude_home), RecursiveMode::Recursive);
        desired.insert(paths::installed_plugins_path(claude_home), RecursiveMode::NonRecursive);
        desired.insert(paths::global_settings_path(claude_home), RecursiveMode::NonRecursive);
        for repo in known_repos {
            desired.insert(paths::repo_skills_dir(&repo.repo_path), RecursiveMode::Recursive);
        }
        if let Some(repo) = active_repo {
            desired.insert(paths::repo_settings_path(&repo.repo_path), RecursiveMode::NonRecursive);
            desired.insert(paths::repo_local_settings_path(&repo.repo_path), RecursiveMode::NonRecursive);
        }

        let watcher = self.debouncer.watcher();

        for (path, mode) in &desired {
            if !self.watched.contains(path) && path.exists() && watcher.watch(path, *mode).is_ok() {
                self.watched.insert(path.clone());
            }
        }

        let stale: Vec<PathBuf> = self.watched.iter().filter(|p| !desired.contains_key(*p)).cloned().collect();
        for path in stale {
            let _ = watcher.unwatch(&path);
            self.watched.remove(&path);
        }
    }

    #[cfg(test)]
    pub fn is_watching(&self, path: &Path) -> bool {
        self.watched.contains(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc::{channel, RecvTimeoutError};
    use std::time::Duration as StdDuration;

    fn wait_for_tick(rx: &std::sync::mpsc::Receiver<()>) -> bool {
        // Debounce window is 500ms; give it real headroom on a loaded CI box.
        !matches!(rx.recv_timeout(StdDuration::from_secs(3)), Err(RecvTimeoutError::Timeout))
    }

    /// Asserting an *absence* cannot be done by waiting for a timeout to expire
    /// and hoping: the wait has to outlast every delay that could still deliver
    /// the tick. That is the debounce plus the suppression tail plus slack --
    /// deliberately longer than `SELF_WRITE_TAIL`, so a tick that was merely
    /// *late* still fails the test rather than passing it.
    fn expect_no_tick(rx: &std::sync::mpsc::Receiver<()>) -> bool {
        matches!(
            rx.recv_timeout(DEBOUNCE_WINDOW + SELF_WRITE_TAIL + StdDuration::from_secs(1)),
            Err(RecvTimeoutError::Timeout)
        )
    }

    /// Consumes every queued tick and waits for the watcher to fall silent.
    ///
    /// Building a fixture is itself a change to a watched tree, and one that
    /// creates nine directories raises far more than one event -- the debouncer
    /// emits them as several batches, so several ticks queue up. `wait_for_tick`
    /// takes only the first, and the channel is a queue rather than a latest-
    /// value: without draining the rest, the next `expect_no_tick` reads a
    /// leftover from the *setup* and reports it as the mutation's own.
    ///
    /// This costs a real debounce window per call, which is why it is only used
    /// where a test writes before it measures.
    fn drain_until_quiet(rx: &std::sync::mpsc::Receiver<()>) {
        while rx.recv_timeout(DEBOUNCE_WINDOW + StdDuration::from_millis(500)).is_ok() {}
    }

    #[test]
    fn sync_watches_the_static_global_paths_and_reacts_to_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(claude_home.join("skills")).unwrap();

        let (tx, rx) = channel();
        let mut watcher = RegistryWatcher::new(move || {
            let _ = tx.send(());
        })
        .unwrap();
        watcher.sync(&claude_home, &[], None);

        assert!(watcher.is_watching(&paths::personal_skills_dir(&claude_home)));

        fs::write(claude_home.join("skills").join("new-file.md"), "content").unwrap();
        assert!(wait_for_tick(&rx), "expected a debounced tick after writing into the watched skills dir");
    }

    #[test]
    fn sync_adds_a_known_repos_skills_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(claude_home.join("skills")).unwrap();
        let repo_path = tmp.path().join("repo");
        let repo_skills_dir = repo_path.join(".claude").join("skills");
        fs::create_dir_all(&repo_skills_dir).unwrap();

        let (tx, rx) = channel();
        let mut watcher = RegistryWatcher::new(move || {
            let _ = tx.send(());
        })
        .unwrap();
        let repo = RepoInfo { repo_path: repo_path.clone(), project_dir: tmp.path().join("proj"), last_modified: std::time::SystemTime::now() };
        watcher.sync(&claude_home, &[repo], None);

        assert!(watcher.is_watching(&repo_skills_dir));

        fs::write(repo_skills_dir.join("new-file.md"), "content").unwrap();
        assert!(wait_for_tick(&rx), "expected a debounced tick after writing into a known repo's skills dir");
    }

    #[test]
    fn sync_swaps_active_repo_settings_watches_when_the_active_repo_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(claude_home.join("skills")).unwrap();

        let repo_a = tmp.path().join("repo-a");
        fs::create_dir_all(repo_a.join(".claude")).unwrap();
        fs::write(repo_a.join(".claude").join("settings.json"), "{}").unwrap();

        let repo_b = tmp.path().join("repo-b");
        fs::create_dir_all(repo_b.join(".claude")).unwrap();
        fs::write(repo_b.join(".claude").join("settings.json"), "{}").unwrap();

        let (tx, _rx) = channel();
        let mut watcher = RegistryWatcher::new(move || {
            let _ = tx.send(());
        })
        .unwrap();

        let info_a = RepoInfo { repo_path: repo_a.clone(), project_dir: tmp.path().join("proj-a"), last_modified: std::time::SystemTime::now() };
        let info_b = RepoInfo { repo_path: repo_b.clone(), project_dir: tmp.path().join("proj-b"), last_modified: std::time::SystemTime::now() };

        watcher.sync(&claude_home, &[info_a.clone(), info_b.clone()], Some(&info_a));
        assert!(watcher.is_watching(&paths::repo_settings_path(&repo_a)));
        assert!(!watcher.is_watching(&paths::repo_settings_path(&repo_b)));

        watcher.sync(&claude_home, &[info_a.clone(), info_b.clone()], Some(&info_b));
        assert!(!watcher.is_watching(&paths::repo_settings_path(&repo_a)), "old active repo's settings should be unwatched");
        assert!(watcher.is_watching(&paths::repo_settings_path(&repo_b)), "new active repo's settings should be watched");
    }

    /// Builds a watcher already watching `<tmp>/.claude/skills`, and returns the
    /// scan root to write into alongside it.
    fn watching_skills_dir(tmp: &tempfile::TempDir) -> (RegistryWatcher, PathBuf, std::sync::mpsc::Receiver<()>) {
        let claude_home = tmp.path().join(".claude");
        let skills = claude_home.join("skills");
        fs::create_dir_all(&skills).unwrap();

        let (tx, rx) = channel();
        let mut watcher = RegistryWatcher::new(move || {
            let _ = tx.send(());
        })
        .unwrap();
        watcher.sync(&claude_home, &[], None);
        (watcher, skills, rx)
    }

    /// Issue #29 at its smallest: a removal takes entries out of the
    /// recursively watched scan root, so without suppression skillmon rescans
    /// its own work. The cascade this actually happens at is asserted below;
    /// this pins the base case, and pins it against a *live* watcher -- the
    /// precondition is what keeps "no tick" from meaning "nothing was
    /// listening".
    #[test]
    fn a_write_skillmon_makes_itself_raises_no_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let (watcher, skills, rx) = watching_skills_dir(&tmp);
        fs::write(skills.join("doomed.md"), "content").unwrap();
        assert!(wait_for_tick(&rx), "precondition: this write is one the watcher would normally react to");

        drain_until_quiet(&rx);

        let _writing = watcher.self_writes().open();
        fs::remove_file(skills.join("doomed.md")).unwrap();

        assert!(expect_no_tick(&rx), "skillmon's own removal must not trigger a rescan of skillmon's own work");
    }

    /// The case the tail exists for, and the one a hand-rolled flag gets wrong.
    /// The guard is released the moment the move returns -- but the event that
    /// move raised is still sitting in the debouncer, and lands ~500ms later. A
    /// latch that closed with the write would suppress precisely nothing.
    #[test]
    fn an_event_delivered_after_the_mutation_finished_is_still_suppressed() {
        let tmp = tempfile::tempdir().unwrap();
        let (watcher, skills, rx) = watching_skills_dir(&tmp);

        {
            let _writing = watcher.self_writes().open();
            fs::write(skills.join("staged.md"), "content").unwrap();
        } // the mutation returns here, long before its event is delivered

        assert!(expect_no_tick(&rx), "the tail must outlive the write, or it suppresses nothing at all");
    }

    /// Issue #29's scenario as it actually happens, rather than as a stand-in
    /// for it: a real `removal::remove` cascade -- the gstack shape, a primary
    /// plus dependents, one unit and one ledger transaction -- moving real
    /// entries out of a really-watched scan root.
    ///
    /// The composition this asserts is `lib.rs`'s (a guard held across the
    /// mutation, the watcher consulting it), which is why it lives with the
    /// watcher and not in `removal/` -- that module knows nothing of watchers
    /// and must keep it that way. What would otherwise happen here is the
    /// issue's own words: a rescan per entry, racing the ledger write, against a
    /// tree that is half removed at that moment.
    #[test]
    fn a_real_removal_cascade_raises_no_tick_and_stages_outside_the_watched_tree() {
        use crate::domain::removal::{EntryToRemove, Retention};
        use crate::domain::skill::SkillId;
        use crate::removal;

        let tmp = tempfile::tempdir().unwrap();
        let (watcher, skills, rx) = watching_skills_dir(&tmp);

        let install = |name: &str| {
            fs::create_dir_all(skills.join(name)).unwrap();
            fs::write(skills.join(name).join("SKILL.md"), "body").unwrap();
            EntryToRemove {
                skill_id: SkillId::Personal { name: name.to_string() },
                declared_name: name.to_string(),
                entry_path: skills.join(name),
            }
        };
        let primary = install("gstack");
        let dependents: Vec<EntryToRemove> = (0..8).map(|i| install(&format!("shim-{i}"))).collect();
        // Installing them was itself a change to the watched tree, and a real
        // one -- so prove the watcher reacts to it, then let it fall silent, and
        // what follows measures only the removal.
        assert!(wait_for_tick(&rx), "precondition: these writes are ones the watcher reacts to");
        drain_until_quiet(&rx);

        let mut store = removal::store::TrashStore::open_in_memory().unwrap();
        // The real staging root's shape (`paths::removed_dir`), whose containment
        // outside every watched tree `paths.rs` asserts separately.
        let storage_root = paths::removed_dir(&tmp.path().join(".claude"));
        {
            let _writing = watcher.self_writes().open();
            removal::remove(&mut store, &storage_root, 1_000, Retention::Trashed, primary, dependents).unwrap();
        }

        assert!(!skills.join("gstack").exists(), "precondition: the cascade really did move 9 entries");
        assert!(expect_no_tick(&rx), "a tool uninstall must not rescan its own half-removed tree");
    }

    /// Suppression is a window, not a switch someone has to remember to flip
    /// back: a watcher that stayed deaf after one mutation would be worse than
    /// no suppression, because nothing would ever surface the breakage.
    #[test]
    fn the_watcher_reacts_again_once_the_window_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let (watcher, skills, rx) = watching_skills_dir(&tmp);

        {
            let _writing = watcher.self_writes().open();
            fs::write(skills.join("mine.md"), "content").unwrap();
        }
        assert!(expect_no_tick(&rx));

        // Whatever else changed `~/.claude/skills`, the watcher is listening.
        fs::write(skills.join("someone-elses.md"), "content").unwrap();
        assert!(wait_for_tick(&rx), "the window must close on its own");
    }
}
