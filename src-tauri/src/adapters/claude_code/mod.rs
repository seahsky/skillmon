pub mod discovery;
pub mod footprint_text;
pub mod frontmatter;
pub mod paths;
pub mod settings;
pub mod watcher;

use crate::domain::footprint::{AlwaysOnFootprint, Footprint, LayerCount, TokenSource};
use crate::domain::harness::HarnessAdapter;
use crate::domain::report::{ScanReport, SkillReport};
use crate::domain::skill::{DiscoveredSkill, DiscoveryResult, SkillId};
use crate::footprint::api_key_store::ApiKeyStore;
use crate::footprint::cache::TokenCache;
use crate::footprint::compute::count_text;
use crate::footprint::count_tokens_client::CountTokensClient;
use discovery::plugin::{discover_plugin_skills, parse_installed_plugins};
use discovery::project::discover_project_skills;
use discovery::transcript::{enumerate_known_repos, find_active_repo, RepoInfo};
use footprint_text::{always_on_text_from_index, transcript_refs_by_recency, AlwaysOnText, ListingIndex};
use settings::is_plugin_live;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// ADR 0018: the single fixed model `count_tokens` is called against,
/// internal-only, never surfaced to the user.
const REFERENCE_MODEL_ID: &str = "claude-sonnet-5";

pub struct ClaudeCodeAdapter {
    pub claude_home: PathBuf,
    cache: TokenCache,
    api_key_store: Box<dyn ApiKeyStore>,
    client: Box<dyn CountTokensClient>,
}

impl ClaudeCodeAdapter {
    /// Callers construct the cache/key-store/client at the composition root
    /// (e.g. Tauri's `setup` hook) and hand them in already built, so this
    /// constructor stays infallible -- any I/O error in opening the cache or
    /// resolving the keychain entry surfaces where it's actually handled.
    pub fn new(
        claude_home: PathBuf,
        cache: TokenCache,
        api_key_store: Box<dyn ApiKeyStore>,
        client: Box<dyn CountTokensClient>,
    ) -> Self {
        Self { claude_home, cache, api_key_store, client }
    }

    /// Convenience for tests that only exercise `discover_skills` and don't
    /// care about footprint wiring -- an in-memory cache and fakes that
    /// never get called.
    #[cfg(test)]
    pub fn for_discovery_only(claude_home: PathBuf) -> Self {
        Self::new(
            claude_home,
            TokenCache::open_in_memory().unwrap(),
            Box::new(crate::footprint::api_key_store::FakeApiKeyStore::empty()),
            Box::new(crate::footprint::count_tokens_client::FakeCountTokensClient::always_returns(0)),
        )
    }

    /// Personal and plugin skills can render in any repo's session, so their
    /// always-on text is searched for broadly; a project skill can only ever
    /// render in its own repo's sessions, so restricting the search avoids a
    /// same-named project skill in a different repo producing a false match.
    /// Plugin skills don't carry install scope on `DiscoveredSkill`, so they
    /// search broadly too -- safe, since a scope-restricted plugin simply
    /// won't appear in an unrelated repo's transcripts to begin with.
    fn always_on_search_dirs(&self, skill: &DiscoveredSkill, known_repos: &[RepoInfo]) -> Vec<PathBuf> {
        match &skill.id {
            SkillId::Project { repo_path, .. } => known_repos
                .iter()
                .filter(|r| &r.repo_path == repo_path)
                .map(|r| r.project_dir.clone())
                .collect(),
            SkillId::Personal { .. } | SkillId::Plugin { .. } => {
                known_repos.iter().map(|r| r.project_dir.clone()).collect()
            }
        }
    }

