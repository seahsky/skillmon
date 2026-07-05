pub mod discovery;
pub mod footprint_text;
pub mod frontmatter;
pub mod listing_cache;
pub mod paths;
pub mod settings;
pub mod usage;
pub mod usage_cache;
pub mod watcher;

use crate::domain::budget::{
    detect_anomaly, evaluate_budget, BudgetConfig, ToastRequest, ANOMALY_WINDOW_DAYS, DEFAULT_ANOMALY_FLOOR,
    DEFAULT_ANOMALY_MULTIPLIER, DEFAULT_BUDGET_WORK_TOKENS,
};
use crate::domain::footprint::{AlwaysOnFootprint, Footprint, LayerCount, TokenSource};
use crate::domain::harness::HarnessAdapter;
use crate::domain::report::{ScanReport, SkillReport, UsageSettings};
use crate::domain::scan::{ScanOutcome, ScanParams, UsageWindow, DAY_MILLIS, HOUR_MILLIS};
use crate::domain::skill::{DiscoveredSkill, DiscoveryResult, SkillId};
use crate::footprint::api_key_store::ApiKeyStore;
use crate::footprint::cache::TokenCache;
use crate::footprint::compute::count_text;
use crate::footprint::count_tokens_client::CountTokensClient;
use discovery::plugin::{discover_plugin_skills, parse_installed_plugins};
use discovery::project::discover_project_skills;
use discovery::transcript::{enumerate_known_repos, find_active_repo, RepoInfo};
use footprint_text::{always_on_text_from_index, transcript_refs_by_recency, AlwaysOnText, ListingIndex};
use listing_cache::SqliteListingCache;
use settings::is_plugin_live;
use usage::UsageIndex;
use usage_cache::{
    SqliteUsageCache, META_ANOMALY_ENABLED, META_BUDGET_ALERTED, META_BUDGET_ENABLED, META_BUDGET_WORK_TOKENS,
};
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
    api_key_store: Box<dyn ApiKeyStore>,
    client: Box<dyn CountTokensClient>,
}

