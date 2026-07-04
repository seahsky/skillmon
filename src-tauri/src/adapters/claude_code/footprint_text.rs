use super::listing_cache::SqliteListingCache;
use crate::domain::footprint::TextConfidence;
use crate::domain::skill::DiscoveredSkill;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct AlwaysOnText {
    pub text: String,
    pub confidence: TextConfidence,
}

/// The literal substring a `skill_listing` attachment line always contains
/// (`"type":"skill_listing"`, whatever the whitespace). Checked before the
/// full `serde_json` parse so the vast majority of transcript lines -- which
/// are ordinary messages, not skill listings -- are skipped for the price of
/// a substring scan instead of a full JSON parse. On a real machine this is
/// the difference between parsing ~1 line per transcript and parsing all
/// 682 MB of them.
const SKILL_LISTING_MARKER: &str = "skill_listing";

/// A one-pass index of the most-recent rendered bullet per skill, so a full
/// scan reads each transcript **once** instead of re-reading every transcript
/// per skill (which on a real machine is O(skills × transcripts × bytes) --
/// tens of GB of reads). Keyed `directory_name → project_dir → (mtime,
/// bullet)`, keeping the most-recent bullet per `(name, repo)` so a lookup
/// can be scoped to the repos a skill can actually render in (decision #7 /
/// ADR 0016): personal and plugin skills look across every repo, a project
/// skill only within its own.
///
/// It reads every in-scope transcript rather than stopping once each name is
/// seen: a name's absence from a repo is not observable early, so an
/// early-exit on global name coverage would silently drop a second repo's
/// rendering of the same skill name (two repos each with their own `deploy`
/// project skill) and break per-repo scoping. Reading each transcript at most
/// once is already the decisive win; the further optimization is incremental
/// byte-offset parsing (DESIGN.md), not a fragile early-exit.
#[derive(Default)]
pub struct ListingIndex {
    by_name: HashMap<String, HashMap<PathBuf, (SystemTime, String)>>,
}

impl ListingIndex {
    /// Single incremental pass over `transcripts`, which **must** be ordered
    /// most-recent-first: the first bullet seen for a `(name, repo)` is
    /// therefore the most recent, so later hits for that pair are ignored.
    ///
    /// Each transcript is read from disk only on a `cache` miss (a new file,
    /// or a changed `(mtime, size)`); an unchanged file's previously-extracted
    /// bullets are reused, so a warm rescan skips re-reading the corpus (issue
    /// #3, ADR 0022). The memo stores every `skill_listing` bullet in the file
    /// **unfiltered** by `wanted`, and filtering happens here at merge, so
    /// installing a new skill (which grows `wanted`) resolves from the memo
    /// with no re-read. `BuildStats.files_read` reports how many transcripts
    /// were actually read, so a test can assert a warm rescan reads zero.
    pub fn build_incremental(
        transcripts: &[TranscriptRef],
        wanted: &HashSet<String>,
        cache: &SqliteListingCache,
    ) -> (Self, BuildStats) {
        let mut index = ListingIndex::default();
        let mut stats = BuildStats::default();

        for transcript in transcripts {
            stats.files_total += 1;
            let path_key = transcript.path.to_string_lossy();
            let size = transcript.size as i64;
            let mnanos = mtime_nanos(transcript.mtime);

            let file_bullets = match mnanos.and_then(|m| cache.get(&path_key, m, size)) {
                Some(bullets) => bullets,
                None => {
                    stats.files_read += 1;
                    let Ok(content) = fs::read_to_string(&transcript.path) else { continue };
                    let bullets = parse_transcript_bullets(&content);
                    // Only memoize when the file can be keyed exactly; a
                    // missing/pre-1970 mtime is a forced re-read every scan.
                    if let Some(m) = mnanos {
                        cache.put(&path_key, m, size, &bullets);
                    }
                    bullets
                }
            };

            for (name, bullet) in file_bullets {
                if !wanted.contains(&name) {
                    continue;
                }
                let per_repo = index.by_name.entry(name).or_default();
                // First hit per (name, repo) wins because the input is
                // most-recent-first; don't overwrite with an older one.
                per_repo.entry(transcript.project_dir.clone()).or_insert_with(|| (transcript.mtime, bullet));
            }
        }

        (index, stats)
    }