    pub fn discover_skills(&self) -> DiscoveryResult {
        let mut result = DiscoveryResult::default();

        let (personal_skills, personal_warnings) =
            discovery::personal::discover_personal_skills(&self.claude_home);
        result.skills.extend(personal_skills);
        result.warnings.extend(personal_warnings);

        // Computed once, up front, so both the project loop and the plugin
        // loop below can gate liveness against the same active repo.
        let active_repo_path = find_active_repo(&self.claude_home).map(|r| r.repo_path);
        result.active_repo_path = active_repo_path.clone();

        for (repo, repo_skills, repo_warnings) in discover_project_skills(&self.claude_home) {
            // A project skill is only live when its repo is the active one;
            // non-active repos' project skills are still discovered, just
            // not counted as co-resident (DESIGN.md UX decision #5).
            let live = active_repo_path.as_deref() == Some(repo.repo_path.as_path());
            result
                .skills
                .extend(repo_skills.into_iter().map(|s| DiscoveredSkill { live, ..s }));
            result.warnings.extend(repo_warnings);
        }

        // A plugin key can have multiple install records (one per scope: user/project/local),
        // but every scope's files live in the same shared cache -- there is no repo-local
        // cache directory (docs/DESIGN.md). Dedupe by `plugin_at_marketplace` before
        // discovering skills so a multi-scope install doesn't produce duplicate skill rows.
        let mut unique_records: HashMap<String, discovery::plugin::PluginInstallRecord> = HashMap::new();
        let (installed_plugin_records, installed_plugins_warnings) = parse_installed_plugins(&self.claude_home);
        result.warnings.extend(installed_plugins_warnings);
        for record in installed_plugin_records {
            unique_records
                .entry(record.plugin_at_marketplace.clone())
                .or_insert(record);
        }

        for record in unique_records.values() {
            let live = is_plugin_live(
                &record.plugin_at_marketplace,
                &self.claude_home,
                active_repo_path.as_deref(),
            );
            let (plugin_skills, plugin_warnings) = discover_plugin_skills(record);
            result
                .skills
                .extend(plugin_skills.into_iter().map(|s| DiscoveredSkill { live, ..s }));
            result.warnings.extend(plugin_warnings);
        }

        result
    }

    /// Single-skill footprint: reads transcripts directly for the always-on
    /// text. Fine for a one-off recompute, but a full scan uses the batched
    /// `scan_all` override instead, which reads each transcript once rather
    /// than once per skill.
    pub fn compute_footprint(&self, skill: &DiscoveredSkill) -> Footprint {
        let known_repos = enumerate_known_repos(&self.claude_home);
        let search_dirs = self.always_on_search_dirs(skill, &known_repos);
        let always_on = footprint_text::always_on_text(skill, &search_dirs);
        self.footprint_with_always_on(skill, always_on)
    }

    /// Counts the on-invoke and on-demand layers (which never touch
    /// transcripts) and combines them with an already-resolved always-on text.
    /// Shared by the single-skill and batched-scan paths so they can't drift.
    fn footprint_with_always_on(&self, skill: &DiscoveredSkill, always_on: AlwaysOnText) -> Footprint {
        let always_on_count = self.count(&always_on.text);
        let on_invoke_count = self.count(&footprint_text::on_invoke_text(skill));
        let on_demand_count = sum_layer_counts(
            footprint_text::on_demand_file_texts(skill).into_iter().map(|(_, text)| self.count(&text)),
        );
        Footprint {
            always_on: AlwaysOnFootprint { count: always_on_count, confidence: always_on.confidence },
            on_invoke: on_invoke_count,
            on_demand: on_demand_count,
        }
    }

    fn count(&self, text: &str) -> LayerCount {
        count_text(text, &self.cache, self.api_key_store.as_ref(), self.client.as_ref(), REFERENCE_MODEL_ID)
    }

    /// Reconciles the registry watcher's path set against the repos currently
    /// on disk (ADR 0019). Called once at startup and after every rescan, so
    /// a repo that gained a `.claude/skills/` dir since launch starts being
    /// watched. Keeps all `RepoInfo`/path knowledge inside the adapter (ADR
    /// 0002) rather than leaking it to the composition root.
    pub fn sync_watcher(&self, watcher: &mut watcher::RegistryWatcher) {
        let known_repos = enumerate_known_repos(&self.claude_home);
        let active_repo = find_active_repo(&self.claude_home);
        watcher.sync(&self.claude_home, &known_repos, active_repo.as_ref());
    }

    /// How many cached exact counts were measured against a reference model
    /// other than the current one (ADR 0018) -- i.e. skillmon bumped its
    /// internal default since they were stored. A neutral count, not a
    /// promise: `count_text` already declines to trust a stale exact and
    /// re-counts it on the next `scan_all` when a key is present, so the
    /// startup path uses this only to decide whether that recount is worth
    /// kicking off eagerly. Never surfaces the model id itself (ADR 0018).
    pub fn stale_exact_count(&self) -> usize {
        self.cache.stale_exact_hashes(REFERENCE_MODEL_ID).len()
    }
}

impl HarnessAdapter for ClaudeCodeAdapter {
    fn discover_skills(&self) -> DiscoveryResult {
        ClaudeCodeAdapter::discover_skills(self)
    }

    fn compute_footprint(&self, skill: &DiscoveredSkill) -> Footprint {
        ClaudeCodeAdapter::compute_footprint(self, skill)
    }

