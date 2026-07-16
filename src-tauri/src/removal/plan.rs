//! Deciding *what* a removal removes (ADR 0027), before anything moves.
//!
//! The other half of `removal`'s seam. This module resolves symlinks, asks
//! managing tools what they can make stick, and works out which dependents
//! cascade; `removal::remove` then moves what it was handed and knows none of
//! that. The split is what keeps the trash engine tool-agnostic while the
//! decisions stay in one readable place.
//!
//! Harness-neutral all the same: the caller supplies the skills and the storage
//! root, and no Claude Code path is named here.

use std::path::Path;

use crate::domain::removal::{EntryToRemove, RemovalPlan, SourceOffer, SourceToRemove};
use crate::domain::skill::{dependents_of, DiscoveredSkill, SkillId};
use crate::managing_tool::{ManagingTool, ManagingTools};

use super::RemovalError;

/// Works out what removing `skill` entails: what cascades, what a managing tool
/// would put back, and whether its content can be removed too.
///
/// Reads the filesystem (a source is where `SKILL.md` actually resolves) but
/// writes nothing -- the plan is rendered to the user and only then executed, so
/// it must be safe to compute on a dialog open.
pub fn plan(skills: &[DiscoveredSkill], tools: &ManagingTools, skill: &DiscoveredSkill) -> RemovalPlan {
    let manager_root = skill.manager_root.as_deref();
    let tool = manager_root.and_then(|root| tools.for_root(root));

    RemovalPlan {
        primary: entry_for(skill),
        dependents: dependents_of(skills, skill).into_iter().map(entry_for).collect(),
        source: manager_root.map(|root| source_offer(skill, root, tool)),
        // A managed entry is one its manager rebuilds -- known tool or not. The
        // hazard is ADR 0027's, and it does not depend on skillmon recognizing
        // who will do the rebuilding.
        rebuilt_by: manager_root.map(Path::to_path_buf),
    }
}

/// What removing the manager's own copy would take, or why it is not on offer.
///
/// Every path here is refused rather than guessed: a tool that says it cannot
/// make the removal stick is taken at its word, an unknown manager gets no
/// offer, and a source that will not resolve is not invented.
///
/// `manager_root` is passed rather than re-read off the skill, so "an offer
/// exists only for a managed skill" is a fact about the signature instead of an
/// invariant this function would have to paper over with an empty path -- and an
/// empty path would reach the dialog as a blank "content lives at" line.
fn source_offer(
    skill: &DiscoveredSkill,
    manager_root: &Path,
    tool: Option<&(dyn ManagingTool + Send + Sync)>,
) -> SourceOffer {
    let Some(tool) = tool else {
        return SourceOffer {
            // The manager root is the honest fallback: skillmon can see where
            // the content lives without knowing who put it there.
            path: manager_root.to_path_buf(),
            tool_name: None,
            blocked: Some(
                "skillmon does not recognize the tool managing this skill, so it cannot tell whether \
                 removing its copy would stick. Removing the entry is always safe."
                    .to_string(),
            ),
        };
    };

    let path = tool.source_of(skill);
    let blocked = tool.can_remove_source().map(str::to_string).or_else(|| {
        path.is_none().then(|| {
            // Capable in principle, but its content cannot be located right now
            // -- a broken link, or a tool mid-rebuild. Refusing is the only
            // option: there is no path to stage.
            format!("skillmon cannot resolve where {} keeps this skill's content right now.", tool.name())
        })
    });

    SourceOffer {
        // Falls back to the manager root only on the branch just blocked above,
        // so the dialog still names a real directory while refusing.
        path: path.unwrap_or_else(|| manager_root.to_path_buf()),
        tool_name: Some(tool.name().to_string()),
        blocked,
    }
}

fn entry_for(skill: &DiscoveredSkill) -> EntryToRemove {
    EntryToRemove {
        skill_id: skill.id.clone(),
        declared_name: skill.frontmatter.declared_name.clone(),
        entry_path: skill.dir_path.clone(),
        // Filled in by `take_source` on the user's explicit opt-in, never here:
        // computing a plan must not decide to delete anything.
        source: None,
    }
}

