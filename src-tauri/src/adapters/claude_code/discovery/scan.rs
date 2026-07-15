use crate::adapters::claude_code::frontmatter::parse_skill_md;
use crate::domain::skill::{DiscoveredSkill, DiscoveryWarning, SkillId};
use std::fs;
use std::path::{Path, PathBuf};

pub fn discover_skills_in_dir(
    dir: &Path,
    make_id: impl Fn(String) -> SkillId,
) -> (Vec<DiscoveredSkill>, Vec<DiscoveryWarning>) {
    let mut skills = Vec::new();
    let mut warnings = Vec::new();

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return (skills, warnings),
    };

    // Canonicalized once, so an entry's resolved content can be compared
    // against where it would sit if it were unmanaged. Comparing two
    // canonical paths would be wrong (a symlinked entry canonicalizes to its
    // target, so it would always look equal to itself); comparing against the
    // raw `dir` would be wrong too, since a user whose `~/.claude` is itself a
    // symlink would see every skill resolve "elsewhere" and read as managed.
    let canonical_root = fs::canonicalize(dir).ok();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warnings.push(DiscoveryWarning {
                    path: dir.to_path_buf(),
                    reason: format!("error reading directory entry: {err}"),
                });
                continue;
            }
        };
        let dir_path = entry.path();

        let metadata = match fs::symlink_metadata(&dir_path) {
            Ok(m) => m,
            Err(err) => {
                warnings.push(DiscoveryWarning {
                    path: dir_path,
                    reason: format!("cannot read metadata: {err}"),
                });
                continue;
            }
        };
        let is_symlink = metadata.file_type().is_symlink();
        if !dir_path.is_dir() {
            if is_symlink {
                warnings.push(DiscoveryWarning {
                    path: dir_path,
                    reason: "symlink target is not a directory (broken or points at a non-directory)"
                        .to_string(),
                });
            }
            continue;
        }
        let name = match dir_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let skill_md_path = dir_path.join("SKILL.md");
        let manager_root = canonical_root
            .as_deref()
            .and_then(|root| resolve_manager_root(root, &name, &skill_md_path));
        let content = match fs::read_to_string(&skill_md_path) {
            Ok(c) => c,
            Err(_) => {
                warnings.push(DiscoveryWarning {
                    path: skill_md_path,
                    reason: "no readable SKILL.md".to_string(),
                });
                continue;
            }
        };

        let (frontmatter, body) = match parse_skill_md(&content) {
            Ok(parsed) => parsed,
            Err(e) => {
                warnings.push(DiscoveryWarning {
                    path: skill_md_path,
                    reason: format!("malformed frontmatter: {e}"),
                });
                continue;
            }
        };

        let on_demand_files = list_on_demand_files(&dir_path, &skill_md_path);

        skills.push(DiscoveredSkill {
            id: make_id(name),
            dir_path,
            skill_md_path,
            frontmatter,
            body,
            manager_root,
            on_demand_files,
            live: true,
        });
    }

    (skills, warnings)
}

/// The directory owning a skill's real content, or `None` when the skill owns
/// it itself (ADR 0026).
///
/// Resolving `SKILL.md` rather than the entry directory is what makes one rule
/// cover both shapes a managed skill takes, with no branch on where the link
/// sits: a symlinked directory (`tdd -> ~/.agents/skills/tdd`) and a real
/// directory holding a symlinked `SKILL.md` (gstack's shims) both resolve out
/// of the scan root. The previous check lstat'd only the directory and so
/// missed the second shape entirely -- 46 of 66 managed skills on a real
/// machine (issue #25).
///
/// `canonicalize` also resolves relative link bodies (`../../.agents/skills/tdd`)
/// against the link's own location, which `read_link` does not.
fn resolve_manager_root(canonical_root: &Path, name: &str, skill_md_path: &Path) -> Option<PathBuf> {
    let resolved_dir = fs::canonicalize(skill_md_path).ok()?.parent()?.to_path_buf();
    if resolved_dir == canonical_root.join(name) {
        return None;
    }
    Some(resolved_dir.parent()?.to_path_buf())
}

fn list_on_demand_files(dir_path: &Path, skip: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files_recursive(dir_path, skip, &mut files);
    files
}

