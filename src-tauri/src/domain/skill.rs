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
}

#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    pub id: SkillId,
    pub dir_path: PathBuf,
    pub skill_md_path: PathBuf,
    pub frontmatter: Frontmatter,
    pub body: String,
    pub is_symlink: bool,
    pub symlink_target: Option<PathBuf>,
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

    pub fn name_mismatch(&self) -> bool {
        self.directory_name() != self.frontmatter.declared_name
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveryWarning {
    pub path: PathBuf,
    pub reason: String,
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
            },
            body: "body text".to_string(),
            is_symlink: false,
            symlink_target: None,
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
