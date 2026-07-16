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
/// still a write inside a watched tree. The other half is
/// `SelfWriteWindow` (ADR 0019 Update 4), the latch a mutation holds across its
/// writes -- these two facts are independent, and both are needed.
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

    /// The same invariant, against the real `~/.claude` and its real symlinks --
    /// because the test above cannot see them.
    ///
    /// `starts_with` compares path *components*, so it answers the question as
    /// written on disk, not as resolved. If `~/.claude/skills` were itself a
    /// symlink to a tree that enclosed `~/.claude/skillmon`, the assertion above
    /// would still pass while `notify` -- which watches the resolved tree --
    /// watched every byte a removal staged, and every trash write would be
    /// self-triggering. Nothing in the layout forbids that; only the actual
    /// filesystem settles it, so it is asserted rather than assumed (as
    /// `real_claude_home_subagent_rollup` asserts its own inertness).
    ///
    /// Read-only: it resolves paths and never writes, so it is safe against a
    /// live install. `#[ignore]`d because it depends on this machine's `~`.
    ///
    /// Run by hand:
    /// `cargo test --manifest-path src-tauri/Cargo.toml
    /// adapters::claude_code::paths::tests::the_real_claude_home_keeps_staging_outside_the_watched_tree
    /// -- --ignored --exact --nocapture`
    #[test]
    #[ignore]
    fn the_real_claude_home_keeps_staging_outside_the_watched_tree() {
        let home = default_claude_home();
        let skills = personal_skills_dir(&home);
        assert!(skills.is_dir(), "no {} -- is this machine's ~/.claude populated?", skills.display());

        // The staging root does not exist until the first removal, and
        // `canonicalize` requires the path to exist -- so resolve its nearest
        // existing ancestor (`~/.claude`) and let `removed_dir` build the tail
        // onto it. Through the real function, never a literal spelling of what
        // it returns: a test that rebuilds the path by hand keeps passing after
        // `removed_dir` moves the staging root somewhere self-triggering, which
        // is the one thing it exists to catch.
        let real_home = home.canonicalize().expect("~/.claude must resolve");
        let real_skills = skills.canonicalize().expect("~/.claude/skills must resolve");
        let real_removed = removed_dir(&real_home);

        assert!(
            !real_removed.starts_with(&real_skills),
            "staging at {} lands inside the recursively watched {} -- every trash write would rescan itself",
            real_removed.display(),
            real_skills.display()
        );
        eprintln!(
            "\n=== real layout: staging {} is outside the watched {} (skills symlink: {}) ===\n",
            real_removed.display(),
            real_skills.display(),
            std::fs::symlink_metadata(&skills).map(|m| m.is_symlink()).unwrap_or(false),
        );
    }
}
