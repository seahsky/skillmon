use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::discovery::transcript::RepoInfo;
use super::paths;

/// ADR 0019: "debounced (~500ms) so a multi-file operation like a plugin
/// install triggers one rescan, not several." Matches
/// `notify_debouncer_mini::Config`'s own 500ms default; named explicitly
/// here so the ADR's number and the code stay visibly in sync.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

/// Watches the registry surfaces ADR 0019 names -- the personal skills dir,
/// `installed_plugins.json`, the global `settings.json`, each known repo's
/// `.claude/skills/`, and the active repo's `settings.json` /
/// `settings.local.json` -- and calls back (debounced) when any of them
/// change. Claude-Code-specific paths live here, not generic core (ADR
/// 0002). Carries no opinion about what a "rescan" does; that's the
/// caller's job (see `ClaudeCodeAdapter::discover_skills`).
pub struct RegistryWatcher {
    debouncer: Debouncer<notify::RecommendedWatcher>,
    watched: HashSet<PathBuf>,
}

impl RegistryWatcher {
    /// Starts watching nothing yet -- call `sync` right after construction
    /// (with whatever `known_repos`/`active_repo` a first `discover_skills`
    /// call produced) to pick up the static global paths and any repos
    /// already known at startup.
    pub fn new(on_change: impl Fn() + Send + 'static) -> notify::Result<Self> {
        let debouncer = new_debouncer(DEBOUNCE_WINDOW, move |result: DebounceEventResult| {
            if matches!(result, Ok(events) if !events.is_empty()) {
                on_change();
            }
        })?;
        Ok(Self { debouncer, watched: HashSet::new() })
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
}
