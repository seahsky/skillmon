# Skill Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the Claude Code harness adapter's skill-discovery pipeline — personal, project, and plugin skills, unified into one `Vec<DiscoveredSkill>` with a stable identity, plugin liveness resolved, and discovery failures surfaced as warnings rather than aborting the scan.

**Architecture:** A generic `domain::skill` module holds harness-neutral types (`SkillId`, `DiscoveredSkill`, `Frontmatter`, `DiscoveryWarning`). All Claude-Code-specific knowledge — paths, frontmatter YAML parsing, the depth-1 scan, transcript `cwd` reading, `installed_plugins.json`/`plugin.json` parsing, `enabledPlugins` merging — lives under `adapters::claude_code`, per ADR 0002. A single `ClaudeCodeAdapter::discover_skills()` method assembles all three sources into one result. This plan does **not** compute footprint (a follow-up plan) or implement mutation/attribution.

**Tech Stack:** Rust (edition 2021, workspace already scaffolded via Tauri v2 in `src-tauri/`), `serde`/`serde_json` (already present), `serde_yaml_ng` for frontmatter YAML, `thiserror` for typed errors, `dirs` for home-directory resolution, `tempfile` (dev-only) for filesystem-fixture tests.

## Global Constraints

- Rust edition 2021, crate `skillmon_lib` at `src-tauri/` (per existing `Cargo.toml`).
- Every Claude-Code-specific fact (paths, JSON/YAML shapes, `~/.claude` layout) lives under `src-tauri/src/adapters/claude_code/`, never in `src-tauri/src/domain/` (ADR 0002).
- Discovery is fault-isolated per item: a malformed `SKILL.md`, an unreadable file, or a stale registry path is skipped and recorded as a `DiscoveryWarning`, never propagated as an `Err` that aborts the whole scan (resolved in grilling, 2026-07-02).
- Skill identity is `Personal(name)`, `Project(repo_path, name)`, or `Plugin(marketplace, plugin, name)` — `name` is always the **directory name**, never the frontmatter `name:` field, and never includes plugin version (`src-tauri/CONTEXT.md`, "Skill identity").
- Repo paths are read from a transcript's real `cwd` field, **never** decoded from the `~/.claude/projects/<encoded-cwd>/` directory name — that encoding is ambiguous on hyphenated paths (ADR 0014).
- Plugin `installPath` is always read verbatim from `installed_plugins.json`, never reconstructed from `cache/<marketplace>/<plugin>/<version>/` (ADR 0014's neighbor decision, `src-tauri/CONTEXT.md`).
- A plugin is live if enabled in the global settings OR the active repo's `settings.json` OR the active repo's `settings.local.json` — an OR across all three, not a precedence order (ADR 0015).
- No network calls anywhere in this plan (footprint/tokenizer work, which needs network, is out of scope).

---

## File Structure

```
src-tauri/src/
  lib.rs                              (modify: add `mod domain; mod adapters;`)
  domain/
    mod.rs                            (new: `pub mod skill;`)
    skill.rs                          (new: SkillId, InstallScope, Frontmatter, DiscoveredSkill, DiscoveryWarning)
  adapters/
    mod.rs                            (new: `pub mod claude_code;`)
    claude_code/
      mod.rs                          (new: ClaudeCodeAdapter, DiscoveryResult, module wiring)
      paths.rs                        (new: path helpers, parameterized by claude_home)
      frontmatter.rs                  (new: parse_skill_md)
      settings.rs                     (new: is_plugin_live -- ADR 0015 OR-merge)
      discovery/
        mod.rs                        (new: `pub mod scan; pub mod personal; pub mod transcript; pub mod project; pub mod plugin;`)
        scan.rs                       (new: discover_skills_in_dir -- shared depth-1 scanner)
        personal.rs                   (new: discover_personal_skills)
        transcript.rs                 (new: read_repo_cwd, enumerate_known_repos, find_active_repo)
        project.rs                    (new: discover_project_skills)
        plugin.rs                     (new: parse_installed_plugins, discover_plugin_skills)
```

---

### Task 1: Add dependencies, verify the crate still builds

**Files:**
- Modify: `src-tauri/Cargo.toml`

**Interfaces:**
- Produces: `serde_yaml_ng`, `thiserror`, `dirs` available as `[dependencies]`; `tempfile` available as `[dev-dependencies]`.

- [ ] **Step 1: Add the dependencies**

Edit `src-tauri/Cargo.toml`, in the `[dependencies]` block, add after `tauri-plugin-notification = "2"`:

```toml
serde_yaml_ng = "0.10"
thiserror = "2"
dirs = "6"
```

Add a new section at the end of the file:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: builds successfully (this only pulls in the new crates; nothing uses them yet).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "chore: add discovery dependencies (serde_yaml_ng, thiserror, dirs, tempfile)"
```

---

### Task 2: Domain types — `SkillId`, `Frontmatter`, `DiscoveredSkill`, `DiscoveryWarning`

**Files:**
- Create: `src-tauri/src/domain/mod.rs`
- Create: `src-tauri/src/domain/skill.rs`
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Produces:
  - `domain::skill::SkillId` enum: `Personal { name: String }`, `Project { repo_path: PathBuf, name: String }`, `Plugin { marketplace: String, plugin: String, name: String }`
  - `domain::skill::InstallScope` enum: `User`, `Project`, `Local`
  - `domain::skill::Frontmatter` struct: `declared_name: String`, `description: String`, `raw_block: String`
  - `domain::skill::DiscoveredSkill` struct: `id: SkillId`, `dir_path: PathBuf`, `skill_md_path: PathBuf`, `frontmatter: Frontmatter`, `body: String`, `is_symlink: bool`, `symlink_target: Option<PathBuf>`, `on_demand_files: Vec<PathBuf>`, `live: bool`
  - `domain::skill::DiscoveredSkill::directory_name(&self) -> &str`
  - `domain::skill::DiscoveredSkill::name_mismatch(&self) -> bool`
  - `domain::skill::DiscoveryWarning` struct: `path: PathBuf`, `reason: String`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/domain/skill.rs`:

```rust
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
```

Create `src-tauri/src/domain/mod.rs`:

```rust
pub mod skill;
```

Modify `src-tauri/src/lib.rs` — add near the top (before the `#[cfg_attr...]` run function, alongside any existing `mod`/`use` lines):

```rust
mod domain;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml domain::skill -- --exact` (this will fail to compile until `mod domain;` and the file exist — expected, since we just created them together; run it now to confirm the tests are wired up)
Expected: after Step 1's edits, this should actually **pass** already since Step 1 included both the test and implementation together (the domain types are pure data with no external behavior to red/green separately). Confirm with Step 4 instead.

- [ ] **Step 3: (no separate red step — types module, proceed to verify green)**

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml domain::skill`
Expected: 3 tests pass (`directory_name_reads_from_skill_id_not_frontmatter`, `name_mismatch_true_when_directory_and_declared_name_diverge`, `name_mismatch_false_when_they_agree`).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/domain/mod.rs src-tauri/src/domain/skill.rs src-tauri/src/lib.rs
git commit -m "feat: add domain skill identity types"
```

---

### Task 3: Frontmatter parser

**Files:**
- Create: `src-tauri/src/adapters/mod.rs`
- Create: `src-tauri/src/adapters/claude_code/mod.rs`
- Create: `src-tauri/src/adapters/claude_code/frontmatter.rs`
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: `domain::skill::Frontmatter` (Task 2)
- Produces: `adapters::claude_code::frontmatter::parse_skill_md(content: &str) -> Result<(Frontmatter, String), FrontmatterError>`, `adapters::claude_code::frontmatter::FrontmatterError` enum: `NoDelimiters`, `InvalidYaml(serde_yaml_ng::Error)`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/adapters/claude_code/frontmatter.rs`:

```rust
use crate::domain::skill::Frontmatter;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FrontmatterError {
    #[error("no frontmatter delimiters found")]
    NoDelimiters,
    #[error("invalid YAML frontmatter: {0}")]
    InvalidYaml(#[from] serde_yaml_ng::Error),
}

#[derive(Debug, serde::Deserialize)]
struct FrontmatterYaml {
    name: String,
    description: String,
}

pub fn parse_skill_md(content: &str) -> Result<(Frontmatter, String), FrontmatterError> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = content
        .strip_prefix("---\r\n")
        .or_else(|| content.strip_prefix("---\n"))
        .ok_or(FrontmatterError::NoDelimiters)?;
    let end = rest.find("\n---").ok_or(FrontmatterError::NoDelimiters)?;
    let raw_block = &rest[..end];
    let after = &rest[end + 4..];
    let body = after.strip_prefix('\n').unwrap_or(after).to_string();

    let parsed: FrontmatterYaml = serde_yaml_ng::from_str(raw_block)?;

    Ok((
        Frontmatter {
            declared_name: parsed.name,
            description: parsed.description,
            raw_block: raw_block.to_string(),
        },
        body,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_description_and_body() {
        let content = "---\nname: grilling\ndescription: Interview the user relentlessly.\n---\n\nBody line one.\nBody line two.\n";
        let (frontmatter, body) = parse_skill_md(content).unwrap();
        assert_eq!(frontmatter.declared_name, "grilling");
        assert_eq!(frontmatter.description, "Interview the user relentlessly.");
        assert_eq!(body, "Body line one.\nBody line two.\n");
    }

    #[test]
    fn tolerates_unknown_extra_frontmatter_fields() {
        let content = "---\nname: grill-with-docs\ndescription: sharpens a plan\ndisable-model-invocation: true\n---\n\nBody.\n";
        let (frontmatter, _) = parse_skill_md(content).unwrap();
        assert_eq!(frontmatter.declared_name, "grill-with-docs");
    }

    #[test]
    fn errors_on_missing_delimiters() {
        let content = "no frontmatter here";
        assert!(matches!(parse_skill_md(content), Err(FrontmatterError::NoDelimiters)));
    }

    #[test]
    fn errors_on_missing_required_field() {
        let content = "---\nname: incomplete\n---\n\nBody.\n";
        assert!(matches!(parse_skill_md(content), Err(FrontmatterError::InvalidYaml(_))));
    }

    #[test]
    fn colon_in_description_does_not_break_parsing() {
        let content = "---\nname: codex\ndescription: \"Ask codex anything: second opinions welcome\"\n---\n\nBody.\n";
        let (frontmatter, _) = parse_skill_md(content).unwrap();
        assert_eq!(frontmatter.description, "Ask codex anything: second opinions welcome");
    }
}
```

Create `src-tauri/src/adapters/claude_code/mod.rs`:

```rust
pub mod frontmatter;
```

Create `src-tauri/src/adapters/mod.rs`:

```rust
pub mod claude_code;
```

Modify `src-tauri/src/lib.rs` — next to `mod domain;`, add:

```rust
mod adapters;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::frontmatter`
Expected: FAIL to compile (module wiring didn't exist before this step) — confirms the test file is actually being picked up. If Step 1 was applied in full, this instead **passes**; either outcome confirms the wiring is live. Proceed to Step 3 to double-check green.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::frontmatter`
Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/adapters/mod.rs src-tauri/src/adapters/claude_code/mod.rs src-tauri/src/adapters/claude_code/frontmatter.rs src-tauri/src/lib.rs
git commit -m "feat: add SKILL.md frontmatter parser"
```

---

### Task 4: Claude Code path helpers

**Files:**
- Create: `src-tauri/src/adapters/claude_code/paths.rs`
- Modify: `src-tauri/src/adapters/claude_code/mod.rs`

**Interfaces:**
- Produces:
  - `paths::personal_skills_dir(claude_home: &Path) -> PathBuf`
  - `paths::projects_dir(claude_home: &Path) -> PathBuf`
  - `paths::installed_plugins_path(claude_home: &Path) -> PathBuf`
  - `paths::global_settings_path(claude_home: &Path) -> PathBuf`
  - `paths::repo_skills_dir(repo_path: &Path) -> PathBuf`
  - `paths::repo_settings_path(repo_path: &Path) -> PathBuf`
  - `paths::repo_local_settings_path(repo_path: &Path) -> PathBuf`
  - `paths::default_claude_home() -> PathBuf` (real `~/.claude`, used only by production wiring, never by tests)

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/adapters/claude_code/paths.rs`:

```rust
use std::path::{Path, PathBuf};

pub fn default_claude_home() -> PathBuf {
    dirs::home_dir()
        .expect("home directory must be resolvable")
        .join(".claude")
}

pub fn personal_skills_dir(claude_home: &Path) -> PathBuf {
    claude_home.join("skills")
}

pub fn projects_dir(claude_home: &Path) -> PathBuf {
    claude_home.join("projects")
}

pub fn installed_plugins_path(claude_home: &Path) -> PathBuf {
    claude_home.join("plugins").join("installed_plugins.json")
}

pub fn global_settings_path(claude_home: &Path) -> PathBuf {
    claude_home.join("settings.json")
}

pub fn repo_skills_dir(repo_path: &Path) -> PathBuf {
    repo_path.join(".claude").join("skills")
}

pub fn repo_settings_path(repo_path: &Path) -> PathBuf {
    repo_path.join(".claude").join("settings.json")
}

pub fn repo_local_settings_path(repo_path: &Path) -> PathBuf {
    repo_path.join(".claude").join("settings.local.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_expected_relative_paths() {
        let home = Path::new("/tmp/fake-home/.claude");
        assert_eq!(personal_skills_dir(home), Path::new("/tmp/fake-home/.claude/skills"));
        assert_eq!(projects_dir(home), Path::new("/tmp/fake-home/.claude/projects"));
        assert_eq!(
            installed_plugins_path(home),
            Path::new("/tmp/fake-home/.claude/plugins/installed_plugins.json")
        );
        assert_eq!(global_settings_path(home), Path::new("/tmp/fake-home/.claude/settings.json"));

        let repo = Path::new("/tmp/some-repo");
        assert_eq!(repo_skills_dir(repo), Path::new("/tmp/some-repo/.claude/skills"));
        assert_eq!(repo_settings_path(repo), Path::new("/tmp/some-repo/.claude/settings.json"));
        assert_eq!(
            repo_local_settings_path(repo),
            Path::new("/tmp/some-repo/.claude/settings.local.json")
        );
    }
}
```

Modify `src-tauri/src/adapters/claude_code/mod.rs`:

```rust
pub mod frontmatter;
pub mod paths;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::paths`
Expected: FAIL to compile before `pub mod paths;` is added — confirms wiring is required. Add the `mod.rs` edit from Step 1, then re-run.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::paths`
Expected: 1 test passes.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/adapters/claude_code/paths.rs src-tauri/src/adapters/claude_code/mod.rs
git commit -m "feat: add Claude Code path helpers"
```

---

### Task 5: Shared depth-1 scanner

**Files:**
- Create: `src-tauri/src/adapters/claude_code/discovery/mod.rs`
- Create: `src-tauri/src/adapters/claude_code/discovery/scan.rs`
- Modify: `src-tauri/src/adapters/claude_code/mod.rs`

**Interfaces:**
- Consumes: `frontmatter::parse_skill_md` (Task 3), `domain::skill::{DiscoveredSkill, DiscoveryWarning, SkillId}` (Task 2)
- Produces: `discovery::scan::discover_skills_in_dir(dir: &Path, make_id: impl Fn(String) -> SkillId) -> (Vec<DiscoveredSkill>, Vec<DiscoveryWarning>)` — depth-1 scan; a missing root directory yields `(vec![], vec![])` (not a warning: absent scan roots are normal, not anomalous, per `src-tauri/CONTEXT.md`).

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/adapters/claude_code/discovery/scan.rs`:

```rust
use crate::adapters::claude_code::frontmatter::parse_skill_md;
use crate::domain::skill::{DiscoveredSkill, DiscoveryWarning, SkillId};
use std::fs;
use std::path::{Path, PathBuf};

pub fn discover_skills_in_dir(
    dir: &Path,
    make_id: impl Fn(String) -> SkillId,
) -> (Vec<DiscoveredSkill>, Vec<DiscoveryWarning>) {
    let mut skills = Vec::new();
    let mut warnings = Vec::new();

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return (skills, warnings),
    };

    for entry in entries.flatten() {
        let dir_path = entry.path();

        let metadata = match fs::symlink_metadata(&dir_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_symlink = metadata.file_type().is_symlink();
        if !dir_path.is_dir() {
            continue;
        }
        let symlink_target = if is_symlink { fs::read_link(&dir_path).ok() } else { None };

        let name = match dir_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let skill_md_path = dir_path.join("SKILL.md");
        let content = match fs::read_to_string(&skill_md_path) {
            Ok(c) => c,
            Err(_) => {
                warnings.push(DiscoveryWarning {
                    path: skill_md_path,
                    reason: "no readable SKILL.md".to_string(),
                });
                continue;
            }
        };

        let (frontmatter, body) = match parse_skill_md(&content) {
            Ok(parsed) => parsed,
            Err(e) => {
                warnings.push(DiscoveryWarning {
                    path: skill_md_path,
                    reason: format!("malformed frontmatter: {e}"),
                });
                continue;
            }
        };

        let on_demand_files = list_on_demand_files(&dir_path, &skill_md_path);

        skills.push(DiscoveredSkill {
            id: make_id(name),
            dir_path,
            skill_md_path,
            frontmatter,
            body,
            is_symlink,
            symlink_target,
            on_demand_files,
            live: true,
        });
    }

    (skills, warnings)
}

fn list_on_demand_files(dir_path: &Path, skip: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files_recursive(dir_path, skip, &mut files);
    files
}

fn collect_files_recursive(dir: &Path, skip: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip {
            continue;
        }
        if path.is_dir() {
            collect_files_recursive(&path, skip, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write_skill(root: &Path, dir_name: &str, name: &str, description: &str, body: &str) {
        let dir = root.join(dir_name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn discovers_well_formed_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "grilling", "grilling", "Interview relentlessly.", "Body.");

        let (skills, warnings) =
            discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        assert_eq!(skills[0].directory_name(), "grilling");
        assert_eq!(skills[0].frontmatter.description, "Interview relentlessly.");
        assert_eq!(skills[0].body, "Body.\n");
        assert!(skills[0].live);
    }

    #[test]
    fn missing_root_yields_no_skills_and_no_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");

        let (skills, warnings) = discover_skills_in_dir(&missing, |name| SkillId::Personal { name });

        assert!(skills.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn malformed_skill_is_skipped_and_warned_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "good", "good", "fine", "Body.");
        let bad_dir = tmp.path().join("bad");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("SKILL.md"), "not frontmatter at all").unwrap();

        let (skills, warnings) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].directory_name(), "good");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("malformed frontmatter"));
    }

    #[test]
    fn directory_with_no_skill_md_is_skipped_and_warned() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("empty-dir")).unwrap();

        let (skills, warnings) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("no readable SKILL.md"));
    }

    #[test]
    fn symlinked_skill_directory_records_target() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real-location");
        write_skill(tmp.path(), "real-location", "linked", "a linked skill", "Body.");
        let scan_root = tmp.path().join("scan-root");
        fs::create_dir_all(&scan_root).unwrap();
        symlink(&real_dir, scan_root.join("linked")).unwrap();

        let (skills, warnings) = discover_skills_in_dir(&scan_root, |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        assert!(skills[0].is_symlink);
        assert_eq!(skills[0].symlink_target.as_deref(), Some(real_dir.as_path()));
    }

    #[test]
    fn bundled_reference_files_are_collected_as_on_demand() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "domain-modeling", "domain-modeling", "models domains", "Body.");
        fs::write(tmp.path().join("domain-modeling").join("CONTEXT-FORMAT.md"), "format doc").unwrap();
        fs::write(tmp.path().join("domain-modeling").join("ADR-FORMAT.md"), "adr doc").unwrap();

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Personal { name });

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].on_demand_files.len(), 2);
    }

    #[test]
    fn make_id_closure_receives_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "foo", "foo", "desc", "Body.");

        let (skills, _) = discover_skills_in_dir(tmp.path(), |name| SkillId::Plugin {
            marketplace: "test-market".to_string(),
            plugin: "test-plugin".to_string(),
            name,
        });

        assert_eq!(
            skills[0].id,
            SkillId::Plugin {
                marketplace: "test-market".to_string(),
                plugin: "test-plugin".to_string(),
                name: "foo".to_string(),
            }
        );
    }
}
```

`SkillId` needs `PartialEq` for the last test's `assert_eq!` — it already derives `PartialEq, Eq, Hash` from Task 2, so this works as-is.

Create `src-tauri/src/adapters/claude_code/discovery/mod.rs`:

```rust
pub mod scan;
```

Modify `src-tauri/src/adapters/claude_code/mod.rs`:

```rust
pub mod discovery;
pub mod frontmatter;
pub mod paths;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::scan`
Expected: FAIL to compile before the `mod.rs` files are wired — confirms the test is live once Step 1 is fully applied.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::scan`
Expected: 7 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/adapters/claude_code/discovery/mod.rs src-tauri/src/adapters/claude_code/discovery/scan.rs src-tauri/src/adapters/claude_code/mod.rs
git commit -m "feat: add shared depth-1 skill scanner"
```

---

### Task 6: Personal skill discovery

**Files:**
- Create: `src-tauri/src/adapters/claude_code/discovery/personal.rs`
- Modify: `src-tauri/src/adapters/claude_code/discovery/mod.rs`

**Interfaces:**
- Consumes: `scan::discover_skills_in_dir` (Task 5), `paths::personal_skills_dir` (Task 4)
- Produces: `discovery::personal::discover_personal_skills(claude_home: &Path) -> (Vec<DiscoveredSkill>, Vec<DiscoveryWarning>)`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/adapters/claude_code/discovery/personal.rs`:

