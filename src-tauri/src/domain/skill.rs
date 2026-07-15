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
    /// `SKILL.md`. Read by the source column (issue #30) and by removal, which
    /// must know whether deleting an entry is durable (issue #31, ADR 0027).
    #[allow(dead_code)]
    pub manager_root: Option<PathBuf>,
    pub on_demand_files: Vec<PathBuf>,
    pub live: bool,
}

impl DiscoveredSkill {
    pub fn directory_name(&self) -> &str {
        match &self.id {
            SkillId::Personal { name } => name,
            SkillId::Project { name, .. } => name,
            SkillId::Plugin { name, .. } => name,
        }
    }

    /// Read by a UI "directory name ≠ declared name" badge in a later plan.
    #[allow(dead_code)]
    pub fn name_mismatch(&self) -> bool {
        self.directory_name() != self.frontmatter.declared_name
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
}
