use crate::adapters::claude_code::discovery::scan::{
    discover_skill_at_dir, discover_skills_in_dir, ChildDirs,
};
use crate::adapters::claude_code::paths::{installed_plugins_path, plugin_manifest_path};
use crate::domain::skill::{DiscoveredSkill, DiscoveryWarning, InstallScope, SkillId};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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
    /// Install provenance; read by plugin mutation ops (disable/enable per
    /// scope) in a later plan, not by discovery or footprint.
    #[allow(dead_code)]
    pub scope: InstallScope,
    pub install_path: PathBuf,
}

/// Reads `installed_plugins.json` verbatim. Never reconstructs `installPath`
/// from `cache/<marketplace>/<plugin>/<version>/` -- a real version
/// directory can be named `unknown` (ADR 0014's neighbor decision).
///
/// A missing file is the normal "nothing installed yet" case and stays
/// silent. A file that EXISTS but fails to read as UTF-8 or parse as JSON is
/// registry corruption -- since a corrupt registry silently hides every
/// installed plugin, that case is recorded as a `DiscoveryWarning` rather
/// than degrading to an empty result with no signal.
pub fn parse_installed_plugins(claude_home: &Path) -> (Vec<PluginInstallRecord>, Vec<DiscoveryWarning>) {
    let path = installed_plugins_path(claude_home);
    if !path.exists() {
        return (Vec::new(), Vec::new());
    }

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(err) => {
            return (
                Vec::new(),
                vec![DiscoveryWarning {
                    path,
                    reason: format!("installed_plugins.json exists but could not be read: {err}"),
                }],
            );
        }
    };

    let parsed = match serde_json::from_str::<InstalledPluginsFile>(&content) {
        Ok(p) => p,
        Err(err) => {
            return (
                Vec::new(),
                vec![DiscoveryWarning {
                    path,
                    reason: format!("installed_plugins.json exists but could not be parsed: {err}"),
                }],
            );
        }
    };

    let records = parsed
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
        .collect();

    (records, Vec::new())
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

/// A manifest's `skills` field, which Claude Code accepts as either a single
/// directory or a list of them. Modeling only the string form made
/// `serde_json::from_str` fail for the *whole* struct on an array -- and the
/// error was discarded, so `mattpocock-skills` reported none of its 22 declared
/// skills (issue #33).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SkillsDecl {
    One(String),
    Many(Vec<String>),
}

