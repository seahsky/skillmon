//! `.agents`: the capable tool -- trash the content, prune its lock entry
//! (ADR 0027).
//!
//! Every rule below was read off the `skills` CLI that writes the file
//! (`node_modules/skills/dist/cli.mjs`, v1.5.17), never guessed from a sample:
//! the reference machine's `.skill-lock.json` has `"skills": {}`, so no
//! populated entry exists here to copy the shape from, and inventing one would
//! be editing a third-party tool's state on a hunch.
//!
//! What the CLI settles:
//!
//! - The lock lives at `$XDG_STATE_HOME/skills/.skill-lock.json` when that
//!   variable is set, and at `~/.agents/.skill-lock.json` otherwise
//!   (`getSkillLockPath`). The skills themselves stay at `~/.agents/skills`
//!   (`getCanonicalSkillsDir`) either way, so the two are resolved separately.
//! - `skills` is a map keyed by the skill's **unsanitized** name
//!   (`addSkillToLock(skill.name, ...)`), while the folder it installs is
//!   `sanitizeName(skill.name)` (`installSkill:1755`). The two coincide for an
//!   ordinary kebab-case name and diverge for anything else, so the key cannot
//!   simply be read off the directory -- see `lock_key_for`.
//! - Pruning is a key delete and a rewrite (`removeSkillFromLock`), serialized
//!   with `JSON.stringify(lock, null, 2)`.
//! - `readSkillLock` discards the whole file when `version < 3`, so writing a
//!   version we do not understand back is never merely cosmetic.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{ManagingTool, SourceError};
use crate::domain::skill::DiscoveredSkill;

const TOOL: &str = "the skills CLI (.agents)";
/// `CURRENT_VERSION` in the CLI. A lock at any other version has a shape this
/// build has not read, so it is refused rather than rewritten.
const SUPPORTED_VERSION: u32 = 3;

pub struct AgentsTool {
    /// `~/.agents/skills` -- the manager root discovery reports for these
    /// skills, canonicalized so it compares against one (ADR 0026).
    skills_dir: PathBuf,
    /// `~/.agents/.skill-lock.json`, or the `XDG_STATE_HOME` location.
    lock_path: PathBuf,
}

impl AgentsTool {
    pub fn new(skills_dir: PathBuf, lock_path: PathBuf) -> Self {
        // Canonical if it resolves, raw if it does not: `manager_root` is
        // always canonical, so an uncanonicalized `~` (a symlinked home on a
        // dotfiles setup) would silently match nothing and the capable tool
        // would never be found. Falling back to the raw path keeps a
        // not-yet-created directory from being an error.
        let skills_dir = fs::canonicalize(&skills_dir).unwrap_or(skills_dir);
        AgentsTool { skills_dir, lock_path }
    }