```rust
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
```

Modify `src-tauri/src/adapters/claude_code/discovery/mod.rs`:

```rust
pub mod personal;
pub mod scan;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::personal`
Expected: FAIL to compile before `pub mod personal;` is added.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::personal`
Expected: 1 test passes.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/adapters/claude_code/discovery/personal.rs src-tauri/src/adapters/claude_code/discovery/mod.rs
git commit -m "feat: add personal skill discovery"
```

---

### Task 7: Transcript `cwd` reading, repo enumeration, active repo

**Files:**
- Create: `src-tauri/src/adapters/claude_code/discovery/transcript.rs`
- Modify: `src-tauri/src/adapters/claude_code/discovery/mod.rs`

**Interfaces:**
- Consumes: `paths::projects_dir` (Task 4)
- Produces:
  - `discovery::transcript::RepoInfo` struct: `repo_path: PathBuf`, `project_dir: PathBuf`, `last_modified: SystemTime`
  - `discovery::transcript::read_repo_cwd(project_dir: &Path) -> Option<PathBuf>`
  - `discovery::transcript::enumerate_known_repos(claude_home: &Path) -> Vec<RepoInfo>`
  - `discovery::transcript::find_active_repo(claude_home: &Path) -> Option<RepoInfo>`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/adapters/claude_code/discovery/transcript.rs`:

```rust
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
```

Modify `src-tauri/src/adapters/claude_code/discovery/mod.rs`:

```rust
pub mod personal;
pub mod scan;
pub mod transcript;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::transcript`
Expected: FAIL to compile before `pub mod transcript;` is added.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::transcript`
Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/adapters/claude_code/discovery/transcript.rs src-tauri/src/adapters/claude_code/discovery/mod.rs
git commit -m "feat: add transcript-based repo and active-repo discovery"
```

---

### Task 8: Project skill discovery

**Files:**
- Create: `src-tauri/src/adapters/claude_code/discovery/project.rs`
- Modify: `src-tauri/src/adapters/claude_code/discovery/mod.rs`

**Interfaces:**
- Consumes: `transcript::enumerate_known_repos`, `transcript::RepoInfo` (Task 7), `scan::discover_skills_in_dir` (Task 5), `paths::repo_skills_dir` (Task 4)
- Produces: `discovery::project::discover_project_skills(claude_home: &Path) -> Vec<(RepoInfo, Vec<DiscoveredSkill>, Vec<DiscoveryWarning>)>`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/adapters/claude_code/discovery/project.rs`:

