use crate::domain::footprint::TextConfidence;
use crate::domain::skill::DiscoveredSkill;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

pub struct AlwaysOnText {
    pub text: String,
    pub confidence: TextConfidence,
}

#[derive(Debug, Deserialize)]
struct AttachmentRecord {
    attachment: Option<SkillListingAttachment>,
}

#[derive(Debug, Deserialize)]
struct SkillListingAttachment {
    #[serde(rename = "type")]
    kind: String,
    content: String,
    names: Vec<String>,
}

/// Sources the always-on layer from a live transcript when one exists
/// (native, high confidence), falling back to a frontmatter reconstruction
/// only when no transcript in scope has ever rendered this skill (ADR 0016).
/// `search_project_dirs` is skill-type-scoped by the caller: every known
/// repo's project dir for personal/user-scoped-plugin skills, just the
/// owning repo's for project/scoped-plugin skills.
pub fn always_on_text(skill: &DiscoveredSkill, search_project_dirs: &[PathBuf]) -> AlwaysOnText {
    match find_rendered_bullet(skill.directory_name(), search_project_dirs) {
        Some(text) => AlwaysOnText { text, confidence: TextConfidence::Native },
        None => AlwaysOnText { text: reconstruct_bullet(skill), confidence: TextConfidence::Reconstructed },
    }
}

fn reconstruct_bullet(skill: &DiscoveredSkill) -> String {
    format!("- {}: {}", skill.directory_name(), skill.frontmatter.description)
}

fn find_rendered_bullet(directory_name: &str, search_project_dirs: &[PathBuf]) -> Option<String> {
    for transcript_path in transcripts_by_recency(search_project_dirs) {
        let Ok(content) = fs::read_to_string(&transcript_path) else { continue };
        for line in content.lines() {
            let Ok(record) = serde_json::from_str::<AttachmentRecord>(line) else { continue };
            let Some(listing) = record.attachment else { continue };
            if listing.kind != "skill_listing" {
                continue;
            }
            if let Some(bullet) = extract_bullet(&listing.content, &listing.names, directory_name) {
                return Some(bullet);
            }
        }
    }
    None
}

/// Anchored on the `names` array (present alongside `content` on a real
/// `skill_listing` attachment) rather than a bare `\n- ` scan, so a
/// description containing its own markdown list can't be mistaken for a
/// skill boundary. Not every entry has a `: description` suffix -- a skill
/// with no frontmatter description renders as a bare `- {name}`.
fn extract_bullet(content: &str, names: &[String], target: &str) -> Option<String> {
    let mut cursor = 0;
    let mut target_start = None;
    let mut target_end = None;

    for name in names {
        let marker = format!("- {name}");
        let start = content[cursor..].find(&marker)? + cursor;

        if target_start.is_some() {
            target_end = Some(start);
            break;
        }
        if name == target {
            target_start = Some(start);
        }

        cursor = start + marker.len();
    }

    let start = target_start?;
    let end = target_end.unwrap_or(content.len());
    Some(content[start..end].trim_end_matches('\n').to_string())
}

/// The on-invoke template, confirmed stable across both the `Skill` tool and
/// slash-command invocation paths (ADR 0017) -- computed directly, no
/// transcript lookup needed, unlike always-on.
pub fn on_invoke_text(skill: &DiscoveredSkill) -> String {
    format!("Base directory for this skill: {}\n\n{}", skill.dir_path.display(), skill.body)
}

/// Raw bytes of each bundled file (ADR 0017's ceiling, not a wrapper
/// reconstruction). A file that isn't valid UTF-8 is skipped, not fatal --
/// matches the discovery pipeline's fault-isolation posture.
pub fn on_demand_file_texts(skill: &DiscoveredSkill) -> Vec<(PathBuf, String)> {
    skill
        .on_demand_files
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok().map(|content| (path.clone(), content)))
        .collect()
}