fn collect_files_recursive(dir: &Path, skip: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip {
            continue;
        }
        if path.is_dir() {
            collect_files_recursive(&path, skip, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write_skill(root: &Path, dir_name: &str, name: &str, description: &str, body: &str) {
        let dir = root.join(dir_name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn discovers_well_formed_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "grilling", "grilling", "Interview relentlessly.", "Body.");

        let (skills, warnings) =
            discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        assert_eq!(skills[0].directory_name(), "grilling");
        assert_eq!(skills[0].frontmatter.description, "Interview relentlessly.");
        assert_eq!(skills[0].body, "Body.\n");
        assert!(skills[0].live);
    }

    #[test]
    fn missing_root_yields_no_skills_and_no_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");

        let (skills, warnings) = discover_skills_in_dir(&missing, |name| SkillId::Personal { name });

        assert!(skills.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn malformed_skill_is_skipped_and_warned_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "good", "good", "fine", "Body.");
        let bad_dir = tmp.path().join("bad");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("SKILL.md"), "not frontmatter at all").unwrap();

        let (skills, warnings) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].directory_name(), "good");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("malformed frontmatter"));
    }

    #[test]
    fn directory_with_no_skill_md_is_skipped_and_warned() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("empty-dir")).unwrap();

        let (skills, warnings) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("no readable SKILL.md"));
    }

    /// The `.agents` shape: the whole entry is a symlink into another tree.
    #[test]
    fn symlinked_skill_directory_reports_its_manager_root() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        write_skill(&store, "linked", "linked", "a linked skill", "Body.");
        let scan_root = tmp.path().join("scan-root");
        fs::create_dir_all(&scan_root).unwrap();
        symlink(store.join("linked"), scan_root.join("linked")).unwrap();

        let (skills, warnings) = discover_skills_in_dir(&scan_root, |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        assert_eq!(
            skills[0].manager_root.as_deref(),
            Some(fs::canonicalize(&store).unwrap().as_path())
        );
    }

    /// The gstack shim shape, and the whole point of issue #25: a *real* entry
    /// directory whose `SKILL.md` links into the managing tool's tree. The old
    /// dir-only lstat reported `is_symlink: false` here, missing 46 of 66
    /// managed skills on a real machine.
    #[test]
    fn real_directory_with_symlinked_skill_md_reports_its_manager_root() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("gstack");
        write_skill(&tool, "ship", "ship", "ships it", "Body.");
        let scan_root = tmp.path().join("scan-root");
        let shim = scan_root.join("ship");
        fs::create_dir_all(&shim).unwrap();
        symlink(tool.join("ship").join("SKILL.md"), shim.join("SKILL.md")).unwrap();

        let (skills, warnings) = discover_skills_in_dir(&scan_root, |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        assert_eq!(skills[0].directory_name(), "ship");
        assert_eq!(
            skills[0].manager_root.as_deref(),
            Some(fs::canonicalize(&tool).unwrap().as_path())
        );
    }

    /// The real `~/.agents` entries link relatively (`../../.agents/skills/tdd`),
    /// which `read_link` would hand back uninterpretable on its own.
    #[test]
    fn relative_symlink_resolves_against_the_entry_not_the_process_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        write_skill(&store, "tdd", "tdd", "test driven", "Body.");
        let scan_root = tmp.path().join("scan-root");
        fs::create_dir_all(&scan_root).unwrap();
        symlink(Path::new("../store/tdd"), scan_root.join("tdd")).unwrap();

        let (skills, warnings) = discover_skills_in_dir(&scan_root, |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        assert_eq!(
            skills[0].manager_root.as_deref(),
            Some(fs::canonicalize(&store).unwrap().as_path())
        );
    }

    #[test]
    fn skill_owning_its_own_content_is_unmanaged() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "mine", "mine", "hand installed", "Body.");

        let (skills, warnings) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        assert_eq!(skills[0].manager_root, None);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_entry_metadata_produces_warning() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let scan_root = tmp.path().join("scan-root");
        let child = scan_root.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("SKILL.md"), "---\nname: child\ndescription: d\n---\n\nBody.\n").unwrap();

        // Read permission lets read_dir list entry names; dropping execute
        // (search) permission on the parent blocks stat-by-path on those
        // entries, so read_dir succeeds but symlink_metadata fails per-entry.
        fs::set_permissions(&scan_root, fs::Permissions::from_mode(0o600)).unwrap();

        // Root (and some CI sandboxes) bypasses this permission check
        // entirely; skip rather than assert a false failure in that case.
        if fs::symlink_metadata(&child).is_ok() {
            fs::set_permissions(&scan_root, fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!(
                "skipping unreadable_entry_metadata_produces_warning: \
                 metadata read succeeded despite missing execute bit (likely running as root)"
            );
            return;
        }

        let (skills, warnings) =
            discover_skills_in_dir(&scan_root, |name| SkillId::Personal { name });

        fs::set_permissions(&scan_root, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("cannot read metadata"));
    }

    #[test]
    fn dangling_symlink_produces_warning_and_is_not_a_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let scan_root = tmp.path().join("scan-root");
        fs::create_dir_all(&scan_root).unwrap();
        symlink("/nonexistent/target", scan_root.join("dangling")).unwrap();

        let (skills, warnings) = discover_skills_in_dir(&scan_root, |name| SkillId::Personal { name });

        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("symlink target is not a directory"));
    }

    #[test]
    fn ordinary_non_directory_entry_is_silently_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        // A stray non-symlink file at depth-1 (e.g. .DS_Store) is not a skill dir
        // but is not anomalous either - it must not produce a warning.
        fs::write(tmp.path().join(".DS_Store"), b"junk").unwrap();

        let (skills, warnings) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert!(skills.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn bundled_reference_files_are_collected_as_on_demand() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "domain-modeling", "domain-modeling", "models domains", "Body.");
        fs::write(tmp.path().join("domain-modeling").join("CONTEXT-FORMAT.md"), "format doc").unwrap();
        fs::write(tmp.path().join("domain-modeling").join("ADR-FORMAT.md"), "adr doc").unwrap();

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].on_demand_files.len(), 2);
    }

    #[test]
    fn make_id_closure_receives_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "foo", "foo", "desc", "Body.");

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Plugin {
            marketplace: "test-market".to_string(),
            plugin: "test-plugin".to_string(),
            name,
        });

        assert_eq!(
            skills[0].id,
            SkillId::Plugin {
                marketplace: "test-market".to_string(),
                plugin: "test-plugin".to_string(),
                name: "foo".to_string(),
            }
        );
    }
}