/// Takes the user up on ADR 0027's second opt-in: drops the tool's bookkeeping
/// and hands back the plan with the source attached, ready to stage.
///
/// The bookkeeping is dropped *before* the move rather than after, and that
/// ordering is deliberate. `forget_source` is the step that can refuse -- a lock
/// in a version skillmon does not understand -- and it must refuse while nothing
/// has moved yet. The reverse order would leave a staged source whose tool still
/// advertises it, which is the desync ADR 0027 rejected.
///
/// Only ever applies to the primary. A cascade is a tool uninstall, and taking
/// 46 dependents' sources would mean reaching into 46 other managers.
pub fn take_source(plan: &mut RemovalPlan, skill: &DiscoveredSkill, tools: &ManagingTools) -> Result<(), RemovalError> {
    let Some(offer) = plan.source.as_ref() else {
        return Err(RemovalError::SourceUnavailable {
            name: plan.primary.declared_name.clone(),
            reason: "this skill's content is its own entry, so there is no separate source to remove".to_string(),
        });
    };
    if let Some(reason) = offer.blocked.as_deref() {
        return Err(RemovalError::SourceUnavailable {
            name: plan.primary.declared_name.clone(),
            reason: reason.to_string(),
        });
    }

    let path = offer.path.clone();
    let root = skill.manager_root.as_deref().expect("an offer exists only for a managed skill");
    let tool = tools.for_root(root).expect("an unblocked offer exists only where a tool was found");
    let state = tool.forget_source(skill).map_err(|e| RemovalError::SourceUnavailable {
        name: plan.primary.declared_name.clone(),
        reason: e.to_string(),
    })?;

    plan.primary.source = Some(SourceToRemove { path, state });
    Ok(())
}

/// Undoes `take_source`'s bookkeeping when the removal it was taken for did not
/// happen.
///
/// Without this, a refused removal leaves the machine worse than it found it:
/// the tool has forgotten a skill that is still installed, so its next run would
/// not maintain it and skillmon achieved nothing. The files never moved, so
/// there is nothing else to unwind.
///
/// Best-effort and logged, like the move rollbacks: this runs while an error is
/// already on its way to the user, and there is nothing useful to return a
/// second one to.
pub fn untake_source(entry: &EntryToRemove, skill: &DiscoveredSkill, tools: &ManagingTools) {
    let Some(source) = entry.source.as_ref() else { return };
    let Some(state) = source.state.as_deref() else { return };
    let Some(tool) = skill.manager_root.as_deref().and_then(|root| tools.for_root(root)) else { return };

    if let Err(e) = tool.relearn_source(state) {
        eprintln!(
            "[skillmon] {} was not removed, but {} had already been told to forget it and could not be told again: {e}",
            entry.declared_name,
            tool.name()
        );
    }
}