    /// The real machine's `.agents`, or `None` when there is no home directory
    /// to hang it off.
    pub fn from_env() -> Option<Self> {
        let home = dirs::home_dir()?;
        let lock_path = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            Some(xdg) => PathBuf::from(xdg).join("skills").join(LOCK_FILE),
            None => home.join(AGENTS_DIR).join(LOCK_FILE),
        };
        Some(AgentsTool::new(home.join(AGENTS_DIR).join(SKILLS_SUBDIR), lock_path))
    }

    fn read_lock(&self) -> Result<Option<Map<String, Value>>, SourceError> {
        let raw = match fs::read_to_string(&self.lock_path) {
            Ok(raw) => raw,
            // No lock at all is not a fault: the tool creates one on demand
            // (`createEmptyLockFile`), and a machine that never installed
            // globally has none. There is simply nothing recorded to prune.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let parsed: Value = serde_json::from_str(&raw).map_err(|e| self.malformed(e.to_string()))?;
        let Some(object) = parsed.as_object() else {
            return Err(self.malformed("the top level is not a JSON object".to_string()));
        };
        match object.get("version").and_then(Value::as_u64) {
            Some(v) if v == u64::from(SUPPORTED_VERSION) => {}
            Some(v) => {
                return Err(SourceError::UnsupportedState {
                    tool: TOOL,
                    path: self.lock_path.clone(),
                    found: v.to_string(),
                    supported: SUPPORTED_VERSION,
                })
            }
            None => return Err(self.malformed("no numeric `version` field".to_string())),
        }
        if !object.get("skills").is_some_and(Value::is_object) {
            return Err(self.malformed("no `skills` object".to_string()));
        }
        Ok(Some(object.clone()))
    }

    /// Writes the lock back the way the CLI does: two-space JSON, whole file.
    ///
    /// Key order survives because `serde_json`'s `preserve_order` is on, which
    /// matters for a file skillmon does not own -- a rewrite that reshuffled
    /// `version`/`skills`/`dismissed` would show up as a spurious diff in
    /// anyone who tracks their dotfiles, for a change that touched one key.
    fn write_lock(&self, lock: &Map<String, Value>) -> Result<(), SourceError> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut json = serde_json::to_string_pretty(&Value::Object(lock.clone()))
            .expect("a parsed lock is plain data and always re-serializes");
        // `JSON.stringify(x, null, 2)` emits no trailing newline, and matching
        // it keeps skillmon's rewrite invisible next to the tool's own.
        json.truncate(json.trim_end().len());
        fs::write(&self.lock_path, json)?;
        Ok(())
    }

    fn malformed(&self, reason: String) -> SourceError {
        SourceError::MalformedState { tool: TOOL, path: self.lock_path.clone(), reason }
    }

    /// The lock key installing `directory_name` would have produced.
    ///
    /// The inverse of the CLI's own rule, applied the only way it can be: the
    /// map is keyed by the raw name and the folder by `sanitizeName` of it, and
    /// `sanitizeName` is lossy, so the key cannot be computed from the folder --
    /// it has to be searched for. An exact key match is tried first because it
    /// is the overwhelmingly common case (an ordinary kebab-case name sanitizes
    /// to itself) and because it is unambiguous.
    ///
    /// Ambiguity is possible in principle -- `My Skill` and `my-skill` both
    /// sanitize to `my-skill` -- but not in practice, since the tool would have
    /// installed both to one folder. On a tie the first match in the map's own
    /// order wins, which is the same entry the tool's last write left pointing
    /// at that folder.
    fn lock_key_for(skills: &Map<String, Value>, directory_name: &str) -> Option<String> {
        if skills.contains_key(directory_name) {
            return Some(directory_name.to_string());
        }
        skills.keys().find(|key| sanitize_name(key) == directory_name).cloned()
    }
}

const AGENTS_DIR: &str = ".agents";
const SKILLS_SUBDIR: &str = "skills";
const LOCK_FILE: &str = ".skill-lock.json";

/// A port of the CLI's `sanitizeName`, quirks included:
///
/// ```js
/// name.toLowerCase()
///     .replace(/[^a-z0-9._]+/g, "-")
///     .replace(/^[.\-]+|[.\-]+$/g, "")
///     .substring(0, 255) || "unnamed-skill"
/// ```
///
/// Faithful rather than tidy. The truncation runs *after* the trim, so a name
/// long enough to be cut can still end in `-`; reproducing that is the point,
/// since the folder on disk carries the same quirk and the two have to match.
/// `substring` counts UTF-16 units, but everything surviving the second replace
/// is ASCII, so `chars().take(255)` agrees with it exactly.
fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '_' {
            out.push(ch);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    let trimmed: String =
        out.trim_start_matches(['.', '-']).trim_end_matches(['.', '-']).chars().take(255).collect();
    if trimmed.is_empty() {
        "unnamed-skill".to_string()
    } else {
        trimmed
    }
}

/// What `forget_source` dropped, so `relearn_source` can put it back exactly.
///
/// The key is stored beside the value because it cannot be recovered from the
/// folder name (`sanitize_name` is lossy), and a restore that guessed it would
/// re-file the skill under a name the tool never used.
#[derive(serde::Serialize, serde::Deserialize)]
struct ForgottenEntry {
    key: String,
    entry: Value,
}

