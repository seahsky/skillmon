use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Deserialize)]
struct TranscriptRecord {
    cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub repo_path: PathBuf,
    pub project_dir: PathBuf,
    pub last_modified: SystemTime,
}

/// Reads the real `cwd` out of any transcript inside `project_dir`. Never
/// decodes the directory name -- that encoding is ambiguous on hyphenated
/// paths (ADR 0014).
pub fn read_repo_cwd(project_dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(project_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(cwd) = read_repo_cwd_from_file(&path) {
            return Some(cwd);
        }
    }
    None
}

fn read_repo_cwd_from_file(transcript_path: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(transcript_path).ok()?;
    for line in content.lines() {
        if let Ok(record) = serde_json::from_str::<TranscriptRecord>(line) {
            if let Some(cwd) = record.cwd {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    None
}

fn most_recent_transcript_mtime(project_dir: &Path) -> Option<SystemTime> {
    let entries = fs::read_dir(project_dir).ok()?;
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()
}

/// Treats `~/.claude/projects/*/` as a candidate list only; every repo path
/// comes from a real `cwd` field, never a decoded directory name (ADR 0014).
pub fn enumerate_known_repos(claude_home: &Path) -> Vec<RepoInfo> {
    let projects_root = crate::adapters::claude_code::paths::projects_dir(claude_home);
    let mut repos = Vec::new();

    let Ok(entries) = fs::read_dir(&projects_root) else { return repos };
    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }
        let Some(repo_path) = read_repo_cwd(&project_dir) else { continue };
        let last_modified = most_recent_transcript_mtime(&project_dir).unwrap_or(SystemTime::UNIX_EPOCH);
        repos.push(RepoInfo { repo_path, project_dir, last_modified });
    }

    repos
}

/// The active repo is whichever known repo's transcript was most recently
/// written to.
pub fn find_active_repo(claude_home: &Path) -> Option<RepoInfo> {
    enumerate_known_repos(claude_home)
        .into_iter()
        .max_by_key(|r| r.last_modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    fn write_transcript(project_dir: &Path, file_name: &str, cwd: &str) {
        fs::create_dir_all(project_dir).unwrap();
        fs::write(
            project_dir.join(file_name),
            format!(r#"{{"cwd":"{cwd}","sessionId":"abc","type":"attachment"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn reads_real_cwd_not_decoded_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        // Directory name is ambiguous to decode (could be .../bas-ai/tools or
        // .../bas/ai/tools) -- the real cwd inside the transcript is not.
        let project_dir = claude_home.join("projects").join("-Users-test-bas-ai-tools");
        write_transcript(&project_dir, "session1.jsonl", "/Users/test/bas-ai-tools");

        let repos = enumerate_known_repos(claude_home);

        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].repo_path, PathBuf::from("/Users/test/bas-ai-tools"));
    }

    #[test]
    fn active_repo_is_the_most_recently_modified_one() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();

        let older = claude_home.join("projects").join("-Users-test-older-repo");
        write_transcript(&older, "session1.jsonl", "/Users/test/older-repo");

        sleep(Duration::from_millis(20));

        let newer = claude_home.join("projects").join("-Users-test-newer-repo");
        write_transcript(&newer, "session1.jsonl", "/Users/test/newer-repo");

        let active = find_active_repo(claude_home).unwrap();
        assert_eq!(active.repo_path, PathBuf::from("/Users/test/newer-repo"));
    }

    #[test]
    fn missing_projects_dir_yields_no_repos() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(enumerate_known_repos(tmp.path()).is_empty());
        assert!(find_active_repo(tmp.path()).is_none());
    }

    #[test]
    fn project_dir_with_no_cwd_bearing_record_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let project_dir = claude_home.join("projects").join("-Users-test-no-cwd");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("session1.jsonl"), r#"{"type":"last-prompt"}"#).unwrap();

        assert!(enumerate_known_repos(claude_home).is_empty());
    }
}
