use crate::adapters::claude_code::frontmatter::parse_skill_md;
use crate::domain::skill::{DiscoveredSkill, DiscoveryWarning, SkillId};
use std::collections::HashSet;
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
        let symlink_target = if is_symlink { fs::read_link(&dir_path).ok() } else { None };

        let name = match dir_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let skill_md_path = dir_path.join("SKILL.md");
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
            is_symlink,
            symlink_target,
            on_demand_files,
            live: true,
        });
    }

    (skills, warnings)
}

/// Directory names whose contents are never bundled references. A skill
/// directory that is also a project checkout carries a VCS object store and a
/// dependency tree, and no `SKILL.md` body tells the agent to read either
/// (ADR 0028). A slice, not a fixed-size array: this list is expected to grow.
const NON_REFERENCE_DIR_NAMES: &[&str] = &[".git", "node_modules"];

fn list_on_demand_files(dir_path: &Path, skip: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut visited = HashSet::new();
    collect_files_recursive(dir_path, skip, &mut visited, &mut files);
    files
}

/// `visited` holds canonical directory paths, so a symlink pointing back at an
/// ancestor terminates the walk instead of recursing until the stack overflows,
/// and a directory reachable through two links is counted once rather than
/// twice. Canonicalizing per directory is the guard's whole cost, and it is
/// noise next to reading every file the walk yields.
fn collect_files_recursive(dir: &Path, skip: &Path, visited: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    // Bailing on a canonicalize failure is part of the guard, not just error
    // handling: resolving a symlink loop is exactly what fails here (ELOOP).
    let Ok(real_dir) = fs::canonicalize(dir) else { return };
    if !visited.insert(real_dir) {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else { return };
    // Sorted so `visited`'s alias tie-break is deterministic rather than left
    // to `read_dir` order: where a link and a real path reach the same content,
    // whichever is walked first wins and the other is skipped, which decides
    // the path recorded (and so the memo's signature). Real directories sort
    // before symlinked ones, making the real path the winner and the honest one
    // to record. Walk order alone is otherwise invisible -- `on_demand_signature`
    // sorts its tuples before hashing.
    let mut children: Vec<(bool, PathBuf)> = entries
        .flatten()
        .map(|entry| {
            let is_symlink = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            (is_symlink, entry.path())
        })
        .collect();
    children.sort();

    for (_, path) in children {
        if path == skip {
            continue;
        }
        if path.is_dir() {
            if is_reference_dir(&path) {
                collect_files_recursive(&path, skip, visited, out);
            }
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Whether a nested directory's contents can enter context through *this*
/// skill. A nested `SKILL.md` marks the subtree as another skill's: that
/// content reaches context (if at all) as that skill's own layers, loaded by
/// the skill mechanism, never because this skill's body said to read it
/// (ADR 0028). When the nested skill is also discovered in its own right --
/// gstack symlinks each of its 46 into `~/.claude/skills` -- counting it here
/// double-counts it outright.
fn is_reference_dir(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    !NON_REFERENCE_DIR_NAMES.contains(&name) && !path.join("SKILL.md").is_file()
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

    #[test]
    fn symlinked_skill_directory_records_target() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real-location");
        write_skill(tmp.path(), "real-location", "linked", "a linked skill", "Body.");
        let scan_root = tmp.path().join("scan-root");
        fs::create_dir_all(&scan_root).unwrap();
        symlink(&real_dir, scan_root.join("linked")).unwrap();

        let (skills, warnings) = discover_skills_in_dir(&scan_root, |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        assert!(skills[0].is_symlink);
        assert_eq!(skills[0].symlink_target.as_deref(), Some(real_dir.as_path()));
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
    fn vcs_and_dependency_dirs_are_excluded_from_on_demand_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "gstack", "gstack", "a skill that is also a checkout", "Body.");
        let skill_dir = tmp.path().join("gstack");
        fs::write(skill_dir.join("REFERENCE.md"), "a real reference").unwrap();

        for junk in [
            skill_dir.join(".git").join("objects").join("ab"),
            skill_dir.join("node_modules").join(".pnpm").join("left-pad@1.0.0"),
        ] {
            fs::create_dir_all(&junk).unwrap();
            fs::write(junk.join("blob"), "not a reference file").unwrap();
        }

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills[0].on_demand_files, vec![skill_dir.join("REFERENCE.md")]);
    }

    #[test]
    fn nested_skill_subtree_is_excluded_from_on_demand_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "gstack", "gstack", "ships nested skills", "Body.");
        let skill_dir = tmp.path().join("gstack");
        fs::write(skill_dir.join("REFERENCE.md"), "a real reference").unwrap();

        // A nested skill's own content is its own row's three layers; counting
        // it here too would double-count it (issue #26).
        write_skill(&skill_dir.join("skills"), "browse", "browse", "drives a browser", "Body.");
        fs::write(skill_dir.join("skills").join("browse").join("PLAYBOOK.md"), "nested ref").unwrap();

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills[0].on_demand_files, vec![skill_dir.join("REFERENCE.md")]);
    }

    #[test]
    fn files_beside_and_below_a_nested_skill_are_still_collected() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "gstack", "gstack", "ships nested skills", "Body.");
        let skill_dir = tmp.path().join("gstack");
        let skills_subdir = skill_dir.join("skills");
        write_skill(&skills_subdir, "browse", "browse", "drives a browser", "Body.");
        // Only the nested skill's own directory is pruned, not its parent --
        // an ordinary reference file sharing that parent still counts.
        fs::write(skills_subdir.join("INDEX.md"), "index of the nested skills").unwrap();

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills[0].on_demand_files, vec![skills_subdir.join("INDEX.md")]);
    }

    #[test]
    fn ordinary_nested_reference_dirs_are_still_collected() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "domain-modeling", "domain-modeling", "models domains", "Body.");
        let refs = tmp.path().join("domain-modeling").join("references");
        fs::create_dir_all(&refs).unwrap();
        fs::write(refs.join("ADR-FORMAT.md"), "adr doc").unwrap();

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills[0].on_demand_files, vec![refs.join("ADR-FORMAT.md")]);
    }

    #[test]
    fn symlinked_directory_loop_terminates() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "looping", "looping", "links to itself", "Body.");
        let refs = tmp.path().join("looping").join("references");
        fs::create_dir_all(&refs).unwrap();
        fs::write(refs.join("ADR-FORMAT.md"), "adr doc").unwrap();
        // The loop is kept clear of any SKILL.md so it exercises the cycle
        // guard rather than the nested-skill prune: without the guard `is_dir()`
        // follows the link and the walk recurses until the stack overflows
        // (issue #26).
        symlink(&refs, refs.join("self")).unwrap();

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills[0].on_demand_files, vec![refs.join("ADR-FORMAT.md")]);
    }

    #[test]
    fn directory_reached_twice_via_a_symlink_is_collected_once() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "linking", "linking", "links a sibling dir", "Body.");
        let skill_dir = tmp.path().join("linking");
        let refs = skill_dir.join("references");
        fs::create_dir_all(&refs).unwrap();
        fs::write(refs.join("ADR-FORMAT.md"), "adr doc").unwrap();
        symlink(&refs, skill_dir.join("also-references")).unwrap();

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills[0].on_demand_files, vec![refs.join("ADR-FORMAT.md")]);
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
