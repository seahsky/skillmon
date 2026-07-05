pub mod discovery;
pub mod footprint_text;
pub mod frontmatter;
pub mod listing_cache;
pub mod on_demand_cache;
pub mod paths;
pub mod settings;
pub mod usage;
pub mod usage_cache;
pub mod watcher;

use crate::domain::footprint::{AlwaysOnFootprint, Footprint, LayerCount, TokenSource};
use crate::domain::harness::HarnessAdapter;
use crate::domain::report::{ScanReport, SkillReport};
use crate::domain::skill::{DiscoveredSkill, DiscoveryResult, SkillId};
use crate::footprint::api_key_store::ApiKeyStore;
use crate::footprint::cache::TokenCache;
use crate::footprint::compute::count_text;
use crate::footprint::count_tokens_client::CountTokensClient;
use crate::footprint::hashing::sha256_hex;
use crate::footprint::tokenizer::Tokenizer;
// Only the test constructors (`for_discovery_only` and the two explicit
// `new(...)` call sites) name the concrete tokenizer; production wires it in
// the `lib.rs` composition root.
#[cfg(test)]
use crate::footprint::tokenizer::BpeTokenizer;
use discovery::plugin::{discover_plugin_skills, parse_installed_plugins};
use discovery::project::discover_project_skills;
use discovery::transcript::{enumerate_known_repos, find_active_repo, RepoInfo};
use footprint_text::{
    always_on_text_from_index, subagent_transcript_refs, transcript_refs_by_recency, AlwaysOnText,
    ListingIndex,
};
use listing_cache::SqliteListingCache;
use on_demand_cache::SqliteOnDemandCache;
use settings::is_plugin_live;
use usage::UsageIndex;
use usage_cache::SqliteUsageCache;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// ADR 0018: the single fixed model `count_tokens` is called against,
/// internal-only, never surfaced to the user. `pub(crate)` so the `set_api_key`
/// command's validation probe (issue #4) uses the same model the counter does.
pub(crate) const REFERENCE_MODEL_ID: &str = "claude-sonnet-5";

pub struct ClaudeCodeAdapter {
    pub claude_home: PathBuf,
    cache: TokenCache,
    listing_cache: SqliteListingCache,
    usage_cache: SqliteUsageCache,
    on_demand_cache: SqliteOnDemandCache,
    api_key_store: Box<dyn ApiKeyStore>,
    client: Box<dyn CountTokensClient>,
    tokenizer: Box<dyn Tokenizer>,
}