impl ClaudeCodeAdapter {
    /// Callers construct the caches/key-store/client at the composition root
    /// (e.g. Tauri's `setup` hook) and hand them in already built, so this
    /// constructor stays infallible -- any I/O error in opening a cache or
    /// resolving the keychain entry surfaces where it's actually handled.
    pub fn new(
        claude_home: PathBuf,
        cache: TokenCache,
        listing_cache: SqliteListingCache,
        usage_cache: SqliteUsageCache,
        api_key_store: Box<dyn ApiKeyStore>,
        client: Box<dyn CountTokensClient>,
    ) -> Self {
        Self { claude_home, cache, listing_cache, usage_cache, api_key_store, client }
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
    /// `scan` path instead, which reads each transcript once rather than once
    /// per skill.
    ///
    /// The batched runtime entry point (`list_skills` -> inherent `scan`) no
    /// longer reaches this, so from the cdylib's perspective it and its
    /// transcript helpers are dead; they are kept as the single-skill recompute
    /// primitive (ADR 0019) and the `HarnessAdapter::compute_footprint` seam
    /// (ADR 0002), and are exercised by the test suite. `allow(dead_code)`
    /// propagates to the private always-on helpers this calls.
    #[allow(dead_code)]
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

    /// One full scan, parameterized by an injected clock and the requested
    /// display window (issue #14). The batched pass (discover once, build the
    /// listing index once, ingest usage once) is unchanged from the former
    /// `scan_all`; what's new is that the per-skill usage figures are windowed
    /// per `params`, and a fixed-24h budget/anomaly evaluation runs afterward
    /// and returns any toasts. The trait `scan_all` is the clockless all-time
    /// shim over this, so the `HarnessAdapter` trait stays untouched (ADR 0002).
    pub fn scan(&self, params: &ScanParams) -> ScanOutcome {
        let discovery = ClaudeCodeAdapter::discover_skills(self);
        let known_repos = enumerate_known_repos(&self.claude_home);

        let all_project_dirs: Vec<PathBuf> = known_repos.iter().map(|r| r.project_dir.clone()).collect();
        let wanted: HashSet<String> =
            discovery.skills.iter().map(|s| s.directory_name().to_string()).collect();
        let transcripts = transcript_refs_by_recency(&all_project_dirs);
        let (index, _stats) = ListingIndex::build_incremental(&transcripts, &wanted, &self.listing_cache);
        // Bound the memo (see the former scan_all): only prune when the
        // enumeration is non-empty, so a transient read failure doesn't wipe it.
        let seen: HashSet<String> =
            transcripts.iter().map(|t| t.path.to_string_lossy().into_owned()).collect();
        if !seen.is_empty() {
            self.listing_cache.retain(&seen);
        }

        // Ingest new transcript usage into the persisted deduped store over the
        // SAME enumeration the listing index built (issue #5).
        usage::refresh_usage(&transcripts, &self.usage_cache);

        // The per-skill display figures depend on the requested window; the 24h
        // budget below is independent and always 24h (DESIGN.md UX #4).
        let (usage_index, usage_window_hours) = match params.usage_window {
            UsageWindow::AllTime => (UsageIndex::build(&self.usage_cache), None),
            UsageWindow::Rolling { hours } => {
                let cutoff = params.now_millis - (hours as i64) * HOUR_MILLIS;
                (UsageIndex::build_windowed(&self.usage_cache, cutoff), Some(hours))
            }
        };

        let skills = discovery
            .skills
            .iter()
            .map(|skill| {
                let search_dirs = self.always_on_search_dirs(skill, &known_repos);
                let always_on = always_on_text_from_index(skill, &index, &search_dirs);
                let footprint = self.footprint_with_always_on(skill, always_on);
                SkillReport::from_parts(skill, &footprint, usage_index.for_skill(skill))
            })
            .collect();
        let warnings = discovery
            .warnings
            .iter()
            .map(|w| format!("{}: {}", w.path.display(), w.reason))
            .collect();

        let report = ScanReport {
            skills,
            warnings,
            active_repo_path: discovery.active_repo_path.map(|p| p.display().to_string()),
            api_key_present: self.api_key_present(),
            usage_window_hours,
        };

        // A clockless scan (all_time(), now == 0) skips the time-relative toast
        // evaluation entirely: no wall-clock read, no meta writes. A real panel
        // scan always injects now > 0. Toasts are emitted in lib.rs, outside the
        // scan lock, after the debounce state below is persisted (D6).
        let toasts = if params.now_millis > 0 { self.evaluate_toasts(params.now_millis) } else { Vec::new() };

        ScanOutcome { report, toasts }
    }

    /// The fixed-24h budget check plus the optional per-skill anomaly check
    /// (issue #14), run after usage was ingested. Reads config + the persisted
    /// debounce flag from `usage_meta`, persists the next debounce state, and
    /// returns the toasts to fire. Persisting the flag BEFORE emission (which
    /// happens in lib.rs) makes a failed `.show()` a lost nudge, not a stuck
    /// flag that suppresses the next real crossing (D6).
    fn evaluate_toasts(&self, now_millis: i64) -> Vec<ToastRequest> {
        let mut toasts = Vec::new();
        let settings = self.get_usage_settings();

        let cfg = BudgetConfig { enabled: settings.budget_enabled, work_token_limit: settings.budget_work_tokens };
        let rolling_work = self.usage_cache.attributed_work_since(now_millis - DAY_MILLIS);
        let alerted = self.usage_cache.get_meta(META_BUDGET_ALERTED).unwrap_or(0) != 0;
        let outcome = evaluate_budget(rolling_work, &cfg, alerted);
        self.usage_cache.set_meta(META_BUDGET_ALERTED, outcome.next_alerted as i64);
        toasts.extend(outcome.toast);

        if settings.anomaly_enabled {
            toasts.extend(self.detect_usage_anomalies(now_millis));
        }
        toasts
    }

    /// Per-skill anomaly toasts (issue #14, off by default): a skill whose
    /// current-UTC-day attributed work runs above `DEFAULT_ANOMALY_MULTIPLIER`x
    /// its trailing daily average over the prior week. Fuzzy and default-off; a
    /// proxy, not a bill.
    fn detect_usage_anomalies(&self, now_millis: i64) -> Vec<ToastRequest> {
        let cutoff = now_millis - ANOMALY_WINDOW_DAYS * DAY_MILLIS;
        let today = now_millis / DAY_MILLIS;
        // Fold day buckets into per-key (today's work, trailing days' work).
        let mut by_key: HashMap<(String, Option<String>), (u64, Vec<u64>)> = HashMap::new();
        for bucket in self.usage_cache.work_by_key_and_day_since(cutoff) {
            let entry = by_key.entry((bucket.attribution_skill, bucket.attribution_plugin)).or_default();
            if bucket.day >= today {
                entry.0 = entry.0.saturating_add(bucket.work);
            } else {
                entry.1.push(bucket.work);
            }
        }

        let mut toasts = Vec::new();
        for ((skill, _plugin), (current, trailing)) in by_key {
            if let Some(multiple) =
                detect_anomaly(current, &trailing, DEFAULT_ANOMALY_MULTIPLIER, DEFAULT_ANOMALY_FLOOR)
            {
                // Toast the bare skill name, not the `plugin:name` join key.
                let name = skill.rsplit(':').next().unwrap_or(&skill).to_string();
                toasts.push(ToastRequest::Anomaly { skill: name, window_work: current, multiple });
            }
        }
        toasts
    }

    /// The user-configurable usage-toast settings, with product defaults for any
    /// `usage_meta` key not yet written: budget on at the 250k attributed-work
    /// default, anomaly off (issue #14, DESIGN.md UX #4).
    pub fn get_usage_settings(&self) -> UsageSettings {
        UsageSettings {
            budget_enabled: self.usage_cache.get_meta(META_BUDGET_ENABLED).unwrap_or(1) != 0,
            budget_work_tokens: self
                .usage_cache
                .get_meta(META_BUDGET_WORK_TOKENS)
                .map(|v| v.max(0) as u64)
                .unwrap_or(DEFAULT_BUDGET_WORK_TOKENS),
            anomaly_enabled: self.usage_cache.get_meta(META_ANOMALY_ENABLED).unwrap_or(0) != 0,
        }
    }

    /// Persists the usage-toast settings. Re-arms the budget debounce when the
    /// limit or the enabled flag changed (D5), so a lowered limit re-evaluates
    /// on the next scan instead of staying silent until the 24h window resets.
    pub fn set_usage_settings(&self, settings: &UsageSettings) {
        let prev = self.get_usage_settings();
        self.usage_cache.set_meta(META_BUDGET_ENABLED, settings.budget_enabled as i64);
        self.usage_cache.set_meta(META_BUDGET_WORK_TOKENS, settings.budget_work_tokens as i64);
        self.usage_cache.set_meta(META_ANOMALY_ENABLED, settings.anomaly_enabled as i64);
        if prev.budget_enabled != settings.budget_enabled || prev.budget_work_tokens != settings.budget_work_tokens {
            self.usage_cache.set_meta(META_BUDGET_ALERTED, 0);
        }
    }
}

impl HarnessAdapter for ClaudeCodeAdapter {
    fn discover_skills(&self) -> DiscoveryResult {
        ClaudeCodeAdapter::discover_skills(self)
    }