    /// Overrides the trait's naive per-skill default (which would re-read
    /// every transcript once per skill -- tens of GB on a real machine) with a
    /// batched pass: enumerate repos once, build the skill-listing index once
    /// over exactly the discovered skill names, then resolve each skill's
    /// always-on text from that index with the same type-scoped search dirs
    /// the single-skill path uses. Result is identical, cost drops from
    /// O(skills × transcripts) reads to O(transcripts).
    fn scan_all(&self) -> ScanReport {
        let discovery = ClaudeCodeAdapter::discover_skills(self);
        let known_repos = enumerate_known_repos(&self.claude_home);

        let all_project_dirs: Vec<PathBuf> = known_repos.iter().map(|r| r.project_dir.clone()).collect();
        let wanted: HashSet<String> =
            discovery.skills.iter().map(|s| s.directory_name().to_string()).collect();
        let transcripts = transcript_refs_by_recency(&all_project_dirs);
        let index = ListingIndex::build(&transcripts, &wanted);

        let skills = discovery
            .skills
            .iter()
            .map(|skill| {
                let search_dirs = self.always_on_search_dirs(skill, &known_repos);
                let always_on = always_on_text_from_index(skill, &index, &search_dirs);
                let footprint = self.footprint_with_always_on(skill, always_on);
                SkillReport::from_parts(skill, &footprint)
            })
            .collect();
        let warnings = discovery
            .warnings
            .iter()
            .map(|w| format!("{}: {}", w.path.display(), w.reason))
            .collect();

        ScanReport {
            skills,
            warnings,
            active_repo_path: discovery.active_repo_path.map(|p| p.display().to_string()),
        }
    }
}