/// Finds the row a panel's `SkillRef` named, in a **fresh** scan.
///
/// The whole TOCTOU story, and deliberately a narrow one: a ref names a row, and
/// resolving it here means the removal acts on a `DiscoveredSkill` this scan
/// just saw rather than on a path the panel remembered. It does not close the
/// window -- nothing can, the filesystem moves underneath either -- but a stale
/// ref now *fails* instead of aiming a delete at whatever took its place.
pub fn resolve<'a>(skills: &'a [DiscoveredSkill], id: &SkillId) -> Result<&'a DiscoveredSkill, RemovalError> {
    skills
        .iter()
        .find(|s| &s.id == id)
        .ok_or_else(|| RemovalError::UnknownSkill { name: id.name().to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::skill::{Frontmatter, SkillId};
    use crate::managing_tool::{gstack::GstackTool, SourceError};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::{tempdir, TempDir};

    /// A tool that owns one root and can remove sources -- the `.agents` answer,
    /// without a lock file to keep. What is under test here is the plan, not any
    /// tool's bookkeeping.
    struct CapableTool {
        root: PathBuf,
        forgotten: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ManagingTool for CapableTool {
        fn name(&self) -> &'static str {
            "capable-tool"
        }
        fn detects(&self, root: &Path) -> bool {
            root == self.root
        }
        fn can_remove_source(&self) -> Option<&str> {
            None
        }
        fn forget_source(&self, skill: &DiscoveredSkill) -> Result<Option<String>, SourceError> {
            self.forgotten.lock().unwrap().push(skill.directory_name().to_string());
            Ok(Some(format!("state for {}", skill.directory_name())))
        }
        fn relearn_source(&self, _state: &str) -> Result<(), SourceError> {
            Ok(())
        }
    }

    struct Fixture {
        tmp: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture { tmp: tempdir().unwrap() }
        }

        fn skills_dir(&self) -> PathBuf {
            self.tmp.path().join("skills")
        }

        /// An ordinary skill: real directory, real `SKILL.md`, nobody managing it.
        fn unmanaged(&self, name: &str) -> DiscoveredSkill {
            let dir = self.skills_dir().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("SKILL.md"), "body").unwrap();
            skill(name, &dir, fs::canonicalize(&dir).unwrap(), None)
        }

        /// The `.agents` shape: the entry is a symlink into the tool's tree.
        #[cfg(unix)]
        fn linked(&self, name: &str, tool_dir: &str) -> DiscoveredSkill {
            let content = self.tmp.path().join(tool_dir).join(name);
            fs::create_dir_all(&content).unwrap();
            fs::write(content.join("SKILL.md"), "body").unwrap();
            let entry = self.skills_dir().join(name);
            fs::create_dir_all(self.skills_dir()).unwrap();
            std::os::unix::fs::symlink(&content, &entry).unwrap();

            let manager_root = fs::canonicalize(self.tmp.path().join(tool_dir)).unwrap();
            skill(name, &entry, fs::canonicalize(&content).unwrap(), Some(manager_root))
        }

        fn tool_root(&self, dir: &str) -> PathBuf {
            fs::canonicalize(self.tmp.path().join(dir)).unwrap()
        }
    }

    fn skill(name: &str, dir: &Path, canonical: PathBuf, manager_root: Option<PathBuf>) -> DiscoveredSkill {
        DiscoveredSkill {
            id: SkillId::Personal { name: name.to_string() },
            dir_path: dir.to_path_buf(),
            canonical_dir: canonical,
            skill_md_path: dir.join("SKILL.md"),
            frontmatter: Frontmatter {
                declared_name: name.to_string(),
                description: "d".to_string(),
                raw_block: "name: x".to_string(),
                model_invocable: true,
            },
            body: "body".to_string(),
            manager_root,
            on_demand_files: vec![],
            live: true,
        }
    }

    fn no_tools() -> ManagingTools {
        ManagingTools::new(vec![])
    }

    /// The plain case ADR 0027 allows a plain delete for: nothing manages it,
    /// nothing resolves into it.
    #[test]
    fn an_unmanaged_skill_with_no_dependents_is_a_plain_skill_removal() {
        let f = Fixture::new();
        let skills = vec![f.unmanaged("vercel-react")];

        let plan = plan(&skills, &no_tools(), &skills[0]);

        assert!(!plan.is_tool_uninstall());
        assert_eq!(plan.dependents.len(), 0);
        assert_eq!(plan.primary.entry_path, f.skills_dir().join("vercel-react"));
        assert!(plan.source.is_none(), "its content IS its entry; there is no second thing to offer");
        assert!(plan.rebuilt_by.is_none(), "nothing will put it back");
    }

    /// gstack's shape, and the reason the classification exists: the row is
    /// unmanaged -- which alone reads as "safe to delete" -- while being the one
    /// entry 46 skills resolve into.
    #[cfg(unix)]
    #[test]
    fn removing_a_row_others_resolve_into_is_a_tool_uninstall_that_cascades() {
        let f = Fixture::new();
        let mut skills = vec![f.unmanaged("gstack")];
        // Their content lives inside the gstack entry, which is what makes them
        // dependents.
        for i in 0..3 {
            let name = format!("shim-{i}");
            let content = f.skills_dir().join("gstack").join(&name);
            fs::create_dir_all(&content).unwrap();
            fs::write(content.join("SKILL.md"), "body").unwrap();
            let entry = f.skills_dir().join(&name);
            fs::create_dir_all(&entry).unwrap();
            std::os::unix::fs::symlink(content.join("SKILL.md"), entry.join("SKILL.md")).unwrap();
            skills.push(skill(
                &name,
                &entry,
                fs::canonicalize(&entry).unwrap(),
                Some(fs::canonicalize(f.skills_dir().join("gstack")).unwrap()),
            ));
        }

        let plan = plan(&skills, &no_tools(), &skills[0]);

        assert!(plan.is_tool_uninstall(), "a row with dependents is a tool uninstall, not a skill removal");
        assert_eq!(plan.dependents.len(), 3);
        let cascaded: Vec<&str> = plan.dependents.iter().map(|d| d.skill_id.name()).collect();
        assert_eq!(cascaded, vec!["shim-0", "shim-1", "shim-2"]);
    }

    /// The dependents' own removals stay ordinary: a shim is nobody's manager.
    #[cfg(unix)]
    #[test]
    fn removing_a_dependent_does_not_cascade_to_its_siblings() {
        let f = Fixture::new();
        let skills = vec![f.linked("tdd", "agents/skills"), f.linked("grilling", "agents/skills")];

        let plan = plan(&skills, &no_tools(), &skills[0]);

        assert!(!plan.is_tool_uninstall());
        assert_eq!(plan.dependents.len(), 0, "a sibling under the same manager is not a dependent");
    }

    /// ADR 0027's recorded hazard, surfaced before the removal rather than
    /// discovered after it: a managed entry is one its manager will rebuild.
    #[cfg(unix)]
    #[test]
    fn a_managed_entry_warns_that_its_manager_will_rebuild_it() {
        let f = Fixture::new();
        let skills = vec![f.linked("tdd", "agents/skills")];

        let plan = plan(&skills, &no_tools(), &skills[0]);

        assert_eq!(plan.rebuilt_by, Some(f.tool_root("agents/skills")));
    }

    /// A capable tool: the offer is live, and it names the path outside
    /// `~/.claude` that removing it would reach.
    #[cfg(unix)]
    #[test]
    fn a_capable_tool_offers_its_source_and_names_the_path() {
        let f = Fixture::new();
        let skills = vec![f.linked("tdd", "agents/skills")];
        let tools = ManagingTools::new(vec![Box::new(CapableTool {
            root: f.tool_root("agents/skills"),
            forgotten: Default::default(),
        })]);

        let plan = plan(&skills, &tools, &skills[0]);
        let offer = plan.source.expect("a managed skill has a source");

        assert!(offer.blocked.is_none());
        assert_eq!(offer.tool_name.as_deref(), Some("capable-tool"));
        assert_eq!(offer.path, fs::canonicalize(f.tmp.path().join("agents/skills/tdd")).unwrap());
    }

    /// gstack's answer, carried verbatim to the dialog: the option is absent and
    /// says why, because a missing option that does not explain itself reads as
    /// a bug.
    #[cfg(unix)]
    #[test]
    fn an_incapable_tool_blocks_its_source_offer_with_the_reason_it_gave() {
        let f = Fixture::new();
        // A checkout carrying gstack's marker trio, so the real tool detects it.
        for marker in ["setup", "VERSION", "SKILL.md"] {
            fs::create_dir_all(f.tmp.path().join("gstack")).unwrap();
            fs::write(f.tmp.path().join("gstack").join(marker), "x").unwrap();
        }
        let skills = vec![f.linked("ship", "gstack")];
        let tools = ManagingTools::new(vec![Box::new(GstackTool)]);

        let plan = plan(&skills, &tools, &skills[0]);
        let offer = plan.source.expect("a managed skill has a source");

        assert!(offer.blocked.is_some());
        assert!(offer.blocked.unwrap().contains("reset --hard"), "the refusal must carry the tool's own reason");
        assert_eq!(offer.tool_name.as_deref(), Some("gstack"));
    }

    /// An unknown manager is entry-only, honestly labeled -- never silently
    /// offered a removal skillmon cannot vouch for.
    #[cfg(unix)]
    #[test]
    fn an_unknown_manager_blocks_the_source_offer_and_says_so() {
        let f = Fixture::new();
        let skills = vec![f.linked("mystery", "some-tool/skills")];

        let plan = plan(&skills, &no_tools(), &skills[0]);
        let offer = plan.source.expect("a managed skill has a source");

        assert!(offer.blocked.is_some());
        assert_eq!(offer.tool_name, None);
        assert!(offer.blocked.unwrap().contains("does not recognize"));
    }

    /// Taking the opt-in attaches the source AND drops the tool's bookkeeping --
    /// the ordering that matters, since forgetting is the step that can refuse.
    #[cfg(unix)]
    #[test]
    fn taking_a_source_attaches_it_and_forgets_it_in_the_tool() {
        let f = Fixture::new();
        let skills = vec![f.linked("tdd", "agents/skills")];
        let forgotten: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let tools = ManagingTools::new(vec![Box::new(CapableTool {
            root: f.tool_root("agents/skills"),
            forgotten: forgotten.clone(),
        })]);
        let mut plan = plan(&skills, &tools, &skills[0]);

        take_source(&mut plan, &skills[0], &tools).unwrap();

        let source = plan.primary.source.expect("the source is attached, ready to stage");
        assert_eq!(source.path, fs::canonicalize(f.tmp.path().join("agents/skills/tdd")).unwrap());
        assert_eq!(source.state.as_deref(), Some("state for tdd"), "the tool's state is captured for the undo");
        assert_eq!(*forgotten.lock().unwrap(), vec!["tdd"], "and the tool was told to forget it");
    }

    /// A tool that refused is not talked round: the plan cannot be made to take
    /// a source the tool said it cannot make stick.
    #[cfg(unix)]
    #[test]
    fn taking_a_blocked_source_is_refused_with_its_reason() {
        let f = Fixture::new();
        fs::create_dir_all(f.tmp.path().join("gstack")).unwrap();
        for marker in ["setup", "VERSION", "SKILL.md"] {
            fs::write(f.tmp.path().join("gstack").join(marker), "x").unwrap();
        }
        let skills = vec![f.linked("ship", "gstack")];
        let tools = ManagingTools::new(vec![Box::new(GstackTool)]);
        let mut plan = plan(&skills, &tools, &skills[0]);

        let err = take_source(&mut plan, &skills[0], &tools).unwrap_err();

        assert!(matches!(err, RemovalError::SourceUnavailable { .. }), "got {err:?}");
        assert!(plan.primary.source.is_none(), "nothing was attached");
    }

    /// An unmanaged skill has no source to take: its entry is the content, and
    /// ADR 0027's rule is that the entry is what gets removed.
    #[test]
    fn taking_a_source_from_an_unmanaged_skill_is_refused() {
        let f = Fixture::new();
        let skills = vec![f.unmanaged("vercel-react")];
        let mut plan = plan(&skills, &no_tools(), &skills[0]);

        assert!(matches!(
            take_source(&mut plan, &skills[0], &no_tools()),
            Err(RemovalError::SourceUnavailable { .. })
        ));
    }

    /// CLAUDE.md's verification bar for issue #31: exercise the whole flow --
    /// plan, take the source, remove, restore -- against this machine's **real**
    /// `~/.claude` and the **real** `.agents` lock, rather than only over
    /// synthetic fixtures. Mutations must round-trip (disable -> enable,
    /// uninstall -> restore).
    ///
    /// Nothing live is mutated, and that is not timidity. Removal is destructive
    /// and `~/.claude/skills` is the user's only copy, so a test that removed
    /// from it would be the exact overreach this module exists to prevent.
    /// Instead: a real skill's **real content** is replicated into a tempdir laid
    /// out as `.agents` lays one out, and the user's **real** `.skill-lock.json`
    /// is copied beside it -- so the lock parser meets the actual file, with its
    /// actual version and its actual sibling keys, and the round trip is asserted
    /// byte-for-byte against it.
    ///
    /// Run by hand:
    /// `cargo test --manifest-path src-tauri/Cargo.toml
    /// removal::plan::tests::real_claude_home_plan_remove_and_restore -- --ignored --exact --nocapture`
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn real_claude_home_plan_remove_and_restore() {
        use crate::adapters::claude_code::paths::default_claude_home;
        use crate::adapters::claude_code::ClaudeCodeAdapter;
        use crate::domain::removal::Retention;
        use crate::managing_tool::agents::AgentsTool;
        use crate::removal::store::TrashStore;

        fn copy_tree(from: &Path, to: &Path) {
            let meta = fs::symlink_metadata(from).unwrap();
            if meta.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(from).unwrap(), to).unwrap();
            } else if meta.is_dir() {
                fs::create_dir_all(to).unwrap();
                for child in fs::read_dir(from).unwrap() {
                    let child = child.unwrap();
                    copy_tree(&child.path(), &to.join(child.file_name()));
                }
            } else {
                fs::copy(from, to).unwrap();
            }
        }

        let real_home = default_claude_home();
        let discovery = ClaudeCodeAdapter::for_discovery_only(real_home.clone()).discover_skills();
        assert!(!discovery.skills.is_empty(), "no skills discovered -- is this machine's ~/.claude populated?");

        // Every real row plans without panicking, and every classification is
        // self-consistent. This is the pass that would catch a real entry shape
        // the synthetic fixtures do not have.
        let real_tools = ManagingTools::from_env();
        for skill in &discovery.skills {
            let p = plan(&discovery.skills, &real_tools, skill);
            assert_eq!(p.is_tool_uninstall(), !p.dependents.is_empty());
            assert_eq!(p.source.is_some(), skill.manager_root.is_some(), "an offer exists iff the skill is managed");
            assert_eq!(p.rebuilt_by.is_some(), skill.manager_root.is_some());
            eprintln!(
                "  {:<28} managed={:<5} dependents={:<3} source={}",
                skill.directory_name(),
                skill.manager_root.is_some(),
                p.dependents.len(),
                match p.source.as_ref() {
                    None => "n/a (its entry is its content)".to_string(),
                    Some(s) if s.blocked.is_none() => format!("removable via {}", s.tool_name.as_deref().unwrap_or("?")),
                    Some(s) => format!("blocked: {}", s.blocked.as_deref().unwrap_or("?").split('.').next().unwrap()),
                }
            );
        }
        eprintln!("\n=== {} real skills planned ===", discovery.skills.len());

        // The `.agents` round trip, on real content and the real lock format.
        let tmp = tempdir().unwrap();
        let scan_root = tmp.path().join("skills");
        let agents_skills = tmp.path().join(".agents/skills");
        let lock_path = tmp.path().join(".agents/.skill-lock.json");
        fs::create_dir_all(&scan_root).unwrap();
        fs::create_dir_all(&agents_skills).unwrap();

        let real_lock = dirs::home_dir().unwrap().join(".agents/.skill-lock.json");
        let had_real_lock = real_lock.is_file();
        if had_real_lock {
            // The user's actual file, copied -- never edited in place.
            fs::copy(&real_lock, &lock_path).unwrap();
            eprintln!("=== using the real ~/.agents/.skill-lock.json ({} bytes) ===", fs::metadata(&real_lock).unwrap().len());
        } else {
            fs::write(&lock_path, r#"{"version":3,"skills":{},"dismissed":{}}"#).unwrap();
            eprintln!("=== no real lock on this machine; using a minimal v3 one ===");
        }

        // A real skill's real content, installed the way `.agents` installs one.
        let sample = &discovery.skills[0];
        let name = sample.directory_name().to_string();
        let content = agents_skills.join(&name);
        copy_tree(&sample.dir_path, &content);
        let entry = scan_root.join(&name);
        std::os::unix::fs::symlink(&content, &entry).unwrap();

        // Teach the lock about it, through the tool's own writer, so the entry
        // under test is shaped exactly as the CLI would have shaped it.
        let tool = AgentsTool::new(agents_skills.clone(), lock_path.clone());
        tool.relearn_source(&format!(
            r#"{{"key":"{name}","entry":{{"source":"mattpocock/skills","sourceType":"github","skillFolderHash":"deadbeef"}}}}"#
        ))
        .unwrap();
        let lock_with_skill = fs::read_to_string(&lock_path).unwrap();

        let manager_root = fs::canonicalize(&agents_skills).unwrap();
        let discovered = skill(&name, &entry, fs::canonicalize(&content).unwrap(), Some(manager_root));
        let skills = vec![discovered];
        let tools = ManagingTools::new(vec![Box::new(AgentsTool::new(agents_skills, lock_path.clone()))]);

        let mut p = plan(&skills, &tools, &skills[0]);
        assert!(p.source.as_ref().unwrap().blocked.is_none(), ".agents must be able to remove its own copy");
        take_source(&mut p, &skills[0], &tools).unwrap();
        assert!(
            !fs::read_to_string(&lock_path).unwrap().contains(&format!("\"{name}\"")),
            "the lock entry was pruned"
        );

        let mut store = TrashStore::open_in_memory().unwrap();
        let storage_root = tmp.path().join("skillmon/removed");
        let id = super::super::remove(&mut store, &storage_root, 1_000, Retention::Trashed, p.primary, p.dependents)
            .unwrap();

        assert!(!fs::symlink_metadata(&entry).is_ok(), "the entry left the scan root");
        assert!(!content.exists(), "and so did the tool's copy, since that was asked for");
        let unit = store.get(id).unwrap().unwrap();
        eprintln!("=== staged {} + its .agents content: {} bytes ===", name, unit.bytes());
        assert!(unit.bytes() > 0);

        super::super::restore(&mut store, &tools, id).unwrap();

        assert!(fs::symlink_metadata(&entry).unwrap().is_symlink(), "the entry is a link again");
        assert!(entry.join("SKILL.md").exists(), "and it resolves -- not a dangling shim");
        assert_eq!(
            fs::read_to_string(entry.join("SKILL.md")).unwrap(),
            fs::read_to_string(sample.dir_path.join("SKILL.md")).unwrap(),
            "the restored content is the real skill's, byte for byte"
        );
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap(),
            lock_with_skill,
            "the real lock round-tripped exactly -- ADR 0027 rejected desyncing it"
        );
        assert!(
            sample.dir_path.join("SKILL.md").exists(),
            "the real skill this fixture was copied from must be untouched"
        );
        eprintln!("=== round trip on real content + the real lock format: intact ===\n");
    }

    #[test]
    fn resolving_finds_the_row_a_ref_names() {
        let f = Fixture::new();
        let skills = vec![f.unmanaged("a"), f.unmanaged("b")];

        let found = resolve(&skills, &SkillId::Personal { name: "b".to_string() }).unwrap();
        assert_eq!(found.directory_name(), "b");
    }

    /// The point of resolving at all: a ref the panel held onto after the skill
    /// went away aims no delete, rather than aiming one at whatever is there now.
    #[test]
    fn resolving_a_ref_whose_skill_is_gone_fails_rather_than_guessing() {
        let f = Fixture::new();
        let skills = vec![f.unmanaged("a")];

        let err = resolve(&skills, &SkillId::Personal { name: "since-uninstalled".to_string() }).unwrap_err();
        assert!(matches!(err, RemovalError::UnknownSkill { .. }), "got {err:?}");
    }
}