    /// Full non-incremental build, used only by tests: an ephemeral in-memory
    /// memo makes every file a miss, reproducing the original read-everything
    /// behaviour while routing through the one real `build_incremental` extract
    /// path so the two can't drift.
    #[cfg(test)]
    pub fn build(transcripts: &[TranscriptRef], wanted: &HashSet<String>) -> Self {
        let cache = SqliteListingCache::open_in_memory().expect("in-memory listing cache opens");
        Self::build_incremental(transcripts, wanted, &cache).0
    }

    /// Most-recent bullet for `name` among the `allowed` project dirs. Callers
    /// pass the skill-type-scoped dir list (`always_on_search_dirs`), so
    /// personal/plugin skills effectively query globally and project skills
    /// query only their own repo -- preserving the exact scoping the per-skill
    /// path used.
    pub fn resolve(&self, name: &str, allowed: &[PathBuf]) -> Option<&str> {
        let per_repo = self.by_name.get(name)?;
        allowed
            .iter()
            .filter_map(|dir| per_repo.get(dir))
            .max_by_key(|(mtime, _)| *mtime)
            .map(|(_, bullet)| bullet.as_str())
    }
}

/// A transcript file paired with the project dir it belongs to, its mtime, and
/// its size. `(mtime, size)` is the incremental index's change-detector key
/// (ADR 0022); both come from the one `metadata()` call the enumeration
/// already makes.
pub struct TranscriptRef {
    pub path: PathBuf,
    pub project_dir: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
}

/// Per-build read accounting so a test can prove a warm rescan re-reads no
/// transcripts (issue #3's acceptance criterion).
#[derive(Default, Debug, Clone, Copy)]
pub struct BuildStats {
    pub files_total: usize,
    pub files_read: usize,
}

/// Collapses one transcript to its `skill_listing` bullets, first-occurrence
/// per name winning across every `skill_listing` line in the file (a file can
/// carry more than one: the initial injection plus a re-injection after a
/// mid-session compaction or a skill install). This is exactly the intra-file
/// selection `build_incremental`'s merge relies on, extracted so the memo
/// stores the same thing a fresh read would produce. Unfiltered by the wanted
/// set; filtering is the merge's job.
fn parse_transcript_bullets(content: &str) -> Vec<(String, String)> {
    let mut bullets: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in content.lines() {
        if !line.contains(SKILL_LISTING_MARKER) {
            continue;
        }
        let Ok(record) = serde_json::from_str::<AttachmentRecord>(line) else { continue };
        let Some(listing) = record.attachment else { continue };
        if listing.kind != "skill_listing" {
            continue;
        }
        for (name, bullet) in extract_all_bullets(&listing.content, &listing.names) {
            if seen.insert(name.clone()) {
                bullets.push((name, bullet));
            }
        }
    }
    bullets
}

/// A file's mtime as an exact integer count of nanoseconds since the Unix
/// epoch -- the change-detector key. `None` (a pre-1970 mtime, or one past
/// ~year 2262) forces a re-read rather than risking a false match.
fn mtime_nanos(mtime: SystemTime) -> Option<i64> {
    mtime.duration_since(UNIX_EPOCH).ok().and_then(|d| i64::try_from(d.as_nanos()).ok())
}

/// Always-on text for one skill resolved from a prebuilt `ListingIndex`
/// instead of re-reading transcripts -- the batched-scan counterpart to
/// `always_on_text`. Same native/reconstructed contract (ADR 0016).
pub fn always_on_text_from_index(
    skill: &DiscoveredSkill,
    index: &ListingIndex,
    search_project_dirs: &[PathBuf],
) -> AlwaysOnText {
    match index.resolve(skill.directory_name(), search_project_dirs) {
        Some(bullet) => AlwaysOnText { text: bullet.to_string(), confidence: TextConfidence::Native },
        None => AlwaysOnText { text: reconstruct_bullet(skill), confidence: TextConfidence::Reconstructed },
    }
}