```rust
use crate::adapters::claude_code::discovery::scan::discover_skills_in_dir;
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
            let (skills, warnings) = discover_skills_in_dir(&skills_dir, move |name| SkillId::Project {
                repo_path: repo_path.clone(),
                name,
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
```

Modify `src-tauri/src/adapters/claude_code/discovery/mod.rs`:

```rust
pub mod personal;
pub mod project;
pub mod scan;
pub mod transcript;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::project`
Expected: FAIL to compile before `pub mod project;` is added.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::project`
Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/adapters/claude_code/discovery/project.rs src-tauri/src/adapters/claude_code/discovery/mod.rs
git commit -m "feat: add project skill discovery"
```

---

### Task 9: Plugin registry parsing and plugin skill discovery

**Files:**
- Create: `src-tauri/src/adapters/claude_code/discovery/plugin.rs`
- Modify: `src-tauri/src/adapters/claude_code/discovery/mod.rs`

**Interfaces:**
- Consumes: `scan::discover_skills_in_dir` (Task 5), `paths::installed_plugins_path` (Task 4), `domain::skill::InstallScope` (Task 2)
- Produces:
  - `discovery::plugin::PluginInstallRecord` struct: `plugin_at_marketplace: String`, `plugin: String`, `marketplace: String`, `scope: InstallScope`, `install_path: PathBuf`
  - `discovery::plugin::parse_installed_plugins(claude_home: &Path) -> Vec<PluginInstallRecord>`
  - `discovery::plugin::discover_plugin_skills(record: &PluginInstallRecord) -> (Vec<DiscoveredSkill>, Vec<DiscoveryWarning>)`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/adapters/claude_code/discovery/plugin.rs`:

```rust
use crate::adapters::claude_code::discovery::scan::discover_skills_in_dir;
use crate::adapters::claude_code::paths::installed_plugins_path;
use crate::domain::skill::{DiscoveredSkill, DiscoveryWarning, InstallScope, SkillId};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct InstalledPluginsFile {
    plugins: HashMap<String, Vec<InstallRecordRaw>>,
}