    fn compute_footprint(&self, skill: &DiscoveredSkill) -> Footprint {
        ClaudeCodeAdapter::compute_footprint(self, skill)
    }

    /// The clockless all-time shim over the inherent `scan` (issue #14): a scan
    /// with no injected clock, so it evaluates no time-relative budget and
    /// discards any toasts. The batched, per-transcript-once cost still lives in
    /// `scan`; this only picks the all-time window and drops the toast channel,
    /// so a generic `HarnessAdapter` caller that never learned about toasts is
    /// unaffected (the trait stays untouched, ADR 0002).
    fn scan_all(&self) -> ScanReport {
        self.scan(&ScanParams::all_time()).report
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

    /// A directly-ingestable attributed-usage row for the scan-level e2e tests,
    /// so a test can seed the persisted store with a chosen timestamp without
    /// hand-writing a transcript. `message_id` folds in every field so distinct
    /// rows never collide under the `message_id` PK dedup.
    fn usage_row(skill: &str, work: u32, timestamp_millis: i64) -> usage_cache::UsageRow {
        usage_cache::UsageRow {
            message_id: format!("{skill}-{work}-{timestamp_millis}"),
            attribution_skill: skill.to_string(),
            attribution_plugin: None,
            is_subagent: false,
            work,
            cache_write: 0,
            cache_read: 0,
            timestamp_millis,
        }
    }

    /// A fixed 2027-ish clock, so windowed-scan tests are deterministic and
    /// independent of the wall clock.
    const FIXED_NOW: i64 = 1_800_000_000_000;

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

    #[test]
    fn scan_all_reports_no_api_key_when_none_is_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(&claude_home).unwrap();
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);