impl ManagingTool for AgentsTool {
    fn name(&self) -> &'static str {
        "the skills CLI (.agents)"
    }

    fn detects(&self, root: &Path) -> bool {
        root == self.skills_dir
    }

    /// Capable. The tool is not resident -- it runs on demand, so nothing
    /// rebuilds behind skillmon's back -- and its lock is a documented map this
    /// build can edit precisely.
    fn can_remove_source(&self) -> Option<&str> {
        None
    }

    fn forget_source(&self, skill: &DiscoveredSkill) -> Result<Option<String>, SourceError> {
        let Some(mut lock) = self.read_lock()? else { return Ok(None) };
        let skills = lock.get_mut("skills").and_then(Value::as_object_mut).expect("read_lock checked `skills`");

        let Some(key) = Self::lock_key_for(skills, skill.directory_name()) else {
            // The content is still staged and the entry still removed; the lock
            // simply never knew about this skill (installed by hand, or removed
            // from the lock already). Nothing to undo, so nothing to return.
            return Ok(None);
        };
        let entry = skills.remove(&key).expect("the key came from this map");
        self.write_lock(&lock)?;

        Ok(Some(
            serde_json::to_string(&ForgottenEntry { key, entry })
                .expect("a value read out of the lock always re-serializes"),
        ))
    }

    fn relearn_source(&self, state: &str) -> Result<(), SourceError> {
        let forgotten: ForgottenEntry =
            serde_json::from_str(state).map_err(|e| self.malformed(format!("skillmon's own saved lock entry: {e}")))?;

        // The lock is re-read rather than remembered, so a restore lands in
        // whatever the tool has written since -- including a lock it recreated
        // from scratch while this entry sat in the trash.
        let mut lock = match self.read_lock()? {
            Some(lock) => lock,
            None => empty_lock(),
        };
        let skills = lock.get_mut("skills").and_then(Value::as_object_mut).expect("read_lock checked `skills`");
        skills.insert(forgotten.key, forgotten.entry);
        self.write_lock(&lock)
    }
}

