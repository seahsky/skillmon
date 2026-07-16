use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SkillId {
    Personal { name: String },
    Project { repo_path: PathBuf, name: String },
    Plugin { marketplace: String, plugin: String, name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallScope {
    User,
    Project,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub declared_name: String,
    pub description: String,
    pub raw_block: String,
    /// `false` when the skill declares `disable-model-invocation: true`, which
    /// keeps Claude Code from listing it to the model at all -- it stays
    /// slash-invokable, but costs no always-on tokens (issue #24). Defaults to
    /// `true`: absence of the key means an ordinary, listed skill.
    pub model_invocable: bool,
}

#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    pub id: SkillId,
    pub dir_path: PathBuf,
    /// `dir_path` with every symlink resolved -- the same directory, named the
    /// one way that compares against a `manager_root`, which is canonical for
    /// the same reason (`discovery::scan`). Both sides of the dependent test
    /// have to be canonical or it silently answers zero: `~/.claude` is itself a
    /// symlink on a dotfiles setup, and for an entry that *is* a link this is
    /// the directory it names, which is the whole point of the comparison.
    ///
    /// Falls back to `dir_path` when the path cannot be resolved, which discovery
    /// warns about: it names no other directory, so the row matches nothing and
    /// reports no dependents.
    pub canonical_dir: PathBuf,
    /// Populated by discovery; read by the reversible mutation ops (ADR 0007)
    /// in a later plan, not by discovery or footprint.
    #[allow(dead_code)]
    pub skill_md_path: PathBuf,
    pub frontmatter: Frontmatter,
    pub body: String,
    /// The directory owning this skill's real content when that content does
    /// not live in the skill's own entry under the scan root; `None` means
    /// unmanaged (ADR 0026). Derived structurally, from where `SKILL.md`
    /// actually resolves -- which covers both shapes a managed skill takes: a
    /// symlinked directory, and a real directory holding a symlinked
    /// `SKILL.md`. Crosses to the panel on `SkillReport` (issue #27) for the
    /// manager-root column (issue #30), and is read by removal, which must know
    /// whether deleting an entry is durable (issue #31, ADR 0027).
    pub manager_root: Option<PathBuf>,
    pub on_demand_files: Vec<PathBuf>,
    pub live: bool,
}

impl SkillId {
    /// The directory name every variant carries, so reading it costs callers no
    /// match on the kind.
    pub fn name(&self) -> &str {
        match self {
            SkillId::Personal { name } => name,
            SkillId::Project { name, .. } => name,
            SkillId::Plugin { name, .. } => name,
        }
    }
}

impl DiscoveredSkill {
    pub fn directory_name(&self) -> &str {
        self.id.name()
    }

    /// Crosses to the panel as `SkillReport::name_mismatch` (issue #27), which
    /// shows both names rather than silently picking one (CONTEXT.md "Declared
    /// name"). The rule for what counts as divergence stays here, in the
    /// domain, rather than being re-derived in the panel.
    pub fn name_mismatch(&self) -> bool {
        self.directory_name() != self.frontmatter.declared_name
    }
}

/// How many discovered skills resolve into each skill's directory (CONTEXT.md
/// "Dependent skill"), built once per scan and read per row.
///
/// The count is the second half of what ADR 0026 refuses to collapse into one
/// field. `manager_root: None` alone reads as "safe to remove," and on the
/// single most destructive entry on a real machine -- the 1.1 GB checkout 46
/// shims resolve into -- that reading is exactly backwards. Removing an entry
/// with dependents is a tool uninstall, not a skill removal (ADR 0027).
///
/// **The count is a floor, never a total.** It counts *discovered* skills, and
/// skillmon scans Claude Code's paths alone; a managing tool that also installs
/// into other agents' directories has dependents skillmon cannot see. Nothing
/// rendered from this may claim it is exhaustive.
#[derive(Debug, Clone, Default)]
pub struct DependentIndex {
    counts: HashMap<SkillId, u32>,
}

impl DependentIndex {
    /// Skills whose manager root lies at or under another skill's directory.
    ///
    /// The **ancestor** test, not path equality, is load-bearing: equality
    /// happens to work for gstack's shims today, whose manager root *is* the
    /// checkout directory, but it would silently answer zero the day the tool
    /// nested its skills one level deeper -- and a zero here reads as "no
    /// dependents to cascade," which is a wrong answer that deletes things
    /// (ADR 0026).
    ///
    /// `Path::starts_with` matches whole components, so a sibling checkout named
    /// `gstack-old` is not a dependent of `gstack` the way a string prefix would
    /// have it.
    pub fn build(skills: &[DiscoveredSkill]) -> Self {
        let mut counts: HashMap<SkillId, u32> = HashMap::new();
        for dependent in skills {
            let Some(root) = dependent.manager_root.as_deref() else {
                // Unmanaged: its content is its own entry, so it resolves into
                // nobody. Skipped before the inner loop rather than falling out
                // of it, because a depth-1 sibling can never be an ancestor.
                continue;
            };
            for candidate in skills {
                // A skill's own manager root is its content's parent, which is
                // never inside its own directory -- but identity is cheap to
                // assert and the alternative is trusting that forever.
                if candidate.id == dependent.id {
                    continue;
                }
                if root.starts_with(&candidate.canonical_dir) {
                    *counts.entry(candidate.id.clone()).or_insert(0) += 1;
                }
            }
        }
        DependentIndex { counts }
    }

    pub fn for_skill(&self, skill: &DiscoveredSkill) -> u32 {
        self.counts.get(&skill.id).copied().unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveryWarning {
    pub path: PathBuf,
    pub reason: String,
}

/// Harness-neutral: any `HarnessAdapter::discover_skills` implementation
/// returns this shape, not just the Claude Code one (ADR 0002).
#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    pub skills: Vec<DiscoveredSkill>,
    pub warnings: Vec<DiscoveryWarning>,
    /// The repo whose transcript was most recently written to, if any known
    /// repo exists. Only this repo's project skills are live (DESIGN.md UX
    /// decision #5); other repos' project skills are still discovered.
    pub active_repo_path: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_skill(dir_name: &str, declared_name: &str) -> DiscoveredSkill {
        DiscoveredSkill {
            id: SkillId::Personal { name: dir_name.to_string() },
            dir_path: PathBuf::from(format!("/tmp/{dir_name}")),
            canonical_dir: PathBuf::from(format!("/tmp/{dir_name}")),
            skill_md_path: PathBuf::from(format!("/tmp/{dir_name}/SKILL.md")),
            frontmatter: Frontmatter {
                declared_name: declared_name.to_string(),
                description: "does things".to_string(),
                raw_block: format!("name: {declared_name}\ndescription: does things"),
                model_invocable: true,
            },
            body: "body text".to_string(),
            manager_root: None,
            on_demand_files: vec![],
            live: true,
        }
    }

    #[test]
    fn directory_name_reads_from_skill_id_not_frontmatter() {
        let skill = sample_skill("connect-chrome", "open-gstack-browser");
        assert_eq!(skill.directory_name(), "connect-chrome");
    }

    #[test]
    fn name_mismatch_true_when_directory_and_declared_name_diverge() {
        let skill = sample_skill("connect-chrome", "open-gstack-browser");
        assert!(skill.name_mismatch());
    }

    #[test]
    fn name_mismatch_false_when_they_agree() {
        let skill = sample_skill("grilling", "grilling");
        assert!(!skill.name_mismatch());
    }

    /// A skill under the personal scan root, resolving its content wherever
    /// `manager_root` says. `None` is the unmanaged case: content in its own
    /// entry.
    fn skill_managed_by(dir_name: &str, manager_root: Option<&str>) -> DiscoveredSkill {
        DiscoveredSkill {
            canonical_dir: PathBuf::from(format!("/home/me/.claude/skills/{dir_name}")),
            manager_root: manager_root.map(PathBuf::from),
            ..sample_skill(dir_name, dir_name)
        }
    }

    /// The reference machine's shape, and the reason the count exists: `gstack`
    /// is unmanaged -- which alone reads as "safe to delete" -- while being the
    /// one entry 46 other skills resolve into (ADR 0026).
    #[test]
    fn skills_resolving_into_another_skills_directory_count_as_its_dependents() {
        let mut skills = vec![skill_managed_by("gstack", None)];
        skills.extend((0..46).map(|i| {
            skill_managed_by(&format!("shim-{i}"), Some("/home/me/.claude/skills/gstack/skills/engineering"))
        }));

        let index = DependentIndex::build(&skills);

        assert_eq!(index.for_skill(&skills[0]), 46);
        assert_eq!(index.for_skill(&skills[1]), 0, "a shim is nobody's manager root");
    }

    /// The ancestor test, stated as a difference: a manager root nested below
    /// the skill's directory still counts. Path equality would answer zero here,
    /// and a zero reads as "nothing to cascade" (ADR 0026).
    #[test]
    fn a_manager_root_nested_deeper_than_the_directory_still_counts() {
        let skills = vec![
            skill_managed_by("gstack", None),
            skill_managed_by("ship", Some("/home/me/.claude/skills/gstack/a/b/c/d")),
        ];

        assert_eq!(DependentIndex::build(&skills).for_skill(&skills[0]), 1);
    }

    /// `Path::starts_with` compares whole components, so a sibling checkout is
    /// not swept in by a shared name prefix the way a string compare would have
    /// it -- and cascading a delete over `gstack-old`'s dependents would trash
    /// entries the user never touched.
    #[test]
    fn a_sibling_sharing_a_name_prefix_is_not_a_dependent() {
        let skills = vec![
            skill_managed_by("gstack", None),
            skill_managed_by("ship", Some("/home/me/.claude/skills/gstack-old/skills")),
        ];

        assert_eq!(DependentIndex::build(&skills).for_skill(&skills[0]), 0);
    }

    /// The `.agents` shape: 20 entries resolving out to a manager root that is
    /// not a discovered skill at all. Managed, and nobody's dependent -- a
    /// manager root need not be a row (CONTEXT.md "Dependent skill").
    #[test]
    fn a_manager_root_outside_the_scan_root_makes_no_skill_a_manager() {
        let skills = vec![
            skill_managed_by("tdd", Some("/home/me/.agents/skills")),
            skill_managed_by("grilling", Some("/home/me/.agents/skills")),
        ];

        let index = DependentIndex::build(&skills);

        assert_eq!(index.for_skill(&skills[0]), 0);
        assert_eq!(index.for_skill(&skills[1]), 0);
    }

    #[test]
    fn an_unmanaged_skill_with_nothing_pointing_at_it_has_no_dependents() {
        let skills = vec![skill_managed_by("vercel-react", None), skill_managed_by("mine", None)];

        assert_eq!(DependentIndex::build(&skills).for_skill(&skills[0]), 0);
    }

    /// An entry whose `SKILL.md` links to a file it itself contains
    /// (`self-hosting/nested/SKILL.md`) has a manager root inside its own
    /// directory. It is one entry, so there is nothing to cascade to and nothing
    /// to warn about -- but the ancestor test matches it, so the identity guard
    /// is the only thing between that shape and a row claiming to provide for
    /// itself.
    #[test]
    fn a_skill_is_never_its_own_dependent() {
        let skills = vec![skill_managed_by("self-hosting", Some("/home/me/.claude/skills/self-hosting"))];

        assert_eq!(DependentIndex::build(&skills).for_skill(&skills[0]), 0);
    }
}
