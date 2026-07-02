use crate::adapters::claude_code::paths::{
    global_settings_path, repo_local_settings_path, repo_settings_path,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Default)]
struct SettingsFile {
    #[serde(rename = "enabledPlugins", default)]
    enabled_plugins: HashMap<String, bool>,
}

fn read_enabled_plugins(path: &Path) -> HashMap<String, bool> {
    fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str::<SettingsFile>(&c).ok())
        .map(|s| s.enabled_plugins)
        .unwrap_or_default()
}

/// A plugin is live if enabled in any applicable scope: global settings,
/// plus the active repo's project and local settings when a repo is active.
/// An OR across sources, not a precedence order (ADR 0015).
pub fn is_plugin_live(plugin_at_marketplace: &str, claude_home: &Path, active_repo_path: Option<&Path>) -> bool {
    let global = read_enabled_plugins(&global_settings_path(claude_home));
    if global.get(plugin_at_marketplace).copied().unwrap_or(false) {
        return true;
    }

    if let Some(repo) = active_repo_path {
        let project = read_enabled_plugins(&repo_settings_path(repo));
        if project.get(plugin_at_marketplace).copied().unwrap_or(false) {
            return true;
        }

        let local = read_enabled_plugins(&repo_local_settings_path(repo));
        if local.get(plugin_at_marketplace).copied().unwrap_or(false) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_settings(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn live_when_enabled_globally() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        write_settings(
            &global_settings_path(claude_home),
            r#"{"enabledPlugins": {"foo@bar": true}}"#,
        );

        assert!(is_plugin_live("foo@bar", claude_home, None));
    }

    #[test]
    fn live_when_enabled_only_in_active_repo_project_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let repo = tmp.path().join("repo");
        write_settings(&global_settings_path(claude_home), r#"{"enabledPlugins": {}}"#);
        write_settings(
            &repo_settings_path(&repo),
            r#"{"enabledPlugins": {"foo@bar": true}}"#,
        );

        assert!(is_plugin_live("foo@bar", claude_home, Some(&repo)));
    }

    #[test]
    fn live_when_enabled_only_in_active_repo_local_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let repo = tmp.path().join("repo");
        write_settings(
            &repo_local_settings_path(&repo),
            r#"{"enabledPlugins": {"foo@bar": true}}"#,
        );

        assert!(is_plugin_live("foo@bar", claude_home, Some(&repo)));
    }

    #[test]
    fn not_live_when_enabled_nowhere_applicable() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let repo = tmp.path().join("repo");

        assert!(!is_plugin_live("foo@bar", claude_home, Some(&repo)));
    }

    #[test]
    fn project_scoped_enable_in_a_different_non_active_repo_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let other_repo = tmp.path().join("other-repo");
        write_settings(
            &repo_settings_path(&other_repo),
            r#"{"enabledPlugins": {"foo@bar": true}}"#,
        );

        // active repo is a *different* repo than the one with the enable
        let active_repo = tmp.path().join("active-repo");
        assert!(!is_plugin_live("foo@bar", claude_home, Some(&active_repo)));
    }
}