/// Gathers every `.jsonl` under `project_dirs`, paired with its project dir
/// and mtime, ordered most-recent-first -- the input `ListingIndex::build`
/// expects.
pub fn transcript_refs_by_recency(project_dirs: &[PathBuf]) -> Vec<TranscriptRef> {
    let mut refs: Vec<TranscriptRef> = Vec::new();
    for dir in project_dirs {
        let Ok(read_dir) = fs::read_dir(dir) else { continue };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            refs.push(TranscriptRef { path, project_dir: dir.clone(), mtime, size: meta.len() });
        }
    }
    refs.sort_by_key(|r| std::cmp::Reverse(r.mtime));
    refs
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
            if !line.contains(SKILL_LISTING_MARKER) {
                continue;
            }
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

/// Extracts every skill's bullet from one `skill_listing` attachment in a
/// single pass, rather than re-scanning `content` once per target name. Same
/// anchoring as `extract_bullet` (each bullet spans from its `- {name}`
/// marker to the next present marker), just yielding all of them at once for
/// the index builder.
fn extract_all_bullets(content: &str, names: &[String]) -> Vec<(String, String)> {
    let mut starts: Vec<Option<usize>> = Vec::with_capacity(names.len());
    let mut cursor = 0;
    for name in names {
        let marker = format!("- {name}");
        match content[cursor..].find(&marker) {
            Some(off) => {
                let start = cursor + off;
                starts.push(Some(start));
                cursor = start + marker.len();
            }
            None => starts.push(None),
        }
    }

    let mut bullets = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let Some(start) = starts[i] else { continue };
        let end = starts[i + 1..].iter().flatten().next().copied().unwrap_or(content.len());
        bullets.push((name.clone(), content[start..end].trim_end_matches('\n').to_string()));
    }
    bullets
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

    fn wanted(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn extract_all_bullets_returns_every_named_bullet_in_one_pass() {
        let content = "- alpha: does alpha\nsecond line\n- beta\n- gamma: does gamma";
        let names = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];

        let bullets = extract_all_bullets(content, &names);

        assert_eq!(
            bullets,
            vec![
                ("alpha".to_string(), "- alpha: does alpha\nsecond line".to_string()),
                ("beta".to_string(), "- beta".to_string()),
                ("gamma".to_string(), "- gamma: does gamma".to_string()),
            ]
        );
    }

    #[test]
    fn index_resolves_a_wanted_skill_from_a_single_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        write_skill_listing_transcript(
            &project_dir,
            "s.jsonl",
            &["grilling", "domain-modeling"],
            "- grilling: Interview relentlessly.\n- domain-modeling: Build the model.",
        );

        let refs = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let index = ListingIndex::build(&refs, &wanted(&["grilling"]));

        assert_eq!(index.resolve("grilling", &[project_dir]), Some("- grilling: Interview relentlessly."));
    }

    #[test]
    fn index_keeps_the_most_recent_bullet_when_a_skill_appears_in_two_transcripts() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        write_skill_listing_transcript(&project_dir, "older.jsonl", &["grilling"], "- grilling: OLD wording");
        sleep(Duration::from_millis(20));
        write_skill_listing_transcript(&project_dir, "newer.jsonl", &["grilling"], "- grilling: NEW wording");

        let refs = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let index = ListingIndex::build(&refs, &wanted(&["grilling"]));

        assert_eq!(index.resolve("grilling", &[project_dir]), Some("- grilling: NEW wording"));
    }

    #[test]
    fn index_resolve_is_scoped_to_the_allowed_project_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_a = tmp.path().join("repo-a");
        let repo_b = tmp.path().join("repo-b");
        write_skill_listing_transcript(&repo_a, "s.jsonl", &["deploy"], "- deploy: repo A wording");
        write_skill_listing_transcript(&repo_b, "s.jsonl", &["deploy"], "- deploy: repo B wording");

        let refs = transcript_refs_by_recency(&[repo_a.clone(), repo_b.clone()]);
        let index = ListingIndex::build(&refs, &wanted(&["deploy"]));

        // A project skill scoped to repo A only sees repo A's rendering.
        assert_eq!(index.resolve("deploy", std::slice::from_ref(&repo_a)), Some("- deploy: repo A wording"));
        assert_eq!(index.resolve("deploy", std::slice::from_ref(&repo_b)), Some("- deploy: repo B wording"));
        // A personal skill scoped to both sees one of them (most-recent wins).
        assert!(index.resolve("deploy", &[repo_a, repo_b]).is_some());
    }

    #[test]
    fn index_records_a_shared_name_in_every_repo_it_renders_in() {
        // Two repos each have their own project skill named `deploy`. The index
        // must record both repos' renderings -- not stop after the first --
        // so each repo's project skill resolves to its own bullet. This is the
        // per-repo-scoping guarantee an early-exit on global name coverage
        // would silently break.
        let tmp = tempfile::tempdir().unwrap();
        let repo_a = tmp.path().join("repo-a");
        let repo_b = tmp.path().join("repo-b");
        // repo-b's session is the most recent globally.
        write_skill_listing_transcript(&repo_a, "s.jsonl", &["deploy"], "- deploy: repo A wording");
        sleep(Duration::from_millis(20));
        write_skill_listing_transcript(&repo_b, "s.jsonl", &["deploy"], "- deploy: repo B wording");

        let refs = transcript_refs_by_recency(&[repo_a.clone(), repo_b.clone()]);
        let index = ListingIndex::build(&refs, &wanted(&["deploy"]));

        assert_eq!(index.resolve("deploy", &[repo_a]), Some("- deploy: repo A wording"));
        assert_eq!(index.resolve("deploy", &[repo_b]), Some("- deploy: repo B wording"));
    }

    #[test]
    fn index_returns_none_for_a_skill_absent_from_every_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        write_skill_listing_transcript(&project_dir, "s.jsonl", &["other"], "- other: something");

        let refs = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let index = ListingIndex::build(&refs, &wanted(&["grilling"]));

        assert_eq!(index.resolve("grilling", &[project_dir]), None);
    }

    #[test]
    fn always_on_text_from_index_matches_the_per_skill_path() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        write_skill_listing_transcript(
            &project_dir,
            "s.jsonl",
            &["grilling"],
            "- grilling: Interview relentlessly.",
        );

        let skill = sample_skill("grilling", "fallback, unused");
        let refs = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let index = ListingIndex::build(&refs, &wanted(&["grilling"]));

        let via_index = always_on_text_from_index(&skill, &index, std::slice::from_ref(&project_dir));
        let via_scan = always_on_text(&skill, &[project_dir]);

        assert_eq!(via_index.text, via_scan.text);
        assert_eq!(via_index.confidence, TextConfidence::Native);
        assert_eq!(via_scan.confidence, TextConfidence::Native);
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

    // ---- issue #3: incremental index (build_incremental + persisted memo) ----

    fn manual_ref(path: PathBuf, project_dir: PathBuf, secs: u64, size: u64) -> TranscriptRef {
        TranscriptRef { path, project_dir, mtime: UNIX_EPOCH + Duration::from_secs(secs), size }
    }

    #[test]
    fn warm_rescan_reads_zero_transcripts_and_resolves_identically() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        write_skill_listing_transcript(&project_dir, "s.jsonl", &["grilling"], "- grilling: Interview relentlessly.");
        let refs = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let cache = SqliteListingCache::open_in_memory().unwrap();

        let (cold, cold_stats) = ListingIndex::build_incremental(&refs, &wanted(&["grilling"]), &cache);
        assert_eq!(cold_stats.files_read, refs.len(), "cold scan reads every transcript");

        let (warm, warm_stats) = ListingIndex::build_incremental(&refs, &wanted(&["grilling"]), &cache);
        assert_eq!(warm_stats.files_read, 0, "warm rescan re-reads nothing");
        assert_eq!(
            cold.resolve("grilling", std::slice::from_ref(&project_dir)),
            warm.resolve("grilling", std::slice::from_ref(&project_dir)),
        );
    }

    #[test]
    fn a_transcript_with_no_skill_listing_is_memoized_and_not_reread() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("plain.jsonl"), "{\"type\":\"message\",\"text\":\"hi\"}\n").unwrap();
        let refs = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let cache = SqliteListingCache::open_in_memory().unwrap();

        let (_, cold) = ListingIndex::build_incremental(&refs, &wanted(&["grilling"]), &cache);
        assert_eq!(cold.files_read, 1, "cold reads the plain transcript once");
        let (_, warm) = ListingIndex::build_incremental(&refs, &wanted(&["grilling"]), &cache);
        assert_eq!(warm.files_read, 0, "the negative-cache row skips the re-read");
    }

    #[test]
    fn two_skill_listings_in_one_file_keep_the_first_occurrence() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        fs::create_dir_all(&project_dir).unwrap();
        let first = serde_json::json!({"type":"attachment","attachment":{"type":"skill_listing","content":"- grilling: FIRST wording","names":["grilling"]}});
        let second = serde_json::json!({"type":"attachment","attachment":{"type":"skill_listing","content":"- grilling: SECOND wording","names":["grilling"]}});
        fs::write(project_dir.join("s.jsonl"), format!("{first}\n{second}\n")).unwrap();
        let refs = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let cache = SqliteListingCache::open_in_memory().unwrap();

        let (cold, _) = ListingIndex::build_incremental(&refs, &wanted(&["grilling"]), &cache);
        assert_eq!(cold.resolve("grilling", std::slice::from_ref(&project_dir)), Some("- grilling: FIRST wording"));
        let (warm, warm_stats) = ListingIndex::build_incremental(&refs, &wanted(&["grilling"]), &cache);
        assert_eq!(warm_stats.files_read, 0);
        assert_eq!(warm.resolve("grilling", std::slice::from_ref(&project_dir)), Some("- grilling: FIRST wording"));
    }

    #[test]
    fn an_appended_render_grows_the_file_and_is_picked_up_as_native() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        write_skill_listing_transcript(&project_dir, "s.jsonl", &["other"], "- other: something");
        let cache = SqliteListingCache::open_in_memory().unwrap();

        let refs1 = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let (idx1, _) = ListingIndex::build_incremental(&refs1, &wanted(&["grilling"]), &cache);
        assert_eq!(idx1.resolve("grilling", std::slice::from_ref(&project_dir)), None, "not rendered yet");

        let extra = serde_json::json!({"type":"attachment","attachment":{"type":"skill_listing","content":"- grilling: Interview relentlessly.","names":["grilling"]}});
        let mut f = std::fs::OpenOptions::new().append(true).open(project_dir.join("s.jsonl")).unwrap();
        writeln!(f, "{extra}").unwrap();
        drop(f);

        let refs2 = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let (idx2, stats2) = ListingIndex::build_incremental(&refs2, &wanted(&["grilling"]), &cache);
        assert!(stats2.files_read >= 1, "the grown file is re-read");
        assert_eq!(
            idx2.resolve("grilling", std::slice::from_ref(&project_dir)),
            Some("- grilling: Interview relentlessly.")
        );
    }

    #[test]
    fn an_older_mtime_at_the_same_size_still_forces_a_reread() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        fs::create_dir_all(&project_dir).unwrap();
        let path = project_dir.join("s.jsonl");
        let rec_a = serde_json::json!({"type":"attachment","attachment":{"type":"skill_listing","content":"- grilling: A","names":["grilling"]}});
        fs::write(&path, format!("{rec_a}\n")).unwrap();
        let size = fs::metadata(&path).unwrap().len();
        let cache = SqliteListingCache::open_in_memory().unwrap();

        let ref_new = manual_ref(path.clone(), project_dir.clone(), 1000, size);
        let (idx1, _) = ListingIndex::build_incremental(std::slice::from_ref(&ref_new), &wanted(&["grilling"]), &cache);
        assert_eq!(idx1.resolve("grilling", std::slice::from_ref(&project_dir)), Some("- grilling: A"));

        // Same byte size, edited content, OLDER mtime: strict-equality gating
        // must re-read, never treat "not newer" as "unchanged".
        let rec_b = serde_json::json!({"type":"attachment","attachment":{"type":"skill_listing","content":"- grilling: B","names":["grilling"]}});
        fs::write(&path, format!("{rec_b}\n")).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), size, "test setup: rewrite must keep the same byte size");
        let ref_old = manual_ref(path, project_dir.clone(), 999, size);
        let (idx2, stats2) = ListingIndex::build_incremental(std::slice::from_ref(&ref_old), &wanted(&["grilling"]), &cache);
        assert_eq!(stats2.files_read, 1, "older mtime is a miss, not a hit");
        assert_eq!(idx2.resolve("grilling", std::slice::from_ref(&project_dir)), Some("- grilling: B"));
    }

    #[test]
    fn bullets_are_stored_unfiltered_so_a_new_wanted_name_resolves_without_a_reread() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        write_skill_listing_transcript(&project_dir, "s.jsonl", &["grilling", "deploy"], "- grilling: g\n- deploy: d");
        let refs = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let cache = SqliteListingCache::open_in_memory().unwrap();

        let (only_grilling, _) = ListingIndex::build_incremental(&refs, &wanted(&["grilling"]), &cache);
        assert_eq!(only_grilling.resolve("deploy", std::slice::from_ref(&project_dir)), None, "deploy not wanted yet");

        let (with_deploy, stats) = ListingIndex::build_incremental(&refs, &wanted(&["grilling", "deploy"]), &cache);
        assert_eq!(stats.files_read, 0, "unchanged file is not re-read even though wanted grew");
        assert_eq!(with_deploy.resolve("deploy", std::slice::from_ref(&project_dir)), Some("- deploy: d"));
    }

    #[test]
    fn the_memo_persists_across_a_reopen_of_the_same_sqlite_file() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        write_skill_listing_transcript(&project_dir, "s.jsonl", &["grilling"], "- grilling: Interview relentlessly.");
        let refs = transcript_refs_by_recency(std::slice::from_ref(&project_dir));
        let db = tmp.path().join("listing_index.sqlite");

        {
            let cache = SqliteListingCache::open(&db).unwrap();
            let (_, stats) = ListingIndex::build_incremental(&refs, &wanted(&["grilling"]), &cache);
            assert_eq!(stats.files_read, refs.len());
        }
        let cache = SqliteListingCache::open(&db).unwrap();
        let (idx, stats) = ListingIndex::build_incremental(&refs, &wanted(&["grilling"]), &cache);
        assert_eq!(stats.files_read, 0, "a reopen of the persisted memo re-reads nothing");
        assert_eq!(idx.resolve("grilling", std::slice::from_ref(&project_dir)), Some("- grilling: Interview relentlessly."));
    }

    /// Isolates the cost issue #3 actually targets -- the listing-index build
    /// (transcript enumeration + read/parse) -- from the rest of scan_all
    /// (discovery, on-demand file reads), over this machine's real corpus.
    /// Proves the warm index rebuild is well under a second. Build in RELEASE:
    /// `cargo test --release --manifest-path src-tauri/Cargo.toml
    ///   footprint_text::tests::warm_index_build_over_the_real_corpus -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn warm_index_build_over_the_real_corpus_is_sub_second() {
        use std::time::Instant;
        let Some(home) = dirs::home_dir() else { return };
        let Ok(entries) = fs::read_dir(home.join(".claude").join("projects")) else { return };
        let project_dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        if project_dirs.is_empty() {
            eprintln!("no real transcripts on this machine; skipping");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let cache = SqliteListingCache::open(&tmp.path().join("listing_index.sqlite")).unwrap();
        let all: HashSet<String> = HashSet::new(); // read cost is independent of `wanted`

        // Time the whole index portion of a scan: enumerate + build.
        let t_cold = Instant::now();
        let refs = transcript_refs_by_recency(&project_dirs);
        let (_, cold) = ListingIndex::build_incremental(&refs, &all, &cache);
        let cold_elapsed = t_cold.elapsed();

        let t_warm = Instant::now();
        let refs2 = transcript_refs_by_recency(&project_dirs);
        let (_, warm) = ListingIndex::build_incremental(&refs2, &all, &cache);
        let warm_elapsed = t_warm.elapsed();

        eprintln!(
            "listing index over {} transcripts: cold {cold_elapsed:?} (read {}), warm {warm_elapsed:?} (read {})",
            refs.len(),
            cold.files_read,
            warm.files_read,
        );
        assert_eq!(warm.files_read, 0, "warm index build must re-read nothing");
        assert!(
            warm_elapsed < std::time::Duration::from_secs(1),
            "warm index rebuild must be well under a second, was {warm_elapsed:?}"
        );
    }
}
