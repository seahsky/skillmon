use std::path::{Path, PathBuf};

pub fn default_claude_home() -> PathBuf {
    dirs::home_dir()
        .expect("home directory must be resolvable")
        .join(".claude")
}

pub fn personal_skills_dir(claude_home: &Path) -> PathBuf {
    claude_home.join("skills")
}

pub fn projects_dir(claude_home: &Path) -> PathBuf {
    claude_home.join("projects")
}

pub fn installed_plugins_path(claude_home: &Path) -> PathBuf {
    claude_home.join("plugins").join("installed_plugins.json")
}

pub fn global_settings_path(claude_home: &Path) -> PathBuf {
    claude_home.join("settings.json")
}

pub fn repo_skills_dir(repo_path: &Path) -> PathBuf {
    repo_path.join(".claude").join("skills")
}

pub fn repo_settings_path(repo_path: &Path) -> PathBuf {
    repo_path.join(".claude").join("settings.json")
}

pub fn repo_local_settings_path(repo_path: &Path) -> PathBuf {
    repo_path.join(".claude").join("settings.local.json")
}

/// Where personal and plugin entries are staged once they leave the scan root
/// (ADR 0029) -- both disabled and trashed, since the intent lives in state, not
/// in the destination (ADR 0027).
///
/// Under `~/.claude` but deliberately **not** under `~/.claude/skills`, which
/// `RegistryWatcher` watches recursively (ADR 0019). Staging bytes here
/// therefore triggers no rescan of skillmon's own work -- see
/// `the_removal_staging_root_sits_outside_every_recursively_watched_tree`. That
/// answers only half of issue #29: taking the entry out of the scan root is
/// still a write inside a watched tree, and still needs self-write suppression.
///
/// The adapter's contribution to the removal seam: `removal::remove` takes a
/// storage root and names no Claude Code path itself (ADR 0002). Its caller
/// lands with issue #31, hence `allow(dead_code)` -- the containment property
/// below is asserted now, because it is a precondition of that work, not a
/// consequence of it.
#[allow(dead_code)]
pub fn removed_dir(claude_home: &Path) -> PathBuf {
    claude_home.join("skillmon").join("removed")
}

/// A repo's own staging root. A project skill stays inside its repo (ADR 0007's
/// project locality), and lands outside that repo's watched `.claude/skills/`
/// for the same reason as above. Reserved for issue #31, as `removed_dir` is.
#[allow(dead_code)]
pub fn repo_removed_dir(repo_path: &Path) -> PathBuf {
    repo_path.join(".claude").join("skillmon").join("removed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_expected_relative_paths() {
        let home = Path::new("/tmp/fake-home/.claude");
        assert_eq!(personal_skills_dir(home), Path::new("/tmp/fake-home/.claude/skills"));
        assert_eq!(projects_dir(home), Path::new("/tmp/fake-home/.claude/projects"));
        assert_eq!(
            installed_plugins_path(home),
            Path::new("/tmp/fake-home/.claude/plugins/installed_plugins.json")
        );
        assert_eq!(global_settings_path(home), Path::new("/tmp/fake-home/.claude/settings.json"));

        let repo = Path::new("/tmp/some-repo");
        assert_eq!(repo_skills_dir(repo), Path::new("/tmp/some-repo/.claude/skills"));
        assert_eq!(repo_settings_path(repo), Path::new("/tmp/some-repo/.claude/settings.json"));
        assert_eq!(
            repo_local_settings_path(repo),
            Path::new("/tmp/some-repo/.claude/settings.local.json")
        );
        assert_eq!(removed_dir(home), Path::new("/tmp/fake-home/.claude/skillmon/removed"));
        assert_eq!(repo_removed_dir(repo), Path::new("/tmp/some-repo/.claude/skillmon/removed"));
    }

    /// Issue #29's second question, settled by assertion rather than by reading
    /// the layout: staging a removal must not trip the watcher that would rescan
    /// it. `RegistryWatcher::sync` watches exactly these two trees recursively
    /// (ADR 0019), so a staging root inside either would make every trash write
    /// self-triggering -- and a tool uninstall writes 47 of them.
    #[test]
    fn the_removal_staging_root_sits_outside_every_recursively_watched_tree() {
        let home = Path::new("/tmp/fake-home/.claude");
        assert!(
            !removed_dir(home).starts_with(personal_skills_dir(home)),
            "{} is inside the recursively watched {}",
            removed_dir(home).display(),
            personal_skills_dir(home).display()
        );

        let repo = Path::new("/tmp/some-repo");
        assert!(
            !repo_removed_dir(repo).starts_with(repo_skills_dir(repo)),
            "{} is inside the recursively watched {}",
            repo_removed_dir(repo).display(),
            repo_skills_dir(repo).display()
        );
        // A repo's staging root must also stay clear of the *personal* watched
        // tree, which it would not if a repo ever lived under `~/.claude/skills`.
        assert!(!repo_removed_dir(repo).starts_with(personal_skills_dir(home)));
    }
}