impl SkillsDecl {
    fn into_dirs(self) -> Vec<String> {
        match self {
            SkillsDecl::One(dir) => vec![dir],
            SkillsDecl::Many(dirs) => dirs,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PluginManifest {
    skills: Option<SkillsDecl>,
}

/// What a plugin's manifest says about where its skills live.
enum ManifestSkills {
    /// No manifest, or a manifest carrying no `skills` field -- 8 of the 11
    /// plugins on a real machine.
    Undeclared,
    Declared(Vec<String>),
    /// The manifest exists but could not be read or parsed. Never collapsed
    /// into `Undeclared`: what it declares is unknown, and a default assumed in
    /// its place can hide every skill in the plugin. The distinction is the
    /// whole lesson of this bug -- the discarded parse error is what let three
    /// separate defects hide behind a plausible-looking `skills/`.
    Unreadable,
}

/// Reads the plugin's own manifest. A missing manifest is ordinary; one that
/// exists but will not parse is recorded as a `DiscoveryWarning`, on the same
/// reasoning as a corrupt `installed_plugins.json` above -- it silently hides
/// skills, so it must not be silent.
fn read_manifest_skills(install_path: &Path) -> (ManifestSkills, Vec<DiscoveryWarning>) {
    let path = plugin_manifest_path(install_path);
    if !path.exists() {
        return (ManifestSkills::Undeclared, Vec::new());
    }

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(err) => {
            return (
                ManifestSkills::Unreadable,
                vec![DiscoveryWarning {
                    path,
                    reason: format!("plugin.json exists but could not be read: {err}"),
                }],
            );
        }
    };

    match serde_json::from_str::<PluginManifest>(&content) {
        Ok(manifest) => (
            manifest
                .skills
                .map_or(ManifestSkills::Undeclared, |s| ManifestSkills::Declared(s.into_dirs())),
            Vec::new(),
        ),
        Err(err) => (
            ManifestSkills::Unreadable,
            vec![DiscoveryWarning {
                path,
                reason: format!("plugin.json exists but could not be parsed: {err}"),
            }],
        ),
    }
}

/// A directory a plugin's skills can be found in.
struct SkillDir {
    path: PathBuf,
    /// Whether the manifest named this path. A declared path that is not on
    /// disk is an anomaly the manifest itself asserts, so it warns; a missing
    /// default `skills/` is ordinary -- 3 of 11 plugins ship none -- so it does
    /// not.
    declared: bool,
    children: ChildDirs,
}

/// Whether a candidate directory is a category tree rather than a flat list of
/// skill entries -- decided from what the plugin itself says, not from a rule
/// about plugins in general.
///
/// A manifest declaring a path *strictly under* this directory is the evidence:
/// the plugin has said its skills sit deeper, so the dirs in between are
/// organizational. `mattpocock-skills` declares `./skills/engineering/tdd` and
/// 21 more, which is exactly what makes its `skills/engineering` a category and
/// not a broken skill.
///
/// Deciding it this way keeps the warning where it still means something: a
/// plugin that declares nothing and ships `skills/foo/` with no `SKILL.md` is
/// reported, because nothing explains that directory. Silently reporting zero
/// skills is the failure this whole issue is about, and blanket-ignoring
/// non-skill children under every plugin would reintroduce it.
fn classify_children(dir: &Path, declared_paths: &[PathBuf]) -> ChildDirs {
    if declared_paths.iter().any(|p| p != dir && p.starts_with(dir)) {
        ChildDirs::MayBeCategories
    } else {
        ChildDirs::AreSkillEntries
    }
}

/// Every directory a plugin's skills can live in, deduped.
///
/// `skills` is the one manifest field that *adds to* its default rather than
/// replacing it: the default `skills/` is always scanned, and declared
/// directories are loaded alongside it. (The reference notes one exception, for
/// a marketplace entry whose `source` resolves to the marketplace root, where
/// declaring subdirectories replaces the default scan. Recognizing it needs
/// marketplace source resolution, which discovery does not do today; no plugin
/// on this machine takes that shape.)
fn skill_dirs(install_path: &Path, manifest: &ManifestSkills) -> Vec<SkillDir> {
    let default = install_path.join("skills");

    // The documented single-skill layout (Claude Code v2.1.142+): a `SKILL.md`
    // at the plugin root, no `skills/`, and no `skills` field. `Unreadable`
    // cannot establish that last condition, so it must not fire here -- an
    // unparsable manifest may well declare skills elsewhere.
    if matches!(manifest, ManifestSkills::Undeclared)
        && !default.is_dir()
        && install_path.join("SKILL.md").is_file()
    {
        return vec![SkillDir {
            path: install_path.to_path_buf(),
            declared: false,
            children: ChildDirs::AreSkillEntries,
        }];
    }

    let declared: Vec<PathBuf> = match manifest {
        ManifestSkills::Declared(dirs) => dirs.iter().map(|rel| install_path.join(rel)).collect(),
        ManifestSkills::Undeclared | ManifestSkills::Unreadable => Vec::new(),
    };

    let dirs = std::iter::once(default)
        .map(|path| (path, false))
        .chain(declared.iter().cloned().map(|path| (path, true)))
        .map(|(path, was_declared)| SkillDir {
            children: classify_children(&path, &declared),
            path,
            declared: was_declared,
        });

    // A manifest may name the default explicitly (`"skills": ["./skills", …]`),
    // which is the documented way to keep a default while adding to it. Resolved
    // rather than compared literally, since `./skills` and `skills` are the same
    // directory spelled two ways -- and a skill discovered twice is counted twice
    // in the always-on headline.
    let mut seen = HashSet::new();
    dirs.filter(|d| seen.insert(fs::canonicalize(&d.path).unwrap_or_else(|_| d.path.clone())))
        .collect()
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

    let (manifest, mut warnings) = read_manifest_skills(&record.install_path);
    let mut skills = Vec::new();

    for dir in skill_dirs(&record.install_path, &manifest) {
        if !dir.path.is_dir() {
            if dir.declared {
                warnings.push(DiscoveryWarning {
                    path: dir.path,
                    reason: format!(
                        "{} declares a skills path that is not a directory on disk",
                        record.plugin_at_marketplace
                    ),
                });
            }
            continue;
        }

        // A path holding `SKILL.md` directly names one skill; anything else is a
        // directory of them, scanned depth-1. Both stay depth-1 by construction,
        // which is what keeps the count honest: `mattpocock-skills` has 40
        // `SKILL.md` on disk and declares 22, and the undeclared 18 sit a level
        // below a category dir. Walking deeper would report skills that never
        // enter context (the error class of #26); walking shallower is how the
        // declared 22 came back as 0.
        let (found, found_warnings) = if dir.path.join("SKILL.md").is_file() {
            let (skill, w) = discover_skill_at_dir(&dir.path, plugin_id_maker(record));
            (skill.into_iter().collect(), w)
        } else {
            discover_skills_in_dir(&dir.path, dir.children, plugin_id_maker(record))
        };

        skills.extend(found);
        warnings.extend(found_warnings);
    }

    (skills, warnings)
}

fn plugin_id_maker(record: &PluginInstallRecord) -> impl Fn(String) -> SkillId {
    let marketplace = record.marketplace.clone();
    let plugin = record.plugin.clone();
    move |name| SkillId::Plugin {
        marketplace: marketplace.clone(),
        plugin: plugin.clone(),
        name,
    }
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

        let (records, warnings) = parse_installed_plugins(claude_home);

        assert_eq!(records.len(), 1);
        assert!(warnings.is_empty());
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

        let (records, warnings) = parse_installed_plugins(claude_home);
        assert_eq!(records.len(), 2);
        assert!(warnings.is_empty());
        assert!(records.iter().any(|r| r.scope == InstallScope::User));
        assert!(records.iter().any(|r| r.scope == InstallScope::Project));
    }

    #[test]
    fn missing_registry_file_yields_no_records_and_no_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let (records, warnings) = parse_installed_plugins(tmp.path());
        assert!(records.is_empty());
        assert!(warnings.is_empty(), "a missing registry file is the normal 'nothing installed' case");
    }