/// `createEmptyLockFile` in the CLI, reproduced so a restore into a machine
/// whose lock has since been deleted writes a file the tool will accept rather
/// than one it discards.
fn empty_lock() -> Map<String, Value> {
    let mut lock = Map::new();
    lock.insert("version".to_string(), Value::from(SUPPORTED_VERSION));
    lock.insert("skills".to_string(), Value::Object(Map::new()));
    lock.insert("dismissed".to_string(), Value::Object(Map::new()));
    lock
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::skill::{Frontmatter, SkillId};
    use serde_json::json;
    use tempfile::{tempdir, TempDir};

    struct Fixture {
        tmp: TempDir,
        tool: AgentsTool,
    }

    impl Fixture {
        fn new(lock: Option<Value>) -> Self {
            let tmp = tempdir().unwrap();
            let skills_dir = tmp.path().join(".agents/skills");
            fs::create_dir_all(&skills_dir).unwrap();
            let lock_path = tmp.path().join(".agents/.skill-lock.json");
            if let Some(lock) = lock {
                fs::write(&lock_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();
            }
            let tool = AgentsTool::new(skills_dir, lock_path);
            Fixture { tmp, tool }
        }

        /// A skill installed the `.agents` way: content under
        /// `~/.agents/skills/<name>`, entry under the scan root symlinking to it.
        fn install(&self, folder: &str) -> DiscoveredSkill {
            let content = self.tmp.path().join(".agents/skills").join(folder);
            fs::create_dir_all(&content).unwrap();
            fs::write(content.join("SKILL.md"), "---\nname: x\ndescription: d\n---\nbody").unwrap();

            let entry = self.tmp.path().join("skills").join(folder);
            fs::create_dir_all(entry.parent().unwrap()).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&content, &entry).unwrap();

            skill(folder, &entry)
        }

        fn lock(&self) -> Value {
            serde_json::from_str(&fs::read_to_string(self.tmp.path().join(".agents/.skill-lock.json")).unwrap())
                .unwrap()
        }
    }

    fn skill(name: &str, entry: &Path) -> DiscoveredSkill {
        DiscoveredSkill {
            id: SkillId::Personal { name: name.to_string() },
            dir_path: entry.to_path_buf(),
            canonical_dir: entry.to_path_buf(),
            skill_md_path: entry.join("SKILL.md"),
            frontmatter: Frontmatter {
                declared_name: name.to_string(),
                description: "d".to_string(),
                raw_block: "name: x".to_string(),
                model_invocable: true,
            },
            body: "body".to_string(),
            manager_root: None,
            on_demand_files: vec![],
            live: true,
        }
    }

    fn lock_with(skills: Value) -> Value {
        json!({
            "version": 3,
            "skills": skills,
            "dismissed": { "findSkillsPrompt": true },
            "lastSelectedAgents": ["claude-code", "codex"]
        })
    }

    fn entry_value() -> Value {
        json!({
            "source": "mattpocock/skills",
            "sourceType": "github",
            "sourceUrl": "https://github.com/mattpocock/skills",
            "ref": "main",
            "skillPath": "skills/tdd/SKILL.md",
            "skillFolderHash": "abc123",
            "installedAt": "2026-07-01T00:00:00.000Z",
            "updatedAt": "2026-07-01T00:00:00.000Z"
        })
    }

    #[test]
    fn the_manager_root_it_detects_is_the_canonical_skills_dir() {
        let f = Fixture::new(None);
        let real = fs::canonicalize(f.tmp.path().join(".agents/skills")).unwrap();

        assert!(f.tool.detects(&real));
        assert!(!f.tool.detects(&f.tmp.path().join("skills")), "the scan root is not a manager root");
    }

    /// The whole reason this tool exists on the capable side of ADR 0027.
    #[test]
    fn agents_can_remove_a_source() {
        assert_eq!(Fixture::new(None).tool.can_remove_source(), None);
    }

    #[test]
    fn forgetting_a_skill_drops_only_its_key_and_leaves_the_rest_of_the_lock_alone() {
        let f = Fixture::new(Some(lock_with(json!({ "tdd": entry_value(), "grilling": entry_value() }))));
        let skill = f.install("tdd");

        let state = f.tool.forget_source(&skill).unwrap().expect("tdd was in the lock");

        let lock = f.lock();
        assert!(lock["skills"].get("tdd").is_none(), "the pruned key is gone");
        assert!(lock["skills"].get("grilling").is_some(), "its neighbour is untouched");
        assert_eq!(lock["version"], 3);
        assert_eq!(lock["dismissed"]["findSkillsPrompt"], true, "state skillmon does not own survives");
        assert_eq!(lock["lastSelectedAgents"][0], "claude-code");
        assert!(state.contains("skillFolderHash"), "the dropped entry is captured for the undo");
    }

    /// The round trip ADR 0027 demands of itself: it rejected deleting a target
    /// partly *because* that desyncs the lock, so a restore that left it pruned
    /// would commit the same sin one step later.
    #[test]
    fn relearning_puts_the_exact_entry_back_under_its_own_key() {
        let f = Fixture::new(Some(lock_with(json!({ "tdd": entry_value() }))));
        let skill = f.install("tdd");
        let before = f.lock();

        let state = f.tool.forget_source(&skill).unwrap().unwrap();
        f.tool.relearn_source(&state).unwrap();

        assert_eq!(f.lock(), before, "the lock is byte-for-byte what it was before the removal");
    }

    /// The divergence the key search exists for: the map is keyed by the raw
    /// name, the folder is `sanitizeName` of it, and reading the key off the
    /// folder would prune nothing at all.
    #[test]
    fn a_skill_whose_lock_key_is_not_its_folder_name_is_still_found() {
        let f = Fixture::new(Some(lock_with(json!({ "My TDD Skill": entry_value() }))));
        // What the CLI's installSkill would have created for that name.
        let skill = f.install("my-tdd-skill");

        let state = f.tool.forget_source(&skill).unwrap().expect("the sanitized folder must find its raw key");

        assert!(f.lock()["skills"].get("My TDD Skill").is_none());
        f.tool.relearn_source(&state).unwrap();
        assert!(f.lock()["skills"].get("My TDD Skill").is_some(), "restored under the tool's own key, not the folder's");
    }

    /// Already-absent is success: the caller wants the key gone, and it is.
    /// Returning `None` also keeps the trash entry from storing an undo that
    /// would re-add a key the tool never had.
    #[test]
    fn forgetting_a_skill_the_lock_never_knew_is_not_an_error() {
        let f = Fixture::new(Some(lock_with(json!({ "grilling": entry_value() }))));
        let skill = f.install("hand-installed");

        assert_eq!(f.tool.forget_source(&skill).unwrap(), None);
        assert!(f.lock()["skills"].get("grilling").is_some(), "an unrelated key is not touched");
    }

    #[test]
    fn a_machine_with_no_lock_file_has_nothing_to_forget() {
        let f = Fixture::new(None);
        let skill = f.install("tdd");

        assert_eq!(f.tool.forget_source(&skill).unwrap(), None);
    }

    /// A version this build has not read is refused, not rewritten. The CLI
    /// discards any lock below `CURRENT_VERSION` wholesale, so writing back a
    /// shape we guessed at could cost the user every lock entry they have.
    #[test]
    fn a_lock_at_an_unknown_version_is_refused_rather_than_rewritten() {
        let f = Fixture::new(Some(json!({ "version": 4, "skills": { "tdd": entry_value() } })));
        let skill = f.install("tdd");
        let before = f.lock();

        let err = f.tool.forget_source(&skill).unwrap_err();

        assert!(matches!(err, SourceError::UnsupportedState { ref found, .. } if found == "4"), "got {err:?}");
        assert_eq!(f.lock(), before, "a refused prune writes nothing");
    }

    #[test]
    fn a_malformed_lock_is_refused_rather_than_rewritten() {
        let f = Fixture::new(None);
        fs::write(f.tmp.path().join(".agents/.skill-lock.json"), "{ not json").unwrap();
        let skill = f.install("tdd");

        assert!(matches!(f.tool.forget_source(&skill).unwrap_err(), SourceError::MalformedState { .. }));
    }

    /// A lock missing `skills` is not a v3 lock, whatever it says: `readSkillLock`
    /// itself treats `!parsed.skills` as an empty file, so writing to one would
    /// be inventing structure.
    #[test]
    fn a_lock_without_a_skills_map_is_refused() {
        let f = Fixture::new(Some(json!({ "version": 3, "dismissed": {} })));
        let skill = f.install("tdd");

        assert!(matches!(f.tool.forget_source(&skill).unwrap_err(), SourceError::MalformedState { .. }));
    }

    /// A restore whose lock has since been deleted must leave one the tool will
    /// accept -- `readSkillLock` discards anything below v3, so an entry written
    /// into a version-less file would vanish on the next run.
    #[test]
    fn relearning_into_a_vanished_lock_recreates_one_the_tool_accepts() {
        let f = Fixture::new(Some(lock_with(json!({ "tdd": entry_value() }))));
        let skill = f.install("tdd");
        let state = f.tool.forget_source(&skill).unwrap().unwrap();
        fs::remove_file(f.tmp.path().join(".agents/.skill-lock.json")).unwrap();

        f.tool.relearn_source(&state).unwrap();

        let lock = f.lock();
        assert_eq!(lock["version"], 3);
        assert!(lock["skills"].get("tdd").is_some());
    }

    /// `source_of` is the trait default, and this is the shape it has to get
    /// right: the entry is a link, so the content is what it resolves to, which
    /// is what gets staged into the unit.
    #[cfg(unix)]
    #[test]
    fn the_source_of_a_linked_entry_is_the_content_it_resolves_to() {
        let f = Fixture::new(None);
        let skill = f.install("tdd");
        let expected = fs::canonicalize(f.tmp.path().join(".agents/skills/tdd")).unwrap();

        assert_eq!(f.tool.source_of(&skill), Some(expected));
    }

    #[test]
    fn sanitize_name_matches_the_clis_own_rule() {
        // The ordinary case: a kebab-case name is its own folder, which is why
        // the exact-key fast path carries almost every real skill.
        assert_eq!(sanitize_name("tdd"), "tdd");
        assert_eq!(sanitize_name("resolving-merge-conflicts"), "resolving-merge-conflicts");
        // Lowercased, and runs of anything outside [a-z0-9._] collapse to ONE
        // dash -- the regex is `+`-quantified.
        assert_eq!(sanitize_name("My TDD Skill"), "my-tdd-skill");
        assert_eq!(sanitize_name("a  b"), "a-b");
        assert_eq!(sanitize_name("@scope/pkg"), "scope-pkg");
        // Dots and underscores survive; leading/trailing dots and dashes do not.
        assert_eq!(sanitize_name("v1.2_beta"), "v1.2_beta");
        assert_eq!(sanitize_name("--hi--"), "hi");
        assert_eq!(sanitize_name("...hi..."), "hi");
        // Nothing left to name it with.
        assert_eq!(sanitize_name("!!!"), "unnamed-skill");
        assert_eq!(sanitize_name(""), "unnamed-skill");
        // Non-ASCII collapses to a dash, which the trim then eats if it landed
        // on an end -- the two rules compose, and only running the real thing
        // settles what they produce together.
        assert_eq!(sanitize_name("café"), "caf");
        assert_eq!(sanitize_name("Ünïcode"), "n-code");
    }

    /// The quirk reproduced on purpose: truncation runs after the trim, so a
    /// long name can still end in a dash. The folder on disk carries it too, so
    /// a "tidier" port would stop matching real directories.
    #[test]
    fn sanitize_name_truncates_after_trimming_and_keeps_the_quirk() {
        let long = format!("{}{}", "a".repeat(254), " tail");
        let sanitized = sanitize_name(&long);
        assert_eq!(sanitized.len(), 255);
        assert!(sanitized.ends_with('-'), "the CLI does not re-trim after substring, and neither do we");
    }
}
