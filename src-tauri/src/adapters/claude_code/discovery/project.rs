use crate::adapters::claude_code::discovery::scan::{discover_skills_in_dir, ChildDirs};
use crate::adapters::claude_code::discovery::transcript::{enumerate_known_repos, RepoInfo};
use crate::adapters::claude_code::paths::repo_skills_dir;
use crate::domain::skill::{DiscoveredSkill, DiscoveryWarning, SkillId};
use std::path::Path;

pub fn discover_project_skills(
    claude_home: &Path,
) -> Vec<(RepoInfo, Vec<DiscoveredSkill>, Vec<DiscoveryWarning>)> {
    enumerate_known_repos(claude_home)
        .into_iter()
        .map(|repo| {
            let skills_dir = repo_skills_dir(&repo.repo_path);
            let repo_path = repo.repo_path.clone();
            let (skills, warnings) =
                discover_skills_in_dir(&skills_dir, ChildDirs::AreSkillEntries, move |name| {
                    SkillId::Project { repo_path: repo_path.clone(), name }
                });
            (repo, skills, warnings)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn discovers_skills_scoped_to_each_known_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();

        let repo_a = tmp.path().join("repo-a");
        let project_dir_a = claude_home.join("projects").join("-tmp-repo-a");
        fs::create_dir_all(&project_dir_a).unwrap();
        fs::write(
            project_dir_a.join("s.jsonl"),
            format!(r#"{{"cwd":"{}","sessionId":"1"}}"#, repo_a.display()),
        )
        .unwrap();
        let skill_dir = repo_a.join(".claude").join("skills").join("repo-only-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: repo-only-skill\ndescription: only here\n---\n\nBody.\n",
        )
        .unwrap();

        let results = discover_project_skills(claude_home);

        assert_eq!(results.len(), 1);
        let (repo, skills, warnings) = &results[0];
        assert_eq!(repo.repo_path, repo_a);
        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        match &skills[0].id {
            SkillId::Project { repo_path, name } => {
                assert_eq!(repo_path, &repo_a);
                assert_eq!(name, "repo-only-skill");
            }
            other => panic!("expected Project id, got {other:?}"),
        }
    }

    #[test]
    fn repo_with_no_project_skills_dir_yields_empty_not_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let repo = tmp.path().join("repo-with-no-skills");
        let project_dir = claude_home.join("projects").join("-tmp-repo-with-no-skills");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("s.jsonl"),
            format!(r#"{{"cwd":"{}","sessionId":"1"}}"#, repo.display()),
        )
        .unwrap();

        let results = discover_project_skills(claude_home);

        assert_eq!(results.len(), 1);
        let (_repo, skills, warnings) = &results[0];
        assert!(skills.is_empty());
        assert!(warnings.is_empty());
    }
}