        assert!(!adapter.scan_all().api_key_present);
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
            Box::new(FakeApiKeyStore::with_key("sk-ant-test")),
            Box::new(FakeCountTokensClient::always_returns(0)),
        );

        assert!(adapter.scan_all().api_key_present);
    }

    #[test]
    fn scan_all_delegates_to_scan_all_time_with_no_toasts() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill(&claude_home.join("skills").join("grilling"), "grilling");
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        // Far over any budget, but a clockless all-time scan reads no clock, so
        // it evaluates no budget and fires no toast, and reports all-time usage.
        adapter.usage_cache.ingest(&[usage_row("grilling", 1_000_000, FIXED_NOW - 1000)]);

        let outcome = adapter.scan(&ScanParams::all_time());
        assert!(outcome.toasts.is_empty(), "a clockless all_time scan never toasts");
        assert_eq!(outcome.report.usage_window_hours, None, "all_time reports the all-time window");
        // And the debounce flag was never touched (no meta write on now == 0).
        assert_eq!(adapter.usage_cache.get_meta(META_BUDGET_ALERTED), None);
    }

    #[test]
    fn scan_windowed_usage_reflects_the_requested_window() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill(&claude_home.join("skills").join("grilling"), "grilling");
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        adapter.usage_cache.ingest(&[
            usage_row("grilling", 100, FIXED_NOW - 90 * DAY_MILLIS), // old, outside 24h
            usage_row("grilling", 40, FIXED_NOW - 1000),             // recent, inside 24h
        ]);

        let all = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime });
        let g_all = all.report.skills.iter().find(|s| s.name == "grilling").unwrap();
        assert_eq!(g_all.usage.unwrap().work, 140, "all-time sums both records");
        assert_eq!(all.report.usage_window_hours, None);

        let win = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::Rolling { hours: 24 } });
        let g_win = win.report.skills.iter().find(|s| s.name == "grilling").unwrap();
        assert_eq!(g_win.usage.unwrap().work, 40, "the 24h window shows only the recent work");
        assert_eq!(win.report.usage_window_hours, Some(24));
    }

    #[test]
    fn scan_emits_budget_toast_when_rolling_24h_work_exceeds_limit_then_debounces() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(&claude_home).unwrap();
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        // 300k attributed work inside the last 24h, over the 250k default budget.
        adapter.usage_cache.ingest(&[usage_row("grilling", 300_000, FIXED_NOW - 1000)]);

        let params = ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime };
        let first = adapter.scan(&params);
        assert_eq!(first.toasts.len(), 1, "crossing the budget fires exactly one toast");
        assert!(matches!(first.toasts[0], ToastRequest::Budget { rolling_work: 300_000, limit: 250_000 }));

        // Second scan, still over budget: debounced by the persisted flag.
        let second = adapter.scan(&params);
        assert!(second.toasts.is_empty(), "still over budget must not re-toast (persisted debounce)");
    }

    #[test]
    fn changing_the_budget_limit_re_arms_the_debounce() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(&claude_home).unwrap();
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        adapter.usage_cache.ingest(&[usage_row("grilling", 300_000, FIXED_NOW - 1000)]);
        let params = ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime };
        assert_eq!(adapter.scan(&params).toasts.len(), 1); // cross -> toast
        assert!(adapter.scan(&params).toasts.is_empty()); // debounced
        adapter.set_usage_settings(&UsageSettings { budget_enabled: true, budget_work_tokens: 100_000, anomaly_enabled: false });
        assert_eq!(adapter.usage_cache.get_meta(META_BUDGET_ALERTED), Some(0), "settings change re-arms (D5)");
        assert_eq!(adapter.scan(&params).toasts.len(), 1, "re-armed budget re-toasts");
    }

    #[test]
    fn budget_is_on_by_default_with_no_meta_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(&claude_home).unwrap();
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);

        let s = adapter.get_usage_settings();
        assert!(s.budget_enabled, "the budget is on by default");
        assert_eq!(s.budget_work_tokens, 250_000, "the default limit is the 250k product default");
        assert!(!s.anomaly_enabled, "anomaly is off by default");

        // And an over-limit window toasts with no configuration at all.
        adapter.usage_cache.ingest(&[usage_row("grilling", 300_000, FIXED_NOW - 1000)]);
        let out = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime });
        assert_eq!(out.toasts.len(), 1, "default-on budget toasts without any set_usage_settings call");
    }

    #[test]
    fn anomaly_is_off_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(&claude_home).unwrap();
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        // A blatant spike: a week of ~10k/day, then 500k today. Anomaly still off.
        let day = FIXED_NOW / DAY_MILLIS;
        for d in 1..=7 {
            adapter.usage_cache.ingest(&[usage_row("grilling", 10_000, (day - d) * DAY_MILLIS + 100)]);
        }
        adapter.usage_cache.ingest(&[usage_row("grilling", 500_000, day * DAY_MILLIS + 100)]);

        let out = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime });
        assert!(
            !out.toasts.iter().any(|t| matches!(t, ToastRequest::Anomaly { .. })),
            "no anomaly toast fires while anomaly detection is off by default"
        );
    }

    #[test]
    fn scan_emits_anomaly_toast_when_enabled_and_a_skill_spikes() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        fs::create_dir_all(&claude_home).unwrap();
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        // Turn anomaly on and the budget off, so only the anomaly path can toast.
        adapter.set_usage_settings(&UsageSettings {
            budget_enabled: false,
            budget_work_tokens: 250_000,
            anomaly_enabled: true,
        });
        let day = FIXED_NOW / DAY_MILLIS;
        for d in 1..=7 {
            adapter.usage_cache.ingest(&[usage_row("grilling", 10_000, (day - d) * DAY_MILLIS + 100)]);
        }
        adapter.usage_cache.ingest(&[usage_row("grilling", 500_000, day * DAY_MILLIS + 100)]);

        let out = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime });
        let anomaly = out
            .toasts
            .iter()
            .find(|t| matches!(t, ToastRequest::Anomaly { .. }))
            .expect("a 50x spike must toast when anomaly is enabled");
        match anomaly {
            ToastRequest::Anomaly { skill, window_work, .. } => {
                assert_eq!(skill, "grilling", "the toast names the spiking skill");
                assert_eq!(*window_work, 500_000, "and reports the current-day work");
            }
            _ => unreachable!(),
        }
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
                "  [{:?}] {:<28} always_on={:>4} (exact={}, native={})  on_invoke={:>5}  on_demand={:>6}  usage={:?}",
                skill.kind,
                skill.name,
                skill.always_on.tokens,
                skill.always_on.exact,
                skill.always_on_native,
                skill.on_invoke.tokens,
                skill.on_demand.tokens,
                skill.usage.map(|u| u.work),
            );
        }
        let attributed = report.skills.iter().filter(|s| s.usage.is_some()).count();
        eprintln!("{attributed} of {} skills have attributed usage (issue #5)", report.skills.len());

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
}