#[derive(Debug, Deserialize)]
struct InstallRecordRaw {
    scope: String,
    #[serde(rename = "installPath")]
    install_path: String,
}

#[derive(Debug, Clone)]
pub struct PluginInstallRecord {
    pub plugin_at_marketplace: String,
    pub plugin: String,
    pub marketplace: String,
    pub scope: InstallScope,
    pub install_path: PathBuf,
}

/// Reads `installed_plugins.json` verbatim. Never reconstructs `installPath`
/// from `cache/<marketplace>/<plugin>/<version>/` -- a real version
/// directory can be named `unknown` (ADR 0014's neighbor decision).
pub fn parse_installed_plugins(claude_home: &Path) -> Vec<PluginInstallRecord> {
    let path = installed_plugins_path(claude_home);
    let Ok(content) = fs::read_to_string(&path) else { return Vec::new() };
    let Ok(parsed) = serde_json::from_str::<InstalledPluginsFile>(&content) else { return Vec::new() };

    parsed
        .plugins
        .into_iter()
        .flat_map(|(key, records)| {
            let (plugin, marketplace) = split_plugin_key(&key);
            records.into_iter().filter_map(move |r| {
                let scope = parse_scope(&r.scope)?;
                Some(PluginInstallRecord {
                    plugin_at_marketplace: key.clone(),
                    plugin: plugin.clone(),
                    marketplace: marketplace.clone(),
                    scope,
                    install_path: PathBuf::from(r.install_path),
                })
            })
        })
        .collect()
}