    #[test]
    fn corrupt_registry_file_yields_no_records_but_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        write_installed_plugins(claude_home, "not valid json");

        let (records, warnings) = parse_installed_plugins(claude_home);

        assert!(records.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("could not be parsed"));
    }

    fn write_skill(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: does {name}\n---\n\nBody.\n"),
        )
        .unwrap();
    }

    /// Writes the manifest where every plugin on disk actually keeps it. Tests
    /// build it through this helper rather than spelling the path inline, so
    /// they cannot re-encode the location bug they exist to catch (issue #33).
    fn write_manifest(install_path: &Path, body: &str) {
        let dir = install_path.join(".claude-plugin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("plugin.json"), body).unwrap();
    }

    fn record_for(install_path: &Path) -> PluginInstallRecord {
        PluginInstallRecord {
            plugin_at_marketplace: "test-plugin@test-market".to_string(),
            plugin: "test-plugin".to_string(),
            marketplace: "test-market".to_string(),
            scope: InstallScope::User,
            install_path: install_path.to_path_buf(),
        }
    }

    fn skill_names(skills: &[DiscoveredSkill]) -> Vec<String> {
        let mut names: Vec<String> = skills.iter().map(|s| s.directory_name().to_string()).collect();
        names.sort();
        names
    }

    /// The manifest lives at `.claude-plugin/plugin.json` -- 11 of 11 plugins on
    /// disk, and the only location the plugin reference documents. Reading
    /// `<installPath>/plugin.json` meant relocation never once fired, so
    /// `impeccable` -- which has no `skills/` at all -- resolved to zero.
    #[test]
    fn manifest_at_the_documented_path_relocates_the_skills_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        write_manifest(&install_path, r#"{"skills": "./.claude/skills"}"#);
        write_skill(&install_path.join(".claude").join("skills").join("reviewer"), "reviewer");

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty(), "{warnings:?}");
        match &skills[0].id {
            SkillId::Plugin { marketplace, plugin, name } => {
                assert_eq!(marketplace, "test-market");
                assert_eq!(plugin, "test-plugin");
                assert_eq!(name, "reviewer");
            }
            other => panic!("expected Plugin id, got {other:?}"),
        }
    }

    /// `skills` is `string|array`. An array made `from_str` fail for the *whole*
    /// struct, and `.ok()` swallowed it -- which is how `mattpocock-skills`
    /// yielded 0 of its 22 declared skills.
    #[test]
    fn manifest_skills_array_loads_every_declared_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        write_manifest(
            &install_path,
            r#"{"skills": ["./skills/engineering/tdd", "./skills/productivity/grilling"]}"#,
        );
        write_skill(&install_path.join("skills").join("engineering").join("tdd"), "tdd");
        write_skill(&install_path.join("skills").join("productivity").join("grilling"), "grilling");

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert_eq!(skill_names(&skills), vec!["grilling", "tdd"]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// A declared path naming a directory that holds `SKILL.md` *directly* is
    /// one skill, not a directory of them -- the documented `"skills": ["./"]`
    /// rule, and the shape every `mattpocock-skills` entry takes.
    #[test]
    fn declared_directory_holding_skill_md_directly_is_one_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        write_manifest(&install_path, r#"{"skills": ["./.claude/skills/ui-ux-pro-max"]}"#);
        let skill_dir = install_path.join(".claude").join("skills").join("ui-ux-pro-max");
        write_skill(&skill_dir, "ui-ux-pro-max");
        // Bundled references sit beside SKILL.md; they are this skill's own
        // on-demand layer, never sibling skills.
        fs::create_dir_all(skill_dir.join("data")).unwrap();
        fs::write(skill_dir.join("data").join("patterns.md"), "reference").unwrap();

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert_eq!(skill_names(&skills), vec!["ui-ux-pro-max"]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skills[0].on_demand_files.len(), 1);
    }

    /// `skills` is the one manifest field that *adds to* its default rather than
    /// replacing it: "The default `skills/` directory is always scanned, and
    /// directories listed in `skills` are loaded alongside it."
    #[test]
    fn declared_dirs_add_to_the_default_skills_scan_rather_than_replacing_it() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        write_manifest(&install_path, r#"{"skills": ["./extra"]}"#);
        write_skill(&install_path.join("skills").join("from-default"), "from-default");
        write_skill(&install_path.join("extra").join("from-declared"), "from-declared");

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert_eq!(skill_names(&skills), vec!["from-declared", "from-default"]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// The default scan and an explicit `"./skills"` name the same directory,
    /// and a skill counted twice is counted twice in the always-on headline.
    #[test]
    fn the_default_skills_dir_named_explicitly_is_not_scanned_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        write_manifest(&install_path, r#"{"skills": ["./skills"]}"#);
        write_skill(&install_path.join("skills").join("only-once"), "only-once");

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert_eq!(skill_names(&skills), vec!["only-once"]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// The scan under each candidate dir stays depth-1. `mattpocock-skills` has
    /// 40 `SKILL.md` on disk but declares 22; the undeclared 18 sit two levels
    /// down (`skills/deprecated/<name>/SKILL.md`) and never enter context. A
    /// recursive walk would trade a 40-skill under-count for an 18-skill
    /// over-count -- the same class of error as #26.
    #[test]
    fn undeclared_skills_below_a_category_dir_are_not_walked_into() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        write_manifest(&install_path, r#"{"skills": ["./skills/engineering/tdd"]}"#);
        write_skill(&install_path.join("skills").join("engineering").join("tdd"), "tdd");
        write_skill(&install_path.join("skills").join("deprecated").join("old"), "old");

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert_eq!(skill_names(&skills), vec!["tdd"], "an undeclared skill must not be reported");
        // The category dirs the default depth-1 scan lands on are not malformed
        // skills; warning about them is what misclassified the plugin.
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// The silent `.ok()` is what let all three defects hide. A manifest that
    /// exists but will not parse must say so -- the adapter already does this
    /// for a corrupt `installed_plugins.json`, and the same reasoning applies.
    #[test]
    fn unparsable_manifest_warns_rather_than_silently_falling_back() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        write_manifest(&install_path, "{ not valid json");
        write_skill(&install_path.join("skills").join("still-found"), "still-found");

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        // The default scan is not conditional on the manifest, so it still runs.
        assert_eq!(skill_names(&skills), vec!["still-found"]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("could not be parsed"), "{:?}", warnings[0]);
    }

    /// A `skills` entry of the wrong shape is a declaration skillmon cannot
    /// honor, not an absent one.
    #[test]
    fn manifest_with_a_wrongly_typed_skills_field_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        write_manifest(&install_path, r#"{"skills": 42}"#);

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("could not be parsed"), "{:?}", warnings[0]);
    }

    /// A declared path that is not on disk is an anomaly the manifest itself
    /// asserts, so it is warned about...
    #[test]
    fn declared_path_that_is_not_on_disk_is_warned() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        write_manifest(&install_path, r#"{"skills": ["./gone"]}"#);

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("declares a skills path"), "{:?}", warnings[0]);
    }

    /// ...whereas a missing default `skills/` is ordinary: 3 of 11 plugins on
    /// disk ship none, and nothing declared it.
    #[test]
    fn missing_default_skills_dir_is_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert!(skills.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// "A plugin that has a `SKILL.md` at its root, no `skills/` subdirectory,
    /// and no `skills` manifest field is automatically loaded as a single-skill
    /// plugin" (Claude Code v2.1.142+). Fires on none of the 11 plugins on this
    /// machine; without it, such a plugin would be invisible in exactly the way
    /// this issue is about.
    #[test]
    fn plugin_with_only_a_root_skill_md_is_loaded_as_a_single_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        write_skill(&install_path, "solo");
        write_manifest(&install_path, r#"{"name": "solo-plugin"}"#);

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skills[0].frontmatter.declared_name, "solo");
    }

    #[test]
    fn root_skill_md_is_ignored_when_the_plugin_ships_a_skills_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        write_skill(&install_path, "not-a-skill");
        write_skill(&install_path.join("skills").join("real"), "real");

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert_eq!(skill_names(&skills), vec!["real"]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn root_skill_md_is_ignored_when_the_manifest_declares_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        write_skill(&install_path, "not-a-skill");
        write_manifest(&install_path, r#"{"skills": ["./elsewhere/real"]}"#);
        write_skill(&install_path.join("elsewhere").join("real"), "real");

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert_eq!(skill_names(&skills), vec!["real"]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// The root fallback requires "no `skills` manifest field". A manifest that
    /// will not parse cannot establish that, so the fallback must not fire on
    /// it: unreadable is not the same as absent, and guessing here would invent
    /// a skill the plugin may not load.
    #[test]
    fn unparsable_manifest_does_not_trigger_the_root_single_skill_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        write_skill(&install_path, "unsure");
        write_manifest(&install_path, "{ not valid json");

        let (skills, warnings) = discover_plugin_skills(&record_for(&install_path));

        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("could not be parsed"), "{:?}", warnings[0]);
    }

    /// Issue #33's verification, against the real `~/.claude` -- because no
    /// tempdir fixture can settle what plugin authors actually ship, and it was
    /// a fixture (a `plugin.json` written where discovery looked for it, rather
    /// than where every plugin keeps it) that let the manifest-path bug pass a
    /// green suite.
    ///
    /// Asserts the properties rather than this machine's plugin versions, which
    /// change on every update:
    ///
    /// * no plugin resolves to a silent zero while its manifest declares skills
    ///   -- `mattpocock-skills` (22) and `impeccable` (18) both did, which is
    ///   the bug;
    /// * no `no readable SKILL.md` warning survives -- a category dir is a
    ///   nested layout, not a malformed skill.
    ///
    /// Read-only. `#[ignore]`d because it depends on this machine's `~`.
    ///
    /// Run by hand:
    /// `cargo test --manifest-path src-tauri/Cargo.toml
    /// adapters::claude_code::discovery::plugin::tests::real_claude_home_plugin_skills
    /// -- --ignored --exact --nocapture`
    #[test]
    #[ignore]
    fn real_claude_home_plugin_skills() {
        use crate::adapters::claude_code::paths::default_claude_home;

        let home = default_claude_home();
        let (records, warnings) = parse_installed_plugins(&home);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!records.is_empty(), "no plugins installed -- is this machine's ~/.claude populated?");

        let mut total = 0;
        for record in &records {
            let (skills, warnings) = discover_plugin_skills(record);
            let (manifest, _) = read_manifest_skills(&record.install_path);
            let declared = match &manifest {
                ManifestSkills::Declared(dirs) => dirs.len(),
                _ => 0,
            };
            total += skills.len();

            eprintln!(
                "{:<44} {:>3} skills  ({} declared){}",
                record.plugin_at_marketplace,
                skills.len(),
                if declared > 0 { declared.to_string() } else { "-".to_string() },
                if warnings.is_empty() { String::new() } else { format!("  warnings: {warnings:?}") },
            );

            assert!(
                !warnings.iter().any(|w| w.reason.contains("no readable SKILL.md")),
                "{} reports a malformed skill where it has a nested layout: {warnings:?}",
                record.plugin_at_marketplace,
            );
            if declared > 0 {
                assert!(
                    !skills.is_empty(),
                    "{} declares {declared} skills paths and resolved none -- the #33 silent zero",
                    record.plugin_at_marketplace,
                );
            }
        }
        eprintln!("\n=== {total} plugin skills across {} plugins ===\n", records.len());
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