/// On-demand is a single ceiling number, not a per-file breakdown (ADR
/// 0017). Any estimated component makes the sum an estimate too -- a total
/// is only as trustworthy as its least-exact part.
fn sum_layer_counts(counts: impl Iterator<Item = LayerCount>) -> LayerCount {
    let mut tokens = 0u32;
    let mut source = TokenSource::Exact;
    for count in counts {
        tokens += count.tokens;
        if count.source == TokenSource::Estimate {
            source = TokenSource::Estimate;
        }
    }
    LayerCount { tokens, source }
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

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
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

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let result = adapter.discover_skills();

        assert_eq!(result.skills.len(), 1);
        assert!(!result.skills[0].live);
    }

    #[test]
    fn multi_scope_plugin_install_records_are_discovered_only_once() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");

        // A single shared cache directory -- both the "user" and "project" scope
        // records below point at the same installPath, matching reality: every
        // scope's files live in the same shared cache (docs/DESIGN.md).
        let plugin_install = tmp.path().join("plugin-cache").join("multi-scope-plugin").join("1.0.0");
        write_skill(&plugin_install.join("skills").join("multi-scope-skill"), "multi-scope-skill");
        fs::create_dir_all(claude_home.join("plugins")).unwrap();
        fs::write(
            claude_home.join("plugins").join("installed_plugins.json"),
            format!(
                r#"{{"version": 2, "plugins": {{"multi-scope-plugin@test-market": [
                    {{"scope": "user", "installPath": "{path}", "version": "1.0.0"}},
                    {{"scope": "project", "installPath": "{path}", "version": "1.0.0"}}
                ]}}}}"#,
                path = plugin_install.display()
            ),
        )
        .unwrap();
        fs::write(
            claude_home.join("settings.json"),
            r#"{"enabledPlugins": {"multi-scope-plugin@test-market": true}}"#,
        )
        .unwrap();

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let result = adapter.discover_skills();

        let matches: Vec<_> = result
            .skills
            .iter()
            .filter(|s| matches!(&s.id, SkillId::Plugin { name, .. } if name == "multi-scope-skill"))
            .collect();
        assert_eq!(matches.len(), 1, "expected exactly one discovered skill, got {matches:?}");
        assert!(matches[0].live);
    }

    #[test]
    fn project_skill_liveness_is_gated_by_active_repo() {
        use std::thread::sleep;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");

        // Older known repo, written first.
        let older_repo = tmp.path().join("older-repo");
        let older_project_dir = claude_home.join("projects").join("-tmp-older-repo");
        fs::create_dir_all(&older_project_dir).unwrap();
        fs::write(
            older_project_dir.join("s.jsonl"),
            format!(r#"{{"cwd":"{}","sessionId":"1"}}"#, older_repo.display()),
        )
        .unwrap();
        write_skill(&older_repo.join(".claude").join("skills").join("older-skill"), "older-skill");

        sleep(Duration::from_millis(20));

        // Newer known repo, written after -- this is the active one.
        let newer_repo = tmp.path().join("newer-repo");
        let newer_project_dir = claude_home.join("projects").join("-tmp-newer-repo");
        fs::create_dir_all(&newer_project_dir).unwrap();
        fs::write(
            newer_project_dir.join("s.jsonl"),
            format!(r#"{{"cwd":"{}","sessionId":"1"}}"#, newer_repo.display()),
        )
        .unwrap();
        write_skill(&newer_repo.join(".claude").join("skills").join("newer-skill"), "newer-skill");

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let result = adapter.discover_skills();

        assert_eq!(result.active_repo_path.as_deref(), Some(newer_repo.as_path()));

        let older_skill = result
            .skills
            .iter()
            .find(|s| matches!(&s.id, SkillId::Project { name, .. } if name == "older-skill"))
            .unwrap();
        assert!(!older_skill.live, "non-active repo's project skill must still be discovered but not live");

        let newer_skill = result
            .skills
            .iter()
            .find(|s| matches!(&s.id, SkillId::Project { name, .. } if name == "newer-skill"))
            .unwrap();
        assert!(newer_skill.live, "active repo's project skill must be live");
    }

    #[test]
    fn compute_footprint_assembles_all_three_layers_end_to_end() {
        use crate::domain::footprint::TextConfidence;
        use crate::footprint::tokenizer::estimate_tokens;

        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");

        let skill_dir = claude_home.join("skills").join("grilling");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: grilling\ndescription: Interview relentlessly.\n---\n\nInterview the user about every aspect of the plan.\n",
        )
        .unwrap();
        fs::write(skill_dir.join("REFERENCE.md"), "supplementary reference material").unwrap();

        // A transcript that both registers a known repo (the `cwd` field)
        // and rendered this skill's bullet, so always-on comes back Native.
        let project_dir = claude_home.join("projects").join("-tmp-repo");
        fs::create_dir_all(&project_dir).unwrap();
        let record = serde_json::json!({
            "type": "attachment",
            "cwd": "/tmp/some-repo",
            "sessionId": "abc",
            "attachment": {
                "type": "skill_listing",
                "content": "- grilling: Interview the user relentlessly.\n- other-skill: does other things",
                "names": ["grilling", "other-skill"],
                "skillCount": 2,
                "isInitial": true
            }
        });
        fs::write(project_dir.join("session1.jsonl"), format!("{record}\n")).unwrap();

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let discovery = adapter.discover_skills();
        let skill = discovery.skills.iter().find(|s| s.directory_name() == "grilling").unwrap();

        let footprint = adapter.compute_footprint(skill);

        assert_eq!(footprint.always_on.confidence, TextConfidence::Native);
        assert_eq!(footprint.always_on.count.source, TokenSource::Estimate);
        assert_eq!(footprint.always_on.count.tokens, estimate_tokens("- grilling: Interview the user relentlessly."));

        let expected_on_invoke = format!(
            "Base directory for this skill: {}\n\nInterview the user about every aspect of the plan.\n",
            skill.dir_path.display()
        );
        assert_eq!(footprint.on_invoke.source, TokenSource::Estimate);
        assert_eq!(footprint.on_invoke.tokens, estimate_tokens(&expected_on_invoke));

        assert_eq!(footprint.on_demand.source, TokenSource::Estimate);
        assert_eq!(footprint.on_demand.tokens, estimate_tokens("supplementary reference material"));
    }

    #[test]
    fn scan_all_bundles_every_discovered_skill_with_its_footprint() {
        use crate::domain::report::SkillKind;

        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill(&claude_home.join("skills").join("alpha"), "alpha");
        write_skill(&claude_home.join("skills").join("beta"), "beta");

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all();

        assert_eq!(report.skills.len(), 2);
        assert!(report.skills.iter().all(|s| s.kind == SkillKind::Personal));
        let names: Vec<&str> = report.skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        // No key configured, so every layer is the estimate tier.
        assert!(report.skills.iter().all(|s| !s.always_on.exact && !s.on_invoke.exact));
    }

    #[test]
    fn batched_scan_all_resolves_native_always_on_like_the_per_skill_path() {
        use crate::domain::footprint::TextConfidence;

        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");

        let skill_dir = claude_home.join("skills").join("grilling");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: grilling\ndescription: Interview relentlessly.\n---\n\nBody.\n",
        )
        .unwrap();

        // A transcript that both registers a known repo and rendered the skill's
        // bullet, so the batched index path must return Native, not Reconstructed.
        let project_dir = claude_home.join("projects").join("-tmp-repo");
        fs::create_dir_all(&project_dir).unwrap();
        let record = serde_json::json!({
            "type": "attachment",
            "cwd": "/tmp/some-repo",
            "attachment": {
                "type": "skill_listing",
                "content": "- grilling: Interview the user relentlessly.\n- other: does other things",
                "names": ["grilling", "other"],
            }
        });
        fs::write(project_dir.join("s.jsonl"), format!("{record}\n")).unwrap();

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all();

        let grilling = report.skills.iter().find(|s| s.name == "grilling").unwrap();
        assert!(grilling.always_on_native, "batched scan should source always-on from the transcript (Native)");

        // And it agrees with the single-skill path's confidence + tokens.
        let discovery = adapter.discover_skills();
        let skill = discovery.skills.iter().find(|s| s.directory_name() == "grilling").unwrap();
        let per_skill = adapter.compute_footprint(skill);
        assert_eq!(per_skill.always_on.confidence, TextConfidence::Native);
        assert_eq!(grilling.always_on.tokens, per_skill.always_on.count.tokens);
    }

    #[test]
    fn stale_exact_count_is_zero_on_a_fresh_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill(&claude_home.join("skills").join("alpha"), "alpha");

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        adapter.scan_all();

        // Nothing exact has ever been stored (no key), so nothing is stale.
        assert_eq!(adapter.stale_exact_count(), 0);
    }

    /// Exercises the real production adapter (real keychain store, real HTTP
    /// client, real `default_claude_home()`) against this machine's actual
    /// `~/.claude` -- the CLAUDE.md verification bar for this flow. Not run by
    /// the default suite (it depends on the developer's real home and, if a
    /// key is configured, hits the network). Run by hand:
    /// `cargo test --manifest-path src-tauri/Cargo.toml
    /// adapters::claude_code::tests::scan_all_against_the_real_claude_home -- --ignored --exact --nocapture`
    #[test]
    #[ignore]
    fn scan_all_against_the_real_claude_home() {
        use crate::footprint::api_key_store::KeychainApiKeyStore;
        use crate::footprint::count_tokens_client::AnthropicCountTokensClient;
        use crate::footprint::cache::TokenCache;

        let tmp = tempfile::tempdir().unwrap();
        let adapter = ClaudeCodeAdapter::new(
            paths::default_claude_home(),
            TokenCache::open(&tmp.path().join("footprint.sqlite")).unwrap(),
            Box::new(KeychainApiKeyStore::new().unwrap()),
            Box::new(AnthropicCountTokensClient::new()),
        );

        // Cold scan populates the content-hash cache; the second scan reuses
        // it. The often-quoted "~120s cold" was a *debug-build* artifact of
        // running this ignored test under `cargo test`; in release the same
        // real corpus (216 MB, 72M tokens) tokenizes in ~11s with tiktoken and
        // ~6.5s since the swap to bpe-openai (ADR 0006 update, issue #2). Nearly
        // all of that volume is the on-demand ceiling (bundled reference files),
        // not the skill bodies; deferring on-demand tokenization off the
        // interactive scan is tracked separately as a follow-up issue. The
        // persistent production cache (app data dir, not this test's temp file)
        // still amortizes the cold cost to once-ever per unique content.
        use std::time::Instant;
        let cold = Instant::now();
        let report = adapter.scan_all();
        let cold_elapsed = cold.elapsed();

        let warm = Instant::now();
        let _ = adapter.scan_all();
        let warm_elapsed = warm.elapsed();

        eprintln!(
            "scanned {} skills; active repo: {:?}; {} warnings; {} stale exact counts",
            report.skills.len(),
            report.active_repo_path,
            report.warnings.len(),
            adapter.stale_exact_count(),
        );
        eprintln!("cold scan: {cold_elapsed:?}; warm (cached) scan: {warm_elapsed:?}");
        assert!(
            warm_elapsed < cold_elapsed,
            "the content-hash cache must make a second scan faster than the first"
        );
        for skill in report.skills.iter().take(10) {
            eprintln!(
                "  [{:?}] {:<28} always_on={:>4} (exact={}, native={})  on_invoke={:>5}  on_demand={:>6}",
                skill.kind,
                skill.name,
                skill.always_on.tokens,
                skill.always_on.exact,
                skill.always_on_native,
                skill.on_invoke.tokens,
                skill.on_demand.tokens,
            );
        }

        // The bar: a real machine with skills installed returns a non-empty
        // scan whose always-on layer actually measured something.
        assert!(!report.skills.is_empty(), "expected the real ~/.claude to have at least one skill");
        assert!(
            report.skills.iter().any(|s| s.always_on.tokens > 0),
            "expected at least one skill's always-on layer to count more than zero tokens"
        );
    }
}