fn split_plugin_key(key: &str) -> (String, String) {
    match key.split_once('@') {
        Some((plugin, marketplace)) => (plugin.to_string(), marketplace.to_string()),
        None => (key.to_string(), String::new()),
    }
}

fn parse_scope(raw: &str) -> Option<InstallScope> {
    match raw {
        "user" => Some(InstallScope::User),
        "project" => Some(InstallScope::Project),
        "local" => Some(InstallScope::Local),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct PluginManifest {
    skills: Option<String>,
}

/// Honors `plugin.json`'s own relocation field instead of assuming `skills/`.
fn resolve_skills_dir(install_path: &Path) -> PathBuf {
    let manifest_path = install_path.join("plugin.json");
    let relocated = fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|c| serde_json::from_str::<PluginManifest>(&c).ok())
        .and_then(|m| m.skills);

    match relocated {
        Some(rel) => install_path.join(rel),
        None => install_path.join("skills"),
    }
}

pub fn discover_plugin_skills(record: &PluginInstallRecord) -> (Vec<DiscoveredSkill>, Vec<DiscoveryWarning>) {
    if !record.install_path.exists() {
        return (
            Vec::new(),
            vec![DiscoveryWarning {
                path: record.install_path.clone(),
                reason: format!("installPath for {} does not exist on disk", record.plugin_at_marketplace),
            }],
        );
    }

    let skills_dir = resolve_skills_dir(&record.install_path);
    let marketplace = record.marketplace.clone();
    let plugin = record.plugin.clone();
    discover_skills_in_dir(&skills_dir, move |name| SkillId::Plugin {
        marketplace: marketplace.clone(),
        plugin: plugin.clone(),
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_installed_plugins(claude_home: &Path, body: &str) {
        fs::create_dir_all(claude_home.join("plugins")).unwrap();
        fs::write(claude_home.join("plugins").join("installed_plugins.json"), body).unwrap();
    }

    #[test]
    fn parses_install_path_verbatim_even_when_version_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        write_installed_plugins(
            claude_home,
            r#"{
                "version": 2,
                "plugins": {
                    "serena@claude-plugins-official": [
                        {
                            "scope": "user",
                            "installPath": "/Users/test/.claude/plugins/cache/claude-plugins-official/serena/unknown",
                            "version": "unknown",
                            "installedAt": "2025-12-27T13:20:09.785Z"
                        }
                    ]
                }
            }"#,
        );

        let records = parse_installed_plugins(claude_home);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].plugin, "serena");
        assert_eq!(records[0].marketplace, "claude-plugins-official");
        assert_eq!(records[0].scope, InstallScope::User);
        assert_eq!(
            records[0].install_path,
            PathBuf::from("/Users/test/.claude/plugins/cache/claude-plugins-official/serena/unknown")
        );
    }

    #[test]
    fn multiple_install_records_for_one_plugin_key_are_all_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        write_installed_plugins(
            claude_home,
            r#"{
                "version": 2,
                "plugins": {
                    "foo@bar": [
                        {"scope": "user", "installPath": "/a", "version": "1.0.0"},
                        {"scope": "project", "installPath": "/b", "version": "1.0.0"}
                    ]
                }
            }"#,
        );

        let records = parse_installed_plugins(claude_home);
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|r| r.scope == InstallScope::User));
        assert!(records.iter().any(|r| r.scope == InstallScope::Project));
    }

    #[test]
    fn missing_registry_file_yields_no_records() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(parse_installed_plugins(tmp.path()).is_empty());
    }

    #[test]
    fn discovers_skills_at_install_path_honoring_manifest_relocation() {
        let tmp = tempfile::tempdir().unwrap();
        let install_path = tmp.path().join("plugin-install");
        fs::create_dir_all(&install_path).unwrap();
        fs::write(install_path.join("plugin.json"), r#"{"skills": "./.claude/skills"}"#).unwrap();
        let skill_dir = install_path.join(".claude").join("skills").join("reviewer");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: reviewer\ndescription: reviews code\n---\n\nBody.\n",
        )
        .unwrap();

        let record = PluginInstallRecord {
            plugin_at_marketplace: "test-plugin@test-market".to_string(),
            plugin: "test-plugin".to_string(),
            marketplace: "test-market".to_string(),
            scope: InstallScope::User,
            install_path: install_path.clone(),
        };

        let (skills, warnings) = discover_plugin_skills(&record);

        assert_eq!(skills.len(), 1);
        assert!(warnings.is_empty());
        match &skills[0].id {
            SkillId::Plugin { marketplace, plugin, name } => {
                assert_eq!(marketplace, "test-market");
                assert_eq!(plugin, "test-plugin");
                assert_eq!(name, "reviewer");
            }
            other => panic!("expected Plugin id, got {other:?}"),
        }
    }

    #[test]
    fn missing_install_path_is_skipped_and_warned() {
        let tmp = tempfile::tempdir().unwrap();
        let record = PluginInstallRecord {
            plugin_at_marketplace: "ghost@nowhere".to_string(),
            plugin: "ghost".to_string(),
            marketplace: "nowhere".to_string(),
            scope: InstallScope::User,
            install_path: tmp.path().join("does-not-exist"),
        };

        let (skills, warnings) = discover_plugin_skills(&record);

        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("does not exist on disk"));
    }
}
```

`InstallScope` needs `PartialEq` for these tests — already derived in Task 2 (`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`).

Modify `src-tauri/src/adapters/claude_code/discovery/mod.rs`:

```rust
pub mod personal;
pub mod plugin;
pub mod project;
pub mod scan;
pub mod transcript;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::plugin`
Expected: FAIL to compile before `pub mod plugin;` is added.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::discovery::plugin`
Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/adapters/claude_code/discovery/plugin.rs src-tauri/src/adapters/claude_code/discovery/mod.rs
git commit -m "feat: add plugin registry parsing and plugin skill discovery"
```

---

### Task 10: Plugin enablement — OR-merge across global/project/local settings

**Files:**
- Create: `src-tauri/src/adapters/claude_code/settings.rs`
- Modify: `src-tauri/src/adapters/claude_code/mod.rs`

**Interfaces:**
- Consumes: `paths::{global_settings_path, repo_settings_path, repo_local_settings_path}` (Task 4)
- Produces: `settings::is_plugin_live(plugin_at_marketplace: &str, claude_home: &Path, active_repo_path: Option<&Path>) -> bool`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/adapters/claude_code/settings.rs`:

```rust
use crate::adapters::claude_code::paths::{
    global_settings_path, repo_local_settings_path, repo_settings_path,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Default)]
struct SettingsFile {
    #[serde(rename = "enabledPlugins", default)]
    enabled_plugins: HashMap<String, bool>,
}

fn read_enabled_plugins(path: &Path) -> HashMap<String, bool> {
    fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str::<SettingsFile>(&c).ok())
        .map(|s| s.enabled_plugins)
        .unwrap_or_default()
}

/// A plugin is live if enabled in any applicable scope: global settings,
/// plus the active repo's project and local settings when a repo is active.
/// An OR across sources, not a precedence order (ADR 0015).
pub fn is_plugin_live(plugin_at_marketplace: &str, claude_home: &Path, active_repo_path: Option<&Path>) -> bool {
    let global = read_enabled_plugins(&global_settings_path(claude_home));
    if global.get(plugin_at_marketplace).copied().unwrap_or(false) {
        return true;
    }

    if let Some(repo) = active_repo_path {
        let project = read_enabled_plugins(&repo_settings_path(repo));
        if project.get(plugin_at_marketplace).copied().unwrap_or(false) {
            return true;
        }

        let local = read_enabled_plugins(&repo_local_settings_path(repo));
        if local.get(plugin_at_marketplace).copied().unwrap_or(false) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_settings(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn live_when_enabled_globally() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        write_settings(
            &global_settings_path(claude_home),
            r#"{"enabledPlugins": {"foo@bar": true}}"#,
        );

        assert!(is_plugin_live("foo@bar", claude_home, None));
    }

    #[test]
    fn live_when_enabled_only_in_active_repo_project_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let repo = tmp.path().join("repo");
        write_settings(&global_settings_path(claude_home), r#"{"enabledPlugins": {}}"#);
        write_settings(
            &repo_settings_path(&repo),
            r#"{"enabledPlugins": {"foo@bar": true}}"#,
        );

        assert!(is_plugin_live("foo@bar", claude_home, Some(&repo)));
    }

    #[test]
    fn live_when_enabled_only_in_active_repo_local_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let repo = tmp.path().join("repo");
        write_settings(
            &repo_local_settings_path(&repo),
            r#"{"enabledPlugins": {"foo@bar": true}}"#,
        );

        assert!(is_plugin_live("foo@bar", claude_home, Some(&repo)));
    }

    #[test]
    fn not_live_when_enabled_nowhere_applicable() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let repo = tmp.path().join("repo");

        assert!(!is_plugin_live("foo@bar", claude_home, Some(&repo)));
    }

    #[test]
    fn project_scoped_enable_in_a_different_non_active_repo_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path();
        let other_repo = tmp.path().join("other-repo");
        write_settings(
            &repo_settings_path(&other_repo),
            r#"{"enabledPlugins": {"foo@bar": true}}"#,
        );

        // active repo is a *different* repo than the one with the enable
        let active_repo = tmp.path().join("active-repo");
        assert!(!is_plugin_live("foo@bar", claude_home, Some(&active_repo)));
    }
}
```