impl ClaudeCodeAdapter {
    /// Callers construct the caches/key-store/client at the composition root
    /// (e.g. Tauri's `setup` hook) and hand them in already built, so this
    /// constructor stays infallible -- any I/O error in opening a cache or
    /// resolving the keychain entry surfaces where it's actually handled.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        claude_home: PathBuf,
        cache: TokenCache,
        listing_cache: SqliteListingCache,
        usage_cache: SqliteUsageCache,
        on_demand_cache: SqliteOnDemandCache,
        api_key_store: Box<dyn ApiKeyStore>,
        client: Box<dyn CountTokensClient>,
        tokenizer: Box<dyn Tokenizer>,
    ) -> Self {
        Self {
            claude_home,
            cache,
            listing_cache,
            usage_cache,
            on_demand_cache,
            api_key_store,
            client,
            tokenizer,
        }
    }

    /// Convenience for tests that only exercise `discover_skills` and don't
    /// care about footprint wiring -- in-memory caches and fakes that
    /// never get called.
    #[cfg(test)]
    pub fn for_discovery_only(claude_home: PathBuf) -> Self {
        Self::new(
            claude_home,
            TokenCache::open_in_memory().unwrap(),
            SqliteListingCache::open_in_memory().unwrap(),
            SqliteUsageCache::open_in_memory().unwrap(),
            SqliteOnDemandCache::open_in_memory().unwrap(),
            Box::new(crate::footprint::api_key_store::FakeApiKeyStore::empty()),
            Box::new(crate::footprint::count_tokens_client::FakeCountTokensClient::always_returns(0)),
            Box::new(BpeTokenizer),
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
        // Single-skill recompute stays eager: it is a deliberate one-off (e.g.
        // a UI recount of one row), not the interactive cold scan issue #11
        // defers, so it computes its on-demand ceiling inline.
        self.footprint_with_always_on(skill, always_on, Some(self.compute_on_demand(skill)))
    }

    /// Counts the on-invoke layer (which never touches transcripts) and
    /// combines it with an already-resolved always-on text and an
    /// already-resolved on-demand ceiling. The on-demand argument is
    /// `Option` so the caller decides whether to compute it eagerly (the
    /// single-skill path) or defer it as pending (the interactive scan, issue
    /// #11). Shared by both paths so the on-invoke/always-on halves can't drift.
    fn footprint_with_always_on(
        &self,
        skill: &DiscoveredSkill,
        always_on: AlwaysOnText,
        on_demand: Option<LayerCount>,
    ) -> Footprint {
        let always_on_count = self.count(&always_on.text);
        let on_invoke_count = self.count(&footprint_text::on_invoke_text(skill));
        Footprint {
            always_on: AlwaysOnFootprint { count: always_on_count, confidence: always_on.confidence },
            on_invoke: on_invoke_count,
            on_demand,
        }
    }

    /// The eager on-demand ceiling: reads and tokenizes every bundled file,
    /// summing them (ADR 0017). This is the zero-drift anchor -- the background
    /// pass (issue #11) computes and persists exactly this value, and the
    /// single-skill path calls it inline.
    fn compute_on_demand(&self, skill: &DiscoveredSkill) -> LayerCount {
        compute_on_demand_with(
            skill,
            &self.cache,
            self.api_key_store.as_ref(),
            self.client.as_ref(),
            self.tokenizer.as_ref(),
        )
    }

    /// The interactive-scan on-demand resolution (issue #11): pending by *file
    /// set*, never by cache presence. A skill with no bundled files resolves
    /// immediately to a zero ceiling (matching the old empty sum), never
    /// pending. A skill with bundled files resolves only from a memo hit on
    /// the fresh signature; otherwise it is pending (`None`) and a background
    /// pass will fill it -- never a `Some(0)`, which would flash a wrong ceiling.
    fn cached_on_demand(&self, skill: &DiscoveredSkill) -> Option<LayerCount> {
        if skill.on_demand_files.is_empty() {
            return Some(LayerCount { tokens: 0, source: TokenSource::Exact });
        }
        let (skill_key, signature) = on_demand_signature(skill);
        self.on_demand_cache.get(&skill_key, &signature)
    }

    /// Every discovered skill whose on-demand ceiling is still pending -- the
    /// background pass's worklist (issue #11). Cheap: discovery + file stats +
    /// memo lookups, no tokenization. Runs under the scan `Mutex`; the actual
    /// tokenization happens off the lock on the background's own connections.
    pub fn pending_on_demand(&self) -> Vec<DiscoveredSkill> {
        self.discover_skills()
            .skills
            .into_iter()
            .filter(|skill| self.cached_on_demand(skill).is_none())
            .collect()
    }

    /// Test-only: fill the adapter's OWN on-demand memo (production drives the
    /// free `fill_on_demand_ceilings` against separate background connections).
    /// Lets a test exercise the whole compute-and-persist step on one adapter.
    #[cfg(test)]
    fn fill_on_demand(&self, skills: &[DiscoveredSkill]) -> bool {
        fill_on_demand_ceilings(
            skills,
            &self.cache,
            &self.on_demand_cache,
            self.api_key_store.as_ref(),
            self.client.as_ref(),
            self.tokenizer.as_ref(),
        )
    }

    fn count(&self, text: &str) -> LayerCount {
        count_text(
            text,
            &self.cache,
            self.api_key_store.as_ref(),
            self.client.as_ref(),
            self.tokenizer.as_ref(),
            REFERENCE_MODEL_ID,
        )
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
    ///
    /// `include_subagents` (issue #13) only widens the attributed-usage pass;
    /// the listing index and its retain set stay MAIN-THREAD ONLY regardless
    /// (grill D4), so a sub-agent file's own `skill_listing` can never pollute
    /// always-on or evict the listing memo.
    fn scan_all(&self, include_subagents: bool) -> ScanReport {
        let discovery = ClaudeCodeAdapter::discover_skills(self);
        let known_repos = enumerate_known_repos(&self.claude_home);

        let all_project_dirs: Vec<PathBuf> = known_repos.iter().map(|r| r.project_dir.clone()).collect();
        let wanted: HashSet<String> =
            discovery.skills.iter().map(|s| s.directory_name().to_string()).collect();
        let transcripts = transcript_refs_by_recency(&all_project_dirs);
        let (index, _stats) = ListingIndex::build_incremental(&transcripts, &wanted, &self.listing_cache);
        // Bound the memo: drop rows for transcripts no longer present. Keyed on
        // every transcript this full scan enumerated, so a row is only evicted
        // when its file is genuinely gone (ADR 0022). Safe because scan_all
        // always enumerates the complete set, never a narrowed scope.
        //
        // Skip pruning entirely when the enumeration came back empty: that is
        // far more likely a transient read failure than every transcript
        // vanishing at once, and pruning on it would wipe the memo and re-read
        // the whole corpus cold next scan -- the opposite of the persistence
        // goal. A genuinely empty corpus simply keeps its (already empty) memo.
        let seen: HashSet<String> =
            transcripts.iter().map(|t| t.path.to_string_lossy().into_owned()).collect();
        if !seen.is_empty() {
            self.listing_cache.retain(&seen);
        }

        // Bound the on-demand memo the same way (issue #11): keyed by every
        // discovered skill's dir path -- the key `cached_on_demand`/the
        // background fill use -- so a row survives only while its skill is
        // still discovered. Skip an empty discovery for the same reason the
        // listing retain skips an empty enumeration: it is far likelier a
        // transient read failure than every skill vanishing, and pruning on it
        // would wipe the memo and re-pend the whole corpus next scan.
        let discovered_keys: HashSet<String> =
            discovery.skills.iter().map(|s| s.dir_path.to_string_lossy().into_owned()).collect();
        if !discovered_keys.is_empty() {
            self.on_demand_cache.retain(&discovered_keys);
        }

        // Attributed usage (issue #5): ingest new transcript usage into the
        // persisted, deduped store over the SAME main-thread enumeration the
        // listing index already built, then index the totals by attribution key
        // so each skill can look up its own. When the user opts in (issue #13),
        // the sub-agent transcripts are enumerated as a SECOND provenance-tagged
        // list and folded in; these refs feed only the usage pass, never the
        // listing index or its retain set above (grill D4).
        let subagent_transcripts =
            if include_subagents { subagent_transcript_refs(&all_project_dirs) } else { Vec::new() };
        usage::refresh_usage(&transcripts, &subagent_transcripts, &self.usage_cache);
        let usage_index = UsageIndex::build(&self.usage_cache, include_subagents);

        let skills = discovery
            .skills
            .iter()
            .map(|skill| {
                let search_dirs = self.always_on_search_dirs(skill, &known_repos);
                let always_on = always_on_text_from_index(skill, &index, &search_dirs);
                // Defer the on-demand ceiling: resolve it from the memo if a
                // fresh value is stored, otherwise leave it pending (`None`)
                // for the background pass (issue #11). No tokenization here.
                let on_demand = self.cached_on_demand(skill);
                let footprint = self.footprint_with_always_on(skill, always_on, on_demand);
                SkillReport::from_parts(skill, &footprint, usage_index.for_skill(skill))
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
            api_key_present: self.api_key_present(),
        }
    }

    fn api_key_present(&self) -> bool {
        self.api_key_store.get().is_some()
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

/// The eager on-demand sum as a free function so the adapter method and the
/// off-lock background pass (issue #11) run byte-identical code -- the
/// zero-drift guarantee. Reads every bundled file and tokenizes it through
/// `count_text`, exactly as the pre-#11 `footprint_with_always_on` did.
fn compute_on_demand_with(
    skill: &DiscoveredSkill,
    cache: &TokenCache,
    api_key_store: &dyn ApiKeyStore,
    client: &dyn CountTokensClient,
    tokenizer: &dyn Tokenizer,
) -> LayerCount {
    sum_layer_counts(
        footprint_text::on_demand_file_texts(skill)
            .into_iter()
            .map(|(_, text)| count_text(&text, cache, api_key_store, client, tokenizer, REFERENCE_MODEL_ID)),
    )
}

/// A skill's on-demand memo `(skill_key, signature)`. The key is the skill's
/// dir path (unique per skill, and the same key `scan_all`'s retain uses). The
/// signature is a hash over the SORTED `(path, mtime_nanos, size)` tuples of
/// exactly the discovery `on_demand_files` set -- sorted because `read_dir`
/// order is arbitrary, so an unsorted hash would flip between scans and never
/// hit. Computed identically on read (`cached_on_demand`) and write (the
/// background fill), so a fill always lands on the signature a later scan looks
/// up.
fn on_demand_signature(skill: &DiscoveredSkill) -> (String, String) {
    let skill_key = skill.dir_path.to_string_lossy().into_owned();

    let mut tuples: Vec<(String, i64, i64)> = skill
        .on_demand_files
        .iter()
        .map(|path| {
            // A file that can't be stat'd (deleted between discovery and here)
            // gets a stable sentinel so the signature is still deterministic;
            // when it reappears its real (mtime, size) shifts the signature and
            // forces a recompute.
            let (mtime, size) = match std::fs::metadata(path) {
                Ok(meta) => (meta.modified().ok().and_then(mtime_nanos).unwrap_or(-1), meta.len() as i64),
                Err(_) => (-1, -1),
            };
            (path.to_string_lossy().into_owned(), mtime, size)
        })
        .collect();
    tuples.sort();

    let mut canonical = String::new();
    for (path, mtime, size) in &tuples {
        canonical.push_str(path);
        canonical.push('\0');
        canonical.push_str(&mtime.to_string());
        canonical.push('\0');
        canonical.push_str(&size.to_string());
        canonical.push('\n');
    }
    (skill_key, sha256_hex(&canonical))
}

/// Compute and persist the on-demand ceiling for each still-pending skill,
/// against freshly-opened connections so a panic here can never poison the
/// interactive adapter's `Mutex` and the interactive path is never blocked by
/// tokenization (issue #11). Idempotent: a skill whose fresh signature is
/// already stored is skipped, so the emit -> reload -> rescan cycle terminates
/// after one extra pass. Returns whether at least one ceiling was written, so
/// the caller only emits `on-demand-ready` when there is something new to show.
///
/// The stored value is exactly `compute_on_demand_with(skill, ..)`, so a warm
/// rescan reads back the same tokens and source the eager path would have
/// produced -- zero drift.
pub fn fill_on_demand_ceilings(
    skills: &[DiscoveredSkill],
    cache: &TokenCache,
    on_demand_cache: &SqliteOnDemandCache,
    api_key_store: &dyn ApiKeyStore,
    client: &dyn CountTokensClient,
    tokenizer: &dyn Tokenizer,
) -> bool {
    let mut wrote = false;
    for skill in skills {
        // A no-bundled-files skill resolves immediately in `cached_on_demand`
        // and is never pending, so it should never reach here; skip defensively
        // rather than store a redundant zero row.
        if skill.on_demand_files.is_empty() {
            continue;
        }
        let (skill_key, signature) = on_demand_signature(skill);
        if on_demand_cache.get(&skill_key, &signature).is_some() {
            continue;
        }
        let ceiling = compute_on_demand_with(skill, cache, api_key_store, client, tokenizer);
        on_demand_cache.put(&skill_key, &signature, &ceiling);
        wrote = true;
    }
    wrote
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

        // The single-skill path is eager, so on-demand is resolved (`Some`),
        // never pending.
        let on_demand = footprint.on_demand.expect("single-skill path resolves on-demand eagerly");
        assert_eq!(on_demand.source, TokenSource::Estimate);
        assert_eq!(on_demand.tokens, estimate_tokens("supplementary reference material"));
    }

    #[test]
    fn scan_all_bundles_every_discovered_skill_with_its_footprint() {
        use crate::domain::report::SkillKind;

        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill(&claude_home.join("skills").join("alpha"), "alpha");
        write_skill(&claude_home.join("skills").join("beta"), "beta");

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all(false);

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
        let report = adapter.scan_all(false);

        let grilling = report.skills.iter().find(|s| s.name == "grilling").unwrap();
        assert!(grilling.always_on_native, "batched scan should source always-on from the transcript (Native)");

        // And it agrees with the single-skill path's confidence + tokens.
        let discovery = adapter.discover_skills();
        let skill = discovery.skills.iter().find(|s| s.directory_name() == "grilling").unwrap();
        let per_skill = adapter.compute_footprint(skill);
        assert_eq!(per_skill.always_on.confidence, TextConfidence::Native);
        assert_eq!(grilling.always_on.tokens, per_skill.always_on.count.tokens);
    }

    // ---- issue #13: the sub-agent usage include toggle end to end ----

    /// A main `assistant` record attributing `work` tokens to `skill`.
    fn assistant_usage_line(message_id: &str, skill: &str, work: u32) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": message_id,
            "attributionSkill": skill,
            "message": {"id": message_id, "role": "assistant", "usage": {"input_tokens": work, "output_tokens": 0}}
        })
        .to_string()
    }

    /// The same, written as a sub-agent record (`isSidechain: true`).
    fn subagent_usage_line(message_id: &str, skill: &str, work: u32) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": message_id,
            "attributionSkill": skill,
            "isSidechain": true,
            "message": {"id": message_id, "role": "assistant", "usage": {"input_tokens": work, "output_tokens": 0}}
        })
        .to_string()
    }

    /// A claude_home with a personal `grilling` skill, a main transcript that
    /// registers the repo and attributes 10 work tokens to it, and a sub-agent
    /// transcript under `<session>/subagents/` attributing 999.
    fn claude_home_with_grilling_usage(tmp: &std::path::Path) -> std::path::PathBuf {
        let claude_home = tmp.join(".claude");
        write_skill(&claude_home.join("skills").join("grilling"), "grilling");

        let project_dir = claude_home.join("projects").join("-tmp-repo");
        fs::create_dir_all(&project_dir).unwrap();
        let main = [
            serde_json::json!({"type": "attachment", "cwd": "/tmp/repo", "sessionId": "s"}).to_string(),
            assistant_usage_line("m_main", "grilling", 10),
        ]
        .join("\n");
        fs::write(project_dir.join("main.jsonl"), format!("{main}\n")).unwrap();

        let subagents = project_dir.join("session-x").join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(subagents.join("agent-1.jsonl"), format!("{}\n", subagent_usage_line("m_sub", "grilling", 999))).unwrap();

        claude_home
    }

    #[test]
    fn scan_all_default_excludes_subagent_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = claude_home_with_grilling_usage(tmp.path());
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);

        let report = adapter.scan_all(false);
        let grilling = report.skills.iter().find(|s| s.name == "grilling").unwrap();
        assert_eq!(grilling.usage.unwrap().work, 10, "the default scan never reads the sub-agent file");
    }

    #[test]
    fn scan_all_includes_subagent_usage_when_toggled_on() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = claude_home_with_grilling_usage(tmp.path());
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);

        let report = adapter.scan_all(true);
        let grilling = report.skills.iter().find(|s| s.name == "grilling").unwrap();
        assert_eq!(grilling.usage.unwrap().work, 1009, "the toggle folds the sub-agent's own 999 into the main 10");
    }

    #[test]
    fn a_subagent_skill_listing_does_not_change_always_on_when_toggled_on() {
        // grill D4: with the toggle ON a sub-agent transcript feeds the USAGE
        // pass, but its own `skill_listing` attachment must NEVER feed the
        // listing index. Here the main transcript never renders grilling's
        // bullet, so always-on must stay reconstructed from frontmatter -- if
        // the sub-agent's bullet had leaked into the index it would flip Native.
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill(&claude_home.join("skills").join("grilling"), "grilling");

        let project_dir = claude_home.join("projects").join("-tmp-repo");
        fs::create_dir_all(&project_dir).unwrap();
        // Registers the repo (cwd) but does NOT render grilling's bullet.
        fs::write(
            project_dir.join("main.jsonl"),
            format!("{}\n", serde_json::json!({"type": "attachment", "cwd": "/tmp/repo", "sessionId": "s"})),
        )
        .unwrap();
        // The sub-agent transcript DOES carry a grilling skill_listing bullet.
        let subagents = project_dir.join("session-x").join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        let listing = serde_json::json!({
            "type": "attachment",
            "isSidechain": true,
            "attachment": {"type": "skill_listing", "content": "- grilling: SUB-AGENT wording", "names": ["grilling"]}
        });
        fs::write(subagents.join("agent-1.jsonl"), format!("{listing}\n")).unwrap();

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all(true);

        let grilling = report.skills.iter().find(|s| s.name == "grilling").unwrap();
        assert!(
            !grilling.always_on_native,
            "a sub-agent skill_listing must not feed the listing index; always-on stays reconstructed"
        );
    }

    #[test]
    fn scan_all_reconstructs_usage_from_a_pre_attribution_transcript() {
        use crate::domain::report::AttributionSource;

        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill(&claude_home.join("skills").join("grilling"), "grilling");

        // A pre-attribution (2.1.145) transcript that registers a known repo
        // (cwd) and, with NO native attributionSkill anywhere, invokes `grilling`
        // and then produces a turn -- which the reconstruction walk must credit.
        let project_dir = claude_home.join("projects").join("-tmp-repo");
        fs::create_dir_all(&project_dir).unwrap();
        let human = serde_json::json!({
            "type": "user", "version": "2.1.145", "cwd": "/tmp/some-repo",
            "message": {"role": "user", "content": "help me"}
        });
        let invoke = serde_json::json!({
            "type": "assistant", "version": "2.1.145", "uuid": "m_inv",
            "message": {"id": "m_inv", "role": "assistant",
                "content": [{"type": "tool_use", "name": "Skill", "input": {"skill": "grilling"}}],
                "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        });
        let turn = serde_json::json!({
            "type": "assistant", "version": "2.1.145", "uuid": "m1",
            "message": {"id": "m1", "role": "assistant",
                "content": [{"type": "text", "text": "asking a question"}],
                "usage": {"input_tokens": 30, "output_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        });
        fs::write(project_dir.join("s.jsonl"), format!("{human}\n{invoke}\n{turn}\n")).unwrap();

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all();

        let grilling = report.skills.iter().find(|s| s.name == "grilling").unwrap();
        let usage = grilling.usage.expect("grilling should have reconstructed usage");
        assert_eq!(usage.work, 40, "the post-invoke turn (30 + 10) is credited to grilling");
        assert_eq!(usage.attribution_source, AttributionSource::Reconstructed);
    }

    #[test]
    fn stale_exact_count_is_zero_on_a_fresh_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill(&claude_home.join("skills").join("alpha"), "alpha");

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        adapter.scan_all(false);

        // Nothing exact has ever been stored (no key), so nothing is stale.
        assert_eq!(adapter.stale_exact_count(), 0);
    }

    #[test]
    fn scan_all_reports_no_api_key_when_none_is_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(&claude_home).unwrap();
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);

        assert!(!adapter.scan_all(false).api_key_present);
    }

    #[test]
    fn scan_all_reports_an_api_key_when_one_is_configured() {
        use crate::footprint::api_key_store::FakeApiKeyStore;
        use crate::footprint::cache::TokenCache;
        use crate::footprint::count_tokens_client::FakeCountTokensClient;

        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(&claude_home).unwrap();
        let adapter = ClaudeCodeAdapter::new(
            claude_home,
            TokenCache::open_in_memory().unwrap(),
            SqliteListingCache::open_in_memory().unwrap(),
            SqliteUsageCache::open_in_memory().unwrap(),
            SqliteOnDemandCache::open_in_memory().unwrap(),
            Box::new(FakeApiKeyStore::with_key("sk-ant-test")),
            Box::new(FakeCountTokensClient::always_returns(0)),
            Box::new(BpeTokenizer),
        );

        assert!(adapter.scan_all(false).api_key_present);
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
            SqliteListingCache::open(&tmp.path().join("listing_index.sqlite")).unwrap(),
            SqliteUsageCache::open(&tmp.path().join("usage.sqlite")).unwrap(),
            SqliteOnDemandCache::open(&tmp.path().join("on_demand_index.sqlite")).unwrap(),
            Box::new(KeychainApiKeyStore::new().unwrap()),
            Box::new(AnthropicCountTokensClient::new()),
            Box::new(BpeTokenizer),
        );

        // Cold scan populates the content-hash + listing caches; the second
        // scan reuses them. Since issue #11 the interactive scan no longer
        // tokenizes the on-demand ceiling at all -- the ~216 MB of bundled
        // reference files (the bulk of the old "~6.5s cold" cost) is deferred
        // to the background fill exercised below, so both scans here return the
        // on-demand layer as pending (`None`). The persistent production cache
        // (app data dir, not this test's temp file) still amortizes the cold
        // cost to once-ever per unique content.
        use std::time::Instant;
        let cold = Instant::now();
        let report = adapter.scan_all(false);
        let cold_elapsed = cold.elapsed();

        let warm = Instant::now();
        let _ = adapter.scan_all(false);
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
            "the content-hash + listing caches must make a second scan faster than the first"
        );

        // On the interactive path every skill with bundled files is pending; a
        // skill with none resolves immediately to a zero ceiling (issue #11).
        let pending_count = report.skills.iter().filter(|s| s.on_demand.is_none()).count();
        eprintln!("{pending_count} of {} skills have a pending on-demand ceiling", report.skills.len());

        // Now drive the background fill end-to-end (the real work off the
        // interactive scan) and re-scan: every ceiling must resolve, and a
        // warm rescan that serves them from the memo must not re-pend.
        let pending = adapter.pending_on_demand();
        let filled = adapter.fill_on_demand(&pending);
        let resolved = adapter.scan_all();
        assert!(
            resolved.skills.iter().all(|s| s.on_demand.is_some()),
            "after the background fill no skill should still be pending"
        );
        assert!(adapter.pending_on_demand().is_empty(), "the fill must reach a steady state");
        eprintln!("background fill wrote at least one ceiling: {filled}");

        for skill in resolved.skills.iter().take(10) {
            eprintln!(
                "  [{:?}] {:<28} always_on={:>4} (exact={}, native={})  on_invoke={:>5}  on_demand={:?}  usage={:?}",
                skill.kind,
                skill.name,
                skill.always_on.tokens,
                skill.always_on.exact,
                skill.always_on_native,
                skill.on_invoke.tokens,
                skill.on_demand.map(|l| l.tokens),
                skill.usage.map(|u| u.work),
            );
        }
        let attributed = resolved.skills.iter().filter(|s| s.usage.is_some()).count();
        eprintln!("{attributed} of {} skills have attributed usage (issue #5)", resolved.skills.len());

        // The bar: a real machine with skills installed returns a non-empty
        // scan whose always-on layer actually measured something.
        assert!(!report.skills.is_empty(), "expected the real ~/.claude to have at least one skill");
        assert!(
            report.skills.iter().any(|s| s.always_on.tokens > 0),
            "expected at least one skill's always-on layer to count more than zero tokens"
        );
        // Issue #5: this machine's transcripts carry native attribution, so at
        // least one discovered skill should have attributed work tokens.
        assert!(
            report.skills.iter().any(|s| s.usage.is_some_and(|u| u.work > 0)),
            "expected at least one skill to have attributed usage from the real transcripts"
        );
    }

    // ---- issue #11: deferred on-demand tokenization ----

    use crate::footprint::api_key_store::FakeApiKeyStore;
    use crate::footprint::cache::TokenCache as Tc;
    use crate::footprint::count_tokens_client::FakeCountTokensClient;
    use crate::footprint::tokenizer::estimate_tokens;
    use std::sync::{Arc, Mutex};

    const BODY_SENTINEL: &str = "BODYSENTINELXYZZY";
    const ONDEMAND_SENTINEL: &str = "ONDEMANDSENTINELPLUGH";

    /// A `Tokenizer` that records every text it is asked to estimate, then
    /// delegates to the real one. The negative proof for issue #11's C1: the
    /// interactive scan must tokenize the body but never the bundled files.
    struct SpyTokenizer {
        seen: Arc<Mutex<Vec<String>>>,
        real: BpeTokenizer,
    }

    impl SpyTokenizer {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (Self { seen: seen.clone(), real: BpeTokenizer }, seen)
        }
    }

    impl Tokenizer for SpyTokenizer {
        fn estimate(&self, text: &str) -> u32 {
            self.seen.lock().unwrap().push(text.to_string());
            self.real.estimate(text)
        }
    }

    fn seen_contains(seen: &Arc<Mutex<Vec<String>>>, needle: &str) -> bool {
        seen.lock().unwrap().iter().any(|t| t.contains(needle))
    }

    /// Writes a personal skill whose body carries `body_marker` and, if
    /// `on_demand` are given, a bundled file per entry (`filename`, `content`).
    fn write_skill_with_bundle(
        claude_home: &std::path::Path,
        name: &str,
        body_marker: &str,
        on_demand: &[(&str, &str)],
    ) {
        let dir = claude_home.join("skills").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: a test skill\n---\n\nBody {body_marker}.\n"),
        )
        .unwrap();
        for (filename, content) in on_demand {
            fs::write(dir.join(filename), content).unwrap();
        }
    }

    fn adapter_with(
        claude_home: PathBuf,
        on_demand_cache: SqliteOnDemandCache,
        store: Box<dyn ApiKeyStore>,
        client: Box<dyn CountTokensClient>,
        tokenizer: Box<dyn Tokenizer>,
    ) -> ClaudeCodeAdapter {
        ClaudeCodeAdapter::new(
            claude_home,
            Tc::open_in_memory().unwrap(),
            SqliteListingCache::open_in_memory().unwrap(),
            SqliteUsageCache::open_in_memory().unwrap(),
            on_demand_cache,
            store,
            client,
            tokenizer,
        )
    }

    fn find<'a>(discovery: &'a DiscoveryResult, name: &str) -> &'a DiscoveredSkill {
        discovery.skills.iter().find(|s| s.directory_name() == name).unwrap()
    }

    /// C1: the interactive scan tokenizes the body (on-invoke) but performs
    /// ZERO on-demand tokenization, leaves on-demand pending, and never calls
    /// the exact API.
    #[test]
    fn interactive_scan_all_performs_zero_on_demand_tokenization() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill_with_bundle(&claude_home, "grilling", BODY_SENTINEL, &[("REF.md", ONDEMAND_SENTINEL)]);

        let (spy, seen) = SpyTokenizer::new();
        let client = FakeCountTokensClient::always_returns(0);
        let counter = client.call_count_handle();
        let adapter = adapter_with(
            claude_home,
            SqliteOnDemandCache::open_in_memory().unwrap(),
            Box::new(FakeApiKeyStore::empty()),
            Box::new(client),
            Box::new(spy),
        );

        let report = adapter.scan_all();
        let grilling = report.skills.iter().find(|s| s.name == "grilling").unwrap();

        assert!(seen_contains(&seen, BODY_SENTINEL), "the body (on-invoke) must be tokenized");
        assert!(
            !seen_contains(&seen, ONDEMAND_SENTINEL),
            "the bundled file must NOT be tokenized on the interactive path"
        );
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0, "no exact API call on the interactive path");
        assert!(grilling.on_demand.is_none(), "a skill with bundled files is pending, not resolved");
        assert!(grilling.always_on.tokens > 0, "always-on is resolved and non-zero");
        assert!(grilling.on_invoke.tokens > 0, "on-invoke is resolved and non-zero");
    }

    /// C3 direction A: a skill with no bundled files resolves immediately to a
    /// zero ceiling, never pending.
    #[test]
    fn empty_bundled_files_resolves_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill_with_bundle(&claude_home, "no-refs", BODY_SENTINEL, &[]);

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all();
        let skill = report.skills.iter().find(|s| s.name == "no-refs").unwrap();

        let on_demand = skill.on_demand.expect("an empty bundle resolves immediately, never pending");
        assert_eq!(on_demand.tokens, 0);
        assert!(on_demand.exact, "an empty sum is exact, not an estimate");
    }

    /// C3 direction B: a skill WITH bundled files but no stored ceiling is
    /// pending (`None`), never a `Some(0)` that would flash a wrong ceiling.
    #[test]
    fn non_empty_uncomputed_is_pending_not_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill_with_bundle(&claude_home, "has-refs", BODY_SENTINEL, &[("REF.md", "reference material")]);

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all();
        let skill = report.skills.iter().find(|s| s.name == "has-refs").unwrap();

        assert!(skill.on_demand.is_none(), "an uncomputed non-empty bundle is pending, never Some(0)");
    }

    /// C2 (zero drift): whatever the eager `compute_on_demand` produces --
    /// across all-estimate, a within-skill exact+estimate mix, an empty
    /// bundle, and a non-UTF-8 file -- the background fill stores and a later
    /// `cached_on_demand` reads back the SAME tokens AND source.
    #[test]
    fn background_fill_equals_eager_compute() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill_with_bundle(&claude_home, "all-estimate", BODY_SENTINEL, &[("a.md", "alpha estimate content")]);
        write_skill_with_bundle(
            &claude_home,
            "mixed",
            BODY_SENTINEL,
            &[("x.md", "X-exact-content"), ("y.md", "Y-estimate-content")],
        );
        write_skill_with_bundle(&claude_home, "empty", BODY_SENTINEL, &[]);
        // A readable file plus a non-UTF-8 one (the latter is skipped by
        // `on_demand_file_texts`, so it must not perturb the summed ceiling).
        let bad_dir = claude_home.join("skills").join("nonutf8");
        write_skill_with_bundle(&claude_home, "nonutf8", BODY_SENTINEL, &[("good.md", "readable good content")]);
        fs::write(bad_dir.join("bad.bin"), [0xff, 0xfe, 0x00, 0xff]).unwrap();

        let adapter = adapter_with(
            claude_home,
            SqliteOnDemandCache::open_in_memory().unwrap(),
            Box::new(FakeApiKeyStore::empty()),
            Box::new(FakeCountTokensClient::always_returns(0)),
            Box::new(BpeTokenizer),
        );

        // Pre-seed a fresh exact under the reference model for x.md's content so
        // "mixed" genuinely mixes an Exact file with an Estimate one -- the only
        // way to force a within-skill source mix, since `FakeCountTokensClient`
        // has no per-text control. `exact == tiktoken` keeps the calibration
        // factor at ~1 so the estimate files aren't wildly rescaled.
        let x_hash = sha256_hex("X-exact-content");
        adapter.cache.put_tiktoken(&x_hash, estimate_tokens("X-exact-content"));
        adapter.cache.put_exact(&x_hash, estimate_tokens("X-exact-content"), REFERENCE_MODEL_ID);

        let discovery = adapter.discover_skills();
        let names = ["all-estimate", "mixed", "empty", "nonutf8"];
        let eager: Vec<LayerCount> = names.iter().map(|n| adapter.compute_on_demand(find(&discovery, n))).collect();

        // The "mixed" skill really is a mix: an Exact file + an Estimate file
        // sums to Estimate (ADR 0017). Guards the test's own premise.
        let mixed_eager = eager[1];
        assert_eq!(mixed_eager.source, TokenSource::Estimate, "mixed must downgrade to Estimate");

        let pending = adapter.pending_on_demand();
        adapter.fill_on_demand(&pending);

        for (name, expected) in names.iter().zip(eager) {
            assert_eq!(
                adapter.cached_on_demand(find(&discovery, name)),
                Some(expected),
                "zero drift for {name}: cached ceiling must equal the eager compute in tokens and source"
            );
        }
    }

    /// C4: a warm rescan serves the on-demand ceiling from the persisted memo
    /// -- a fresh tokenizer records ZERO bundled-file tokenizations, the exact
    /// API is never called, and the resolved value equals the eager compute.
    #[test]
    fn warm_rescan_serves_on_demand_from_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill_with_bundle(&claude_home, "grilling", BODY_SENTINEL, &[("REF.md", ONDEMAND_SENTINEL)]);
        let db = tmp.path().join("on_demand_index.sqlite");

        // Cold adapter: scan (pending) then background-fill into the persisted memo.
        let adapter1 = adapter_with(
            claude_home.clone(),
            SqliteOnDemandCache::open(&db).unwrap(),
            Box::new(FakeApiKeyStore::empty()),
            Box::new(FakeCountTokensClient::always_returns(0)),
            Box::new(BpeTokenizer),
        );
        let cold = adapter1.scan_all();
        assert!(cold.skills[0].on_demand.is_none(), "cold interactive scan leaves it pending");
        let eager = adapter1.compute_on_demand(find(&adapter1.discover_skills(), "grilling"));
        let pending = adapter1.pending_on_demand();
        adapter1.fill_on_demand(&pending);

        // Warm adapter: fresh spy tokenizer + fresh client, SAME persisted memo file.
        let (spy, seen) = SpyTokenizer::new();
        let client = FakeCountTokensClient::always_returns(0);
        let counter = client.call_count_handle();
        let adapter2 = adapter_with(
            claude_home,
            SqliteOnDemandCache::open(&db).unwrap(),
            Box::new(FakeApiKeyStore::empty()),
            Box::new(client),
            Box::new(spy),
        );

        let warm = adapter2.scan_all();
        let grilling = warm.skills.iter().find(|s| s.name == "grilling").unwrap();
        let on_demand = grilling.on_demand.expect("warm rescan resolves on-demand from the memo");

        assert_eq!(on_demand.tokens, eager.tokens, "served ceiling equals the eager compute");
        assert_eq!(on_demand.exact, eager.source == TokenSource::Exact, "source is preserved");
        assert!(
            !seen_contains(&seen, ONDEMAND_SENTINEL),
            "the warm rescan must not re-tokenize the bundled file"
        );
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0, "no exact API call on the warm path");
    }

    /// After one fill the worklist is empty, so the emit -> reload -> rescan
    /// cycle terminates after exactly one extra pass.
    #[test]
    fn fill_is_idempotent_and_terminates() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill_with_bundle(&claude_home, "has-refs", BODY_SENTINEL, &[("REF.md", "reference material")]);

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let first = adapter.pending_on_demand();
        assert!(!first.is_empty(), "a fresh skill with bundled files starts pending");

        let wrote = adapter.fill_on_demand(&first);
        assert!(wrote, "the first fill writes at least one ceiling");
        assert!(adapter.pending_on_demand().is_empty(), "after one fill nothing is pending");

        // A second fill over the (now empty) worklist writes nothing.
        assert!(!adapter.fill_on_demand(&adapter.pending_on_demand()), "the fill is idempotent");
    }

    /// An unresolvable bundle (no key -> estimate, and a non-UTF-8 file that is
    /// skipped) still reaches a non-pending steady state and does not re-pend:
    /// pending must NOT be gated on the exact-vs-estimate tier, or an invalid
    /// key would livelock the fill forever.
    #[test]
    fn unresolvable_file_reaches_steady_state() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        // Estimate-only: a readable file, no key.
        write_skill_with_bundle(&claude_home, "estimate-only", BODY_SENTINEL, &[("ref.md", "some reference text")]);
        // A skill whose ONLY bundled file is non-UTF-8 (skipped by the reader).
        let bad_dir = claude_home.join("skills").join("bad-only");
        write_skill_with_bundle(&claude_home, "bad-only", BODY_SENTINEL, &[]);
        fs::write(bad_dir.join("bad.bin"), [0xff, 0xfe, 0x00, 0xff]).unwrap();

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let pending = adapter.pending_on_demand();
        assert_eq!(pending.len(), 2, "both skills start pending (each has bundled files on disk)");
        adapter.fill_on_demand(&pending);

        let discovery = adapter.discover_skills();
        let estimate_only = adapter.cached_on_demand(find(&discovery, "estimate-only")).expect("resolves");
        assert_eq!(estimate_only.source, TokenSource::Estimate, "no key -> estimate, still non-pending");
        let bad_only = adapter.cached_on_demand(find(&discovery, "bad-only")).expect("resolves even when unreadable");
        assert_eq!(bad_only.tokens, 0, "a fully-skipped bundle resolves to a zero ceiling");

        // Steady state: a re-scan finds nothing pending, and the reported layer
        // is resolved (`Some`), not pending.
        assert!(adapter.pending_on_demand().is_empty(), "no skill re-pends after a fill");
        let report = adapter.scan_all();
        assert!(report.skills.iter().all(|s| s.on_demand.is_some()), "every skill is resolved after the fill");
    }
}