fn transcripts_by_recency(project_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut entries: Vec<(PathBuf, SystemTime)> = Vec::new();

    for dir in project_dirs {
        let Ok(read_dir) = fs::read_dir(dir) else { continue };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                entries.push((path, mtime));
            }
        }
    }

    entries.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    entries.into_iter().map(|(path, _)| path).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::skill::{Frontmatter, SkillId};
    use std::path::Path;
    use std::thread::sleep;
    use std::time::Duration;

    fn sample_skill(dir_name: &str, description: &str) -> DiscoveredSkill {
        DiscoveredSkill {
            id: SkillId::Personal { name: dir_name.to_string() },
            dir_path: PathBuf::from(format!("/tmp/{dir_name}")),
            skill_md_path: PathBuf::from(format!("/tmp/{dir_name}/SKILL.md")),
            frontmatter: Frontmatter {
                declared_name: dir_name.to_string(),
                description: description.to_string(),
                raw_block: format!("name: {dir_name}\ndescription: {description}"),
            },
            body: "Body.".to_string(),
            is_symlink: false,
            symlink_target: None,
            on_demand_files: vec![],
            live: true,
        }
    }

    fn write_skill_listing_transcript(project_dir: &Path, file_name: &str, names: &[&str], content: &str) {
        fs::create_dir_all(project_dir).unwrap();
        let record = serde_json::json!({
            "type": "attachment",
            "attachment": {
                "type": "skill_listing",
                "content": content,
                "names": names,
                "skillCount": names.len(),
                "isInitial": true
            }
        });
        fs::write(project_dir.join(file_name), format!("{}\n", record)).unwrap();
    }

    #[test]
    fn extracts_the_target_bullet_including_a_description_without_bleeding_into_the_next() {
        let content = "- alpha: does alpha things\nover two lines\n- beta: does beta things\n- gamma\n";
        let names = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];

        assert_eq!(extract_bullet(content, &names, "beta"), Some("- beta: does beta things".to_string()));
        assert_eq!(extract_bullet(content, &names, "alpha"), Some("- alpha: does alpha things\nover two lines".to_string()));
    }

    #[test]
    fn extracts_a_bare_bullet_with_no_description() {
        let content = "- alpha: does alpha things\n- gamma\n- delta: does delta things\n";
        let names = vec!["alpha".to_string(), "gamma".to_string(), "delta".to_string()];

        assert_eq!(extract_bullet(content, &names, "gamma"), Some("- gamma".to_string()));
    }

    #[test]
    fn extracts_the_last_bullet_up_to_end_of_content() {
        let content = "- alpha: a\n- omega: last one";
        let names = vec!["alpha".to_string(), "omega".to_string()];

        assert_eq!(extract_bullet(content, &names, "omega"), Some("- omega: last one".to_string()));
    }

    #[test]
    fn missing_target_returns_none() {
        let content = "- alpha: a\n- beta: b";
        let names = vec!["alpha".to_string(), "beta".to_string()];

        assert_eq!(extract_bullet(content, &names, "not-there"), None);
    }

    #[test]
    fn always_on_text_prefers_the_native_transcript_line() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("repo-a");
        write_skill_listing_transcript(
            &project_dir,
            "session1.jsonl",
            &["grilling", "domain-modeling"],
            "- grilling: Interview relentlessly.\n- domain-modeling: Build the model.",
        );

        let skill = sample_skill("grilling", "fallback description, should not be used");
        let result = always_on_text(&skill, &[project_dir]);

        assert_eq!(result.text, "- grilling: Interview relentlessly.");
        assert_eq!(result.confidence, TextConfidence::Native);
    }

    #[test]
    fn always_on_text_falls_back_to_reconstruction_when_no_transcript_has_the_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("repo-a");
        write_skill_listing_transcript(&project_dir, "session1.jsonl", &["other-skill"], "- other-skill: something else");

        let skill = sample_skill("grilling", "Interview relentlessly.");
        let result = always_on_text(&skill, &[project_dir]);

        assert_eq!(result.text, "- grilling: Interview relentlessly.");
        assert_eq!(result.confidence, TextConfidence::Reconstructed);
    }

    #[test]
    fn always_on_text_falls_back_when_search_scope_is_empty() {
        let skill = sample_skill("grilling", "Interview relentlessly.");
        let result = always_on_text(&skill, &[]);

        assert_eq!(result.confidence, TextConfidence::Reconstructed);
    }

    #[test]
    fn most_recently_modified_transcript_in_scope_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("repo-a");

        write_skill_listing_transcript(&project_dir, "older.jsonl", &["grilling"], "- grilling: OLD wording");
        sleep(Duration::from_millis(20));
        write_skill_listing_transcript(&project_dir, "newer.jsonl", &["grilling"], "- grilling: NEW wording");

        let skill = sample_skill("grilling", "fallback");
        let result = always_on_text(&skill, &[project_dir]);

        assert_eq!(result.text, "- grilling: NEW wording");
    }

    #[test]
    fn on_invoke_text_matches_the_exact_adr_0017_template() {
        let mut skill = sample_skill("grilling", "Interview relentlessly.");
        skill.dir_path = PathBuf::from("/Users/test/.claude/skills/grilling");
        skill.body = "Interview me relentlessly about every aspect of this plan.".to_string();

        let result = on_invoke_text(&skill);

        assert_eq!(
            result,
            "Base directory for this skill: /Users/test/.claude/skills/grilling\n\nInterview me relentlessly about every aspect of this plan."
        );
    }

    #[test]
    fn on_demand_file_texts_reads_each_bundled_file_literally() {
        let tmp = tempfile::tempdir().unwrap();
        let file_a = tmp.path().join("CONTEXT-FORMAT.md");
        let file_b = tmp.path().join("ADR-FORMAT.md");
        fs::write(&file_a, "context format doc").unwrap();
        fs::write(&file_b, "adr format doc").unwrap();

        let mut skill = sample_skill("domain-modeling", "models domains");
        skill.on_demand_files = vec![file_a.clone(), file_b.clone()];

        let result = on_demand_file_texts(&skill);

        assert_eq!(result.len(), 2);
        assert!(result.contains(&(file_a, "context format doc".to_string())));
        assert!(result.contains(&(file_b, "adr format doc".to_string())));
    }

    #[test]
    fn on_demand_file_texts_skips_a_non_utf8_file_without_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let good_file = tmp.path().join("good.md");
        let bad_file = tmp.path().join("bad.bin");
        fs::write(&good_file, "readable text").unwrap();
        fs::write(&bad_file, [0xff, 0xfe, 0x00, 0xff]).unwrap();

        let mut skill = sample_skill("domain-modeling", "models domains");
        skill.on_demand_files = vec![good_file.clone(), bad_file];

        let result = on_demand_file_texts(&skill);

        assert_eq!(result, vec![(good_file, "readable text".to_string())]);
    }
}