Modify `src-tauri/src/adapters/claude_code/mod.rs`:

```rust
pub mod discovery;
pub mod frontmatter;
pub mod paths;
pub mod settings;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::settings`
Expected: FAIL to compile before `pub mod settings;` is added.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::settings`
Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/adapters/claude_code/settings.rs src-tauri/src/adapters/claude_code/mod.rs
git commit -m "feat: add plugin enablement OR-merge across global/project/local settings"
```

---

### Task 11: `ClaudeCodeAdapter` — assemble all three sources

**Files:**
- Modify: `src-tauri/src/adapters/claude_code/mod.rs`

**Interfaces:**
- Consumes: `discovery::personal::discover_personal_skills`, `discovery::project::discover_project_skills`, `discovery::plugin::{parse_installed_plugins, discover_plugin_skills}`, `discovery::transcript::find_active_repo`, `settings::is_plugin_live` (Tasks 6–10)
- Produces:
  - `ClaudeCodeAdapter` struct: `claude_home: PathBuf`
  - `ClaudeCodeAdapter::new(claude_home: PathBuf) -> Self`
  - `ClaudeCodeAdapter::discover_skills(&self) -> DiscoveryResult`
  - `DiscoveryResult` struct: `skills: Vec<DiscoveredSkill>`, `warnings: Vec<DiscoveryWarning>`

- [ ] **Step 1: Write the failing test**

Modify `src-tauri/src/adapters/claude_code/mod.rs` to its final form:

