use crate::adapters::claude_code::discovery::scan::discover_skills_in_dir;
use crate::adapters::claude_code::paths::installed_plugins_path;
use crate::domain::skill::{DiscoveredSkill, DiscoveryWarning, InstallScope, SkillId};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct InstalledPluginsFile {
    plugins: HashMap<String, Vec<InstallRecordRaw>>,
}

#[derive(Debug, Deserialize)]
struct InstallRecordRaw {
    scope: String,
    #[serde(rename = "installPath")]
    install_path: String,
}

#[derive(Debug, Clone)]
pub struct PluginInstallRecord {
    pub plugin_at_marketplace: String,
    pub plugin: String,
    pub marketplace: String,
    pub scope: InstallScope,
    pub install_path: PathBuf,
}

/// Reads `installed_plugins.json` verbatim. Never reconstructs `installPath`
/// from `cache/<marketplace>/<plugin>/<version>/` -- a real version
/// directory can be named `unknown` (ADR 0014's neighbor decision).
pub fn parse_installed_plugins(claude_home: &Path) -> Vec<PluginInstallRecord> {
    let path = installed_plugins_path(claude_home);
    let Ok(content) = fs::read_to_string(&path) else { return Vec::new() };
    let Ok(parsed) = serde_json::from_str::<InstalledPluginsFile>(&content) else { return Vec::new() };

    parsed
        .plugins
        .into_iter()
        .flat_map(|(key, records)| {
            let (plugin, marketplace) = split_plugin_key(&key);
            records.into_iter().filter_map(move |r| {
                let scope = parse_scope(&r.scope)?;
                Some(PluginInstallRecord {
                    plugin_at_marketplace: key.clone(),
                    plugin: plugin.clone(),
                    marketplace: marketplace.clone(),
                    scope,
                    install_path: PathBuf::from(r.install_path),
                })
            })
        })
        .collect()
}

fn split_plugin_key(key: &str) -> (String, String) {
    match key.split_once('@') {
        Some((plugin, marketplace)) => (plugin.to_string(), marketplace.to_string()),
        None => (key.to_string(), String::new()),
    }
}

fn parse_scope(raw: &str) -> Option<InstallScope> {
    match raw {
        "user" => Some(InstallScope::User),
        "project" => Some(InstallScope::Project),
        "local" => Some(InstallScope::Local),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct PluginManifest {
    skills: Option<String>,
}

/// Honors `plugin.json`'s own relocation field instead of assuming `skills/`.
fn resolve_skills_dir(install_path: &Path) -> PathBuf {
    let manifest_path = install_path.join("plugin.json");
    let relocated = fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|c| serde_json::from_str::<PluginManifest>(&c).ok())
        .and_then(|m| m.skills);

    match relocated {
        Some(rel) => install_path.join(rel),
        None => install_path.join("skills"),
    }
}

pub fn discover_plugin_skills(record: &PluginInstallRecord) -> (Vec<DiscoveredSkill>, Vec<DiscoveryWarning>) {
    if !record.install_path.exists() {
        return (
            Vec::new(),
            vec![DiscoveryWarning {
                path: record.install_path.clone(),
                reason: format!("installPath for {} does not exist on disk", record.plugin_at_marketplace),
            }],
        );
    }

    let skills_dir = resolve_skills_dir(&record.install_path);
    let marketplace = record.marketplace.clone();
    let plugin = record.plugin.clone();
    discover_skills_in_dir(&skills_dir, move |name| SkillId::Plugin {
        marketplace: marketplace.clone(),
        plugin: plugin.clone(),
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_installed_plugins(claude_home: &Path, body: &str) {
        fs::create_dir_all(claude_home.join("plugins")).unwrap();
        fs::write(claude_home.join("plugins").join("installed_plugins.json"), body).unwrap();
    }

    #[test]
    fn parses_install_path_verbatim_even_when_version_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        write_installed_plugins(
            claude_home,
            r#"{
                "version": 2,
                "plugins": {
                    "serena@claude-plugins-official": [
                        {
                            "scope": "user",
                            "installPath": "/Users/test/.claude/plugins/cache/claude-plugins-official/serena/unknown",
                            "version": "unknown",
                            "installedAt": "2025-12-27T13:20:09.785Z"
                        }
                    ]
                }
            }"#,
        );

        let records = parse_installed_plugins(claude_home);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].plugin, "serena");
        assert_eq!(records[0].marketplace, "claude-plugins-official");
        assert_eq!(records[0].scope, InstallScope::User);
        assert_eq!(
            records[0].install_path,
            PathBuf::from("/Users/test/.claude/plugins/cache/claude-plugins-official/serena/unknown")
        );
    }

    #[test]
    fn multiple_install_records_for_one_plugin_key_are_all_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        write_installed_plugins(
            claude_home,
            r#"{
                "version": 2,
                "plugins": {
                    "foo@bar": [
                        {"scope": "user", "installPath": "/a", "version": "1.0.0"},
                        {"scope": "project", "installPath": "/b", "version": "1.0.0"}
                    ]
                }
            }"#,
        );

        let records = parse_installed_plugins(claude_home);
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|r| r.scope == InstallScope::User));
        assert!(records.iter().any(|r| r.scope == InstallScope::Project));
    }

    #[test]
    fn missing_registry_file_yields_no_records() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(parse_installed_plugins(tmp.path()).is_empty());
    }

    #[test]
    fn discovers_skills_at_install_path_honoring_manifest_relocation() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        fs::write(install_path.join("plugin.json"), r#"{"skills": "./.claude/skills"}"#).unwrap();
        let skill_dir = install_path.join(".claude").join("skills").join("reviewer");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: reviewer\ndescription: reviews code\n---\n\nBody.\n",
        )
        .unwrap();

        let record = PluginInstallRecord {
            plugin_at_marketplace: "test-plugin@test-market".to_string(),
            plugin: "test-plugin".to_string(),
            marketplace: "test-market".to_string(),
            scope: InstallScope::User,
            install_path: install_path.clone(),
        };

        let (skills, warnings) = discover_plugin_skills(&record);

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        match &skills[0].id {
            SkillId::Plugin { marketplace, plugin, name } => {
                assert_eq!(marketplace, "test-market");
                assert_eq!(plugin, "test-plugin");
                assert_eq!(name, "reviewer");
            }
            other => panic!("expected Plugin id, got {other:?}"),
        }
    }

    #[test]
    fn missing_install_path_is_skipped_and_warned() {
        let tmp = tempfile::tempdir().unwrap();
        let record = PluginInstallRecord {
            plugin_at_marketplace: "ghost@nowhere".to_string(),
            plugin: "ghost".to_string(),
            marketplace: "nowhere".to_string(),
            scope: InstallScope::User,
            install_path: tmp.path().join("does-not-exist"),
        };

        let (skills, warnings) = discover_plugin_skills(&record);

        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("does not exist on disk"));
    }
}
