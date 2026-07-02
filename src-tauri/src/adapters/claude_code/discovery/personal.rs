use crate::adapters::claude_code::discovery::scan::discover_skills_in_dir;
use crate::adapters::claude_code::paths::personal_skills_dir;
use crate::domain::skill::{DiscoveredSkill, DiscoveryWarning, SkillId};
use std::path::Path;

pub fn discover_personal_skills(claude_home: &Path) -> (Vec<DiscoveredSkill>, Vec<DiscoveryWarning>) {
    let root = personal_skills_dir(claude_home);
    discover_skills_in_dir(&root, |name| SkillId::Personal { name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn discovers_skills_under_claude_home_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let skill_dir = claude_home.join("skills").join("grilling");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: grilling\ndescription: Interview relentlessly.\n---\n\nBody.\n",
        )
        .unwrap();

        let (skills, warnings) = discover_personal_skills(claude_home);

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        match &skills[0].id {
            SkillId::Personal { name } => assert_eq!(name, "grilling"),
            other => panic!("expected Personal id, got {other:?}"),
        }
    }
}