```rust
pub mod discovery;
pub mod frontmatter;
pub mod paths;
pub mod settings;

use crate::domain::skill::{DiscoveredSkill, DiscoveryWarning};
use discovery::plugin::{discover_plugin_skills, parse_installed_plugins};
use discovery::project::discover_project_skills;
use discovery::transcript::find_active_repo;
use settings::is_plugin_live;
use std::path::PathBuf;

pub struct ClaudeCodeAdapter {
    pub claude_home: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    pub skills: Vec<DiscoveredSkill>,
    pub warnings: Vec<DiscoveryWarning>,
}

impl ClaudeCodeAdapter {
    pub fn new(claude_home: PathBuf) -> Self {
        Self { claude_home }
    }

    pub fn discover_skills(&self) -> DiscoveryResult {
        let mut result = DiscoveryResult::default();

        let (personal_skills, personal_warnings) =
            discovery::personal::discover_personal_skills(&self.claude_home);
        result.skills.extend(personal_skills);
        result.warnings.extend(personal_warnings);

        for (_repo, repo_skills, repo_warnings) in discover_project_skills(&self.claude_home) {
            result.skills.extend(repo_skills);
            result.warnings.extend(repo_warnings);
        }

        let active_repo = find_active_repo(&self.claude_home);
        let active_repo_path = active_repo.as_ref().map(|r| r.repo_path.as_path());

        for record in parse_installed_plugins(&self.claude_home) {
            let live = is_plugin_live(&record.plugin_at_marketplace, &self.claude_home, active_repo_path);
            let (plugin_skills, plugin_warnings) = discover_plugin_skills(&record);
            result
                .skills
                .extend(plugin_skills.into_iter().map(|s| DiscoveredSkill { live, ..s }));
            result.warnings.extend(plugin_warnings);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::skill::SkillId;
    use std::fs;

    fn write_skill(dir: &std::path::Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: a test skill\n---\n\nBody.\n"),
        )
        .unwrap();
    }

    #[test]
    fn assembles_personal_project_and_plugin_skills_together() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");

        // Personal skill
        write_skill(&claude_home.join("skills").join("personal-one"), "personal-one");

        // Project skill (via a known repo)
        let repo = tmp.path().join("repo");
        let project_dir = claude_home.join("projects").join("-tmp-repo");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("s.jsonl"),
            format!(r#"{{"cwd":"{}","sessionId":"1"}}"#, repo.display()),
        )
        .unwrap();
        write_skill(&repo.join(".claude").join("skills").join("project-one"), "project-one");

        // Plugin skill, enabled globally
        let plugin_install = tmp.path().join("plugin-cache").join("test-plugin").join("1.0.0");
        write_skill(&plugin_install.join("skills").join("plugin-one"), "plugin-one");
        fs::create_dir_all(claude_home.join("plugins")).unwrap();
        fs::write(
            claude_home.join("plugins").join("installed_plugins.json"),
            format!(
                r#"{{"version": 2, "plugins": {{"test-plugin@test-market": [{{"scope": "user", "installPath": "{}", "version": "1.0.0"}}]}}}}"#,
                plugin_install.display()
            ),
        )
        .unwrap();
        fs::write(
            claude_home.join("settings.json"),
            r#"{"enabledPlugins": {"test-plugin@test-market": true}}"#,
        )
        .unwrap();

        let adapter = ClaudeCodeAdapter::new(claude_home);
        let result = adapter.discover_skills();

        assert_eq!(result.skills.len(), 3);
        assert!(result.warnings.is_empty());

        let personal = result
            .skills
            .iter()
            .find(|s| matches!(&s.id, SkillId::Personal { name } if name == "personal-one"))
            .unwrap();
        assert!(personal.live);

        let project = result
            .skills
            .iter()
            .find(|s| matches!(&s.id, SkillId::Project { name, .. } if name == "project-one"))
            .unwrap();
        assert!(project.live);

        let plugin = result
            .skills
            .iter()
            .find(|s| matches!(&s.id, SkillId::Plugin { name, .. } if name == "plugin-one"))
            .unwrap();
        assert!(plugin.live);
    }

    #[test]
    fn plugin_not_enabled_anywhere_applicable_is_discovered_but_not_live() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");

        let plugin_install = tmp.path().join("plugin-cache").join("dormant-plugin").join("1.0.0");
        write_skill(&plugin_install.join("skills").join("dormant-skill"), "dormant-skill");
        fs::create_dir_all(claude_home.join("plugins")).unwrap();
        fs::write(
            claude_home.join("plugins").join("installed_plugins.json"),
            format!(
                r#"{{"version": 2, "plugins": {{"dormant-plugin@test-market": [{{"scope": "user", "installPath": "{}", "version": "1.0.0"}}]}}}}"#,
                plugin_install.display()
            ),
        )
        .unwrap();
        // No settings.json at all -- nothing enabled anywhere.

        let adapter = ClaudeCodeAdapter::new(claude_home);
        let result = adapter.discover_skills();

        assert_eq!(result.skills.len(), 1);
        assert!(!result.skills[0].live);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::tests`
Expected: FAIL to compile before this edit is applied (the `ClaudeCodeAdapter` type doesn't exist yet).

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml adapters::claude_code::tests`
Expected: 2 tests pass.

- [ ] **Step 4: Run the full suite**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: every test across all modules from Tasks 2–11 passes (30+ tests total).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/adapters/claude_code/mod.rs
git commit -m "feat: assemble personal, project, and plugin discovery into ClaudeCodeAdapter"
```

---

## Self-Review Notes

**Spec coverage** — checked against `src-tauri/CONTEXT.md` and ADRs 0002, 0014, 0015 (the ADRs this plan implements; footprint-related ADRs 0006/0016–0019 are the follow-up plan's concern):
- Skill identity keyed by directory name, not frontmatter name, never plugin version → Task 2, Task 5's `make_id` pattern.
- Directory-name/declared-name mismatch surfaced, not resolved → Task 2's `name_mismatch()`.
- Malformed/missing `SKILL.md` skipped + warned, not fatal → Task 5.
- Repo path and active repo from real transcript `cwd`, never decoded directory name → Task 7, with a test specifically constructed around a hyphenated-path ambiguity.
- Plugin `installPath` read verbatim, never reconstructed → Task 9, with a test using a literal `"unknown"` version directory.
- `plugin.json` skills-dir relocation honored → Task 9.
- Plugin enablement OR-merged across global/project/local, active-repo-gated → Task 10.
- Symlinked personal/project skills detected, target recorded → Task 5's symlink test.
- Bundled on-demand files enumerated → Task 5.
- Harness-adapter boundary: all Claude-Code-specific logic under `adapters::claude_code`, domain types generic → file structure throughout.

**Not in this plan** (explicitly out of scope, follow-up plan): footprint computation (always-on/on-invoke/on-demand text sourcing and tokenization), the `HarnessAdapter` trait (deferred until `compute_footprint` exists alongside `discover_skills`, so the trait isn't introduced with only one of its two methods real), file-watching/rescan wiring (ADR 0019), mutation operations (ADR 0007), attributed usage (ADR 0005), and all UI work.

**Placeholder scan** — no TODOs, no stub methods, no "add error handling" prose; every step has complete, compiling code.

**Type consistency** — `DiscoveredSkill`, `DiscoveryWarning`, `SkillId`, `InstallScope`, `Frontmatter` are defined once in Task 2 and referenced identically by name and field in every later task; `PluginInstallRecord`, `RepoInfo`, `DiscoveryResult` are each defined once and consumed with matching signatures downstream.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-02-skill-discovery.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

**Which approach?**
