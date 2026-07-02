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
    }
}
