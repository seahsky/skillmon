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

use crate::domain::budget::{
    detect_anomaly, evaluate_budget, BudgetConfig, ToastRequest, ANOMALY_WINDOW_DAYS, DEFAULT_ANOMALY_FLOOR,
    DEFAULT_ANOMALY_MULTIPLIER, DEFAULT_BUDGET_WORK_TOKENS,
};
use crate::domain::footprint::{AlwaysOnFootprint, Footprint, LayerCount, AlwaysOnTextKind, TokenSource};
use crate::domain::harness::HarnessAdapter;
use crate::domain::report::{ScanReport, SkillReport, UsageSettings};
use crate::domain::scan::{ScanOutcome, ScanParams, UsageWindow, DAY_MILLIS, HOUR_MILLIS};
use crate::domain::skill::{DependentIndex, DiscoveredSkill, DiscoveryResult, SkillId};
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
    always_on_text_from_index, mtime_nanos, subagent_transcript_refs, transcript_refs_by_recency,
    AlwaysOnText, ListingIndex,
};
use listing_cache::SqliteListingCache;
use on_demand_cache::SqliteOnDemandCache;
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
        // A never-listed skill (issue #24) is a known zero, so it is built
        // rather than counted: `count` would hash and, with a key configured,
        // send empty text to `count_tokens`. The zero is Exact because it is
        // certain -- it is the one count that needs no measuring.
        let always_on_count = if always_on.kind == AlwaysOnTextKind::NotListed {
            LayerCount { tokens: 0, source: TokenSource::Exact }
        } else {
            self.count(&always_on.text)
        };
        let on_invoke_count = self.count(&footprint_text::on_invoke_text(skill));
        Footprint {
            always_on: AlwaysOnFootprint { count: always_on_count, text_kind: always_on.kind },
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

    /// Where a removed entry of this skill is staged (ADR 0029).
    ///
    /// The adapter's whole contribution to the removal seam: `removal::remove`
    /// takes a storage root and names no Claude Code path itself (ADR 0002), so
    /// the fact that one lives under `~/.claude` and another inside a repo is
    /// resolved here.
    ///
    /// A project skill stays inside its own repo, preserving ADR 0007's project
    /// locality -- moving it to `~` would smuggle a repo's file out of the repo,
    /// and restoring it would depend on a home directory the repo knows nothing
    /// about. That is also what makes ADR 0029's cross-device fallback real
    /// rather than theoretical: a cascade spanning a manager root under `~` and a
    /// dependent in a repo on another volume crosses filesystems by construction.
    pub fn storage_root_for(&self, id: &SkillId) -> PathBuf {
        match id {
            SkillId::Project { repo_path, .. } => paths::repo_removed_dir(repo_path),
            SkillId::Personal { .. } | SkillId::Plugin { .. } => paths::removed_dir(&self.claude_home),
        }
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
    ///
    /// The batched pass avoids the trait's naive per-skill default (which would
    /// re-read every transcript once per skill -- tens of GB on a real machine):
    /// enumerate repos once, build the skill-listing index once over exactly the
    /// discovered skill names, then resolve each skill's always-on text from that
    /// index. Cost drops from O(skills × transcripts) reads to O(transcripts).
    ///
    /// `params.include_subagents` (issue #13) only widens the attributed-usage
    /// pass; the listing index and its retain set stay MAIN-THREAD ONLY
    /// regardless (grill D4), so a sub-agent file's own `skill_listing` can never
    /// pollute always-on or evict the listing memo.
    pub fn scan(&self, params: &ScanParams) -> ScanOutcome {
        let discovery = ClaudeCodeAdapter::discover_skills(self);
        let known_repos = enumerate_known_repos(&self.claude_home);

        let all_project_dirs: Vec<PathBuf> = known_repos.iter().map(|r| r.project_dir.clone()).collect();
        let wanted: HashSet<String> =
            discovery.skills.iter().map(|s| s.directory_name().to_string()).collect();
        let (transcripts, enumerated_dirs) = transcript_refs_by_recency(&all_project_dirs);
        let (index, _stats) = ListingIndex::build_incremental(&transcripts, &wanted, &self.listing_cache);
        // Bound the memo (see the former scan_all): only prune when the
        // enumeration is non-empty, so a transient read failure doesn't wipe it.
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
        // listing index already built. When the user opts in (issue #13), the
        // sub-agent transcripts are enumerated as a SECOND provenance-tagged list
        // and folded in; these refs feed only the usage pass, never the listing
        // index or its retain set above (grill D4). The successfully-enumerated
        // dirs are threaded through so the usage prune (issue #15) can tell a
        // genuine deletion from a transient read failure on one dir.
        let subagent_transcripts =
            if params.include_subagents { subagent_transcript_refs(&all_project_dirs) } else { Vec::new() };
        usage::refresh_usage(&transcripts, &subagent_transcripts, &enumerated_dirs, &self.usage_cache);

        // The per-skill display figures depend on the requested window (issue
        // #14), and honor the same sub-agent toggle; the 24h budget below is
        // independent and always 24h (DESIGN.md UX #4).
        let (usage_index, usage_window_hours) = match params.usage_window {
            UsageWindow::AllTime => (UsageIndex::build(&self.usage_cache, params.include_subagents), None),
            UsageWindow::Rolling { hours } => {
                let cutoff = params.now_millis - (hours as i64) * HOUR_MILLIS;
                (UsageIndex::build_windowed(&self.usage_cache, cutoff, params.include_subagents), Some(hours))
            }
        };

        // Built over the whole discovery, not per scan root: the ancestor test
        // is about where content resolves, and nothing says a manager root and
        // the skills resolving into it were found by the same pass (ADR 0026).
        let dependents = DependentIndex::build(&discovery.skills);

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
                SkillReport::from_parts(
                    skill,
                    &footprint,
                    usage_index.for_skill(skill),
                    dependents.for_skill(skill),
                )
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
    /// unaffected (the trait stays untouched, ADR 0002). `include_subagents`
    /// (issue #13) flows straight through to the usage pass.
    fn scan_all(&self, include_subagents: bool) -> ScanReport {
        self.scan(&ScanParams { include_subagents, ..ScanParams::all_time() }).report
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

    /// ADR 0007's project locality, which is the whole reason this is the
    /// adapter's job rather than `removal`'s: a repo's skill is staged inside
    /// that repo, and only a personal or plugin entry goes under `~/.claude`.
    /// Staging a repo's file under `~` would smuggle it out of the repo and make
    /// its restore depend on a home directory the repo knows nothing about.
    #[test]
    fn a_project_skill_is_staged_inside_its_own_repo_and_the_rest_under_claude_home() {
        let adapter = ClaudeCodeAdapter::for_discovery_only(PathBuf::from("/home/me/.claude"));

        assert_eq!(
            adapter.storage_root_for(&SkillId::Project {
                repo_path: PathBuf::from("/work/some-repo"),
                name: "deploy".to_string(),
            }),
            PathBuf::from("/work/some-repo/.claude/skillmon/removed"),
        );
        assert_eq!(
            adapter.storage_root_for(&SkillId::Personal { name: "grilling".to_string() }),
            PathBuf::from("/home/me/.claude/skillmon/removed"),
        );
        assert_eq!(
            adapter.storage_root_for(&SkillId::Plugin {
                marketplace: "official".to_string(),
                plugin: "superpowers".to_string(),
                name: "brainstorming".to_string(),
            }),
            PathBuf::from("/home/me/.claude/skillmon/removed"),
        );
    }

    fn write_skill(dir: &std::path::Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: a test skill\n---\n\nBody.\n"),
        )
        .unwrap();
    }

    /// A short kind label for the diagnostic tables the `#[ignore]`d real-home
    /// tests print. Debug-printing the whole ref would wrap those lines.
    fn skill_kind_label(id: &crate::domain::report::SkillRef) -> &'static str {
        use crate::domain::report::SkillRef;
        match id {
            SkillRef::Personal { .. } => "personal",
            SkillRef::Project { .. } => "project",
            SkillRef::Plugin { .. } => "plugin",
        }
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
            reconstructed: false,
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
        use crate::domain::footprint::AlwaysOnTextKind;
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

        assert_eq!(footprint.always_on.text_kind, AlwaysOnTextKind::Native);
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
        use crate::domain::report::SkillRef;

        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        write_skill(&claude_home.join("skills").join("alpha"), "alpha");
        write_skill(&claude_home.join("skills").join("beta"), "beta");

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all(false);

        assert_eq!(report.skills.len(), 2);
        assert!(report.skills.iter().all(|s| matches!(s.id, SkillRef::Personal { .. })));
        let names: Vec<&str> = report.skills.iter().map(|s| s.id.name()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        // No key configured, so every layer is the estimate tier.
        assert!(report.skills.iter().all(|s| !s.always_on.exact && !s.on_invoke.exact));
    }

    /// The end-to-end gate for issue #27: a manager root computed by discovery
    /// has to reach the panel, over real symlinks in both shapes a managed skill
    /// takes on disk (ADR 0026, issue #25). Reproduced here rather than left to
    /// the `#[ignore]`d real-home run, which only covers whichever shapes that
    /// machine happens to have.
    #[test]
    #[cfg(unix)]
    fn scan_all_carries_the_manager_root_of_both_managed_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        let skills = claude_home.join("skills");
        fs::create_dir_all(&skills).unwrap();

        // Unmanaged: a real directory holding a real SKILL.md.
        write_skill(&skills.join("vercel-react"), "vercel-react");

        // gstack's shape: a real directory whose SKILL.md links into a checkout.
        let checkout = tmp.path().join("gstack-checkout").join("skills").join("ship");
        write_skill(&checkout, "ship");
        fs::create_dir_all(skills.join("ship")).unwrap();
        std::os::unix::fs::symlink(checkout.join("SKILL.md"), skills.join("ship").join("SKILL.md")).unwrap();

        // `.agents`' shape: the entry directory is itself the symlink.
        let agents = tmp.path().join("agents-home").join("skills").join("tdd");
        write_skill(&agents, "tdd");
        std::os::unix::fs::symlink(&agents, skills.join("tdd")).unwrap();

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all(false);

        let root_of = |name: &str| {
            report
                .skills
                .iter()
                .find(|s| s.id.name() == name)
                .unwrap_or_else(|| panic!("{name} was not discovered"))
                .manager_root
                .clone()
        };

        // canonicalize: the tempdir path itself may run through a symlink (on
        // macOS /tmp is one), and the manager root is resolved, so the expected
        // value has to be resolved too.
        let expected = |p: &std::path::Path| Some(fs::canonicalize(p).unwrap().display().to_string());

        assert_eq!(root_of("ship"), expected(checkout.parent().unwrap()));
        assert_eq!(root_of("tdd"), expected(agents.parent().unwrap()));
        assert_eq!(root_of("vercel-react"), None, "a skill owning its own content has no manager root");
    }

    /// The reference machine's most destructive row, end to end (issue #30):
    /// `~/.claude/skills/gstack` is a skill in its own right AND the checkout
    /// every shim resolves into. `manager_root: None` and `provides_for: 2` have
    /// to reach the panel together -- either alone describes the row as harmless
    /// (ADR 0026), and removing it is a tool uninstall, not a skill removal
    /// (ADR 0027).
    #[test]
    #[cfg(unix)]
    fn scan_all_counts_the_shims_resolving_into_a_checkout_that_is_itself_a_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        let skills = claude_home.join("skills");
        fs::create_dir_all(&skills).unwrap();

        // The checkout sits in the scan root, so it is discovered as a skill of
        // its own -- and its content is genuinely its own, so it is unmanaged.
        let checkout = skills.join("gstack");
        write_skill(&checkout, "gstack");
        // Nested a directory deeper than the shims' entries, which is what makes
        // this the ancestor test and not path equality.
        for shim_name in ["ship", "review"] {
            let real = checkout.join("skills").join("engineering").join(shim_name);
            write_skill(&real, shim_name);
            fs::create_dir_all(skills.join(shim_name)).unwrap();
            std::os::unix::fs::symlink(real.join("SKILL.md"), skills.join(shim_name).join("SKILL.md")).unwrap();
        }
        // A skill that resolves nowhere near the checkout, so the count is a
        // count and not just "every other row".
        write_skill(&skills.join("vercel-react"), "vercel-react");

        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let report = adapter.scan_all(false);

        let row = |name: &str| {
            report
                .skills
                .iter()
                .find(|s| s.id.name() == name)
                .unwrap_or_else(|| panic!("{name} was not discovered"))
        };

        assert_eq!(row("gstack").provides_for, 2);
        assert_eq!(row("gstack").manager_root, None, "the checkout owns its own content");
        assert_eq!(row("ship").provides_for, 0, "a shim is nobody's manager root");
        assert_eq!(row("vercel-react").provides_for, 0);
    }

    #[test]
    fn batched_scan_all_resolves_native_always_on_like_the_per_skill_path() {
        use crate::domain::footprint::AlwaysOnTextKind;

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

        let grilling = report.skills.iter().find(|s| s.id.name() == "grilling").unwrap();
        assert_eq!(
            grilling.always_on_text,
            AlwaysOnTextKind::Native,
            "batched scan should source always-on from the transcript (Native)"
        );

        // And it agrees with the single-skill path's confidence + tokens.
        let discovery = adapter.discover_skills();
        let skill = discovery.skills.iter().find(|s| s.directory_name() == "grilling").unwrap();
        let per_skill = adapter.compute_footprint(skill);
        assert_eq!(per_skill.always_on.text_kind, AlwaysOnTextKind::Native);
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
        let grilling = report.skills.iter().find(|s| s.id.name() == "grilling").unwrap();
        assert_eq!(grilling.usage.unwrap().work, 10, "the default scan never reads the sub-agent file");
    }

    #[test]
    fn scan_all_includes_subagent_usage_when_toggled_on() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = claude_home_with_grilling_usage(tmp.path());
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);

        let report = adapter.scan_all(true);
        let grilling = report.skills.iter().find(|s| s.id.name() == "grilling").unwrap();
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

        let grilling = report.skills.iter().find(|s| s.id.name() == "grilling").unwrap();
        assert_eq!(
            grilling.always_on_text,
            AlwaysOnTextKind::Reconstructed,
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
        let report = adapter.scan_all(false);

        let grilling = report.skills.iter().find(|s| s.id.name() == "grilling").unwrap();
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

        let all = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime, include_subagents: false });
        let g_all = all.report.skills.iter().find(|s| s.id.name() == "grilling").unwrap();
        assert_eq!(g_all.usage.unwrap().work, 140, "all-time sums both records");
        assert_eq!(all.report.usage_window_hours, None);

        let win = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::Rolling { hours: 24 }, include_subagents: false });
        let g_win = win.report.skills.iter().find(|s| s.id.name() == "grilling").unwrap();
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

        let params = ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime, include_subagents: false };
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
        let params = ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime, include_subagents: false };
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
        let out = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime, include_subagents: false });
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

        let out = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime, include_subagents: false });
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

        let out = adapter.scan(&ScanParams { now_millis: FIXED_NOW, usage_window: UsageWindow::AllTime, include_subagents: false });
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

    /// Reports how this machine's real `~/.claude` breaks down by always-on
    /// text kind (issue #24), manager root (issue #25), and dependents (issue
    /// #30) -- the CLAUDE.md verification bar for each, which unit tests over
    /// tempdirs cannot meet.
    /// The populations are printed rather than asserted, being one developer's
    /// disk rather than an invariant; what *is* asserted is the per-skill
    /// invariant #24 turns on, which holds on any machine. Run by hand:
    /// `cargo test --manifest-path src-tauri/Cargo.toml
    /// adapters::claude_code::tests::real_claude_home_skill_provenance -- --ignored --exact --nocapture`
    #[test]
    #[ignore]
    fn real_claude_home_skill_provenance() {
        use std::collections::BTreeMap;

        let claude_home = crate::adapters::claude_code::paths::default_claude_home();
        let adapter = ClaudeCodeAdapter::for_discovery_only(claude_home);
        let discovery = adapter.discover_skills();

        let mut by_root: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for skill in &discovery.skills {
            let root = match &skill.manager_root {
                Some(root) => root.display().to_string(),
                None => "<unmanaged>".to_string(),
            };
            by_root.entry(root).or_default().push(skill.directory_name());
        }
        eprintln!("\n=== manager roots (issue #25) ===");
        for (root, mut names) in by_root {
            names.sort_unstable();
            eprintln!("  {:>3}  {}", names.len(), root);
            eprintln!("       e.g. {}", names.iter().take(4).cloned().collect::<Vec<_>>().join(", "));
        }

        // Issue #30's half of the pair: which rows other rows resolve into. The
        // count is what makes an unmanaged row's removal a tool uninstall rather
        // than a skill removal (ADR 0027), so it is walked over real symlinks
        // here, not just tempdir ones.
        let dependents = DependentIndex::build(&discovery.skills);
        eprintln!("\n=== dependents (issue #30) ===");
        let mut providers: Vec<(&DiscoveredSkill, u32)> = discovery
            .skills
            .iter()
            .map(|s| (s, dependents.for_skill(s)))
            .filter(|(_, count)| *count > 0)
            .collect();
        providers.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        if providers.is_empty() {
            eprintln!("  no skill on this machine is a manager root for another");
        }
        for (skill, count) in &providers {
            eprintln!("  {:>3}  {}  ({})", count, skill.directory_name(), skill.canonical_dir.display());
        }
        for skill in &discovery.skills {
            // True on any machine, and the guard against the one shape that
            // would break it: an entry whose SKILL.md resolves inside its own
            // directory would otherwise count itself.
            assert!(
                (dependents.for_skill(skill) as usize) < discovery.skills.len().max(1),
                "{} cannot provide for every discovered skill including itself",
                skill.directory_name()
            );
        }

        let never_listed: Vec<&str> = discovery
            .skills
            .iter()
            .filter(|s| !s.frontmatter.model_invocable)
            .map(|s| s.directory_name())
            .collect();
        eprintln!("\n=== never listed, so always-on is zero (issue #24) ===");
        eprintln!("  {} of {} skills: {:?}", never_listed.len(), discovery.skills.len(), never_listed);

        // The bug itself: these used to be billed a reconstructed bullet. The
        // populations above are one machine's facts, but this is an invariant
        // that holds on any -- vacuously, on a machine with no such skill.
        eprintln!("\n=== their real footprints ===");
        for skill in discovery.skills.iter().filter(|s| !s.frontmatter.model_invocable) {
            let footprint = adapter.compute_footprint(skill);
            eprintln!(
                "  {:<28} always_on={} tokens (exact={}, {:?})  on_invoke={}",
                skill.directory_name(),
                footprint.always_on.count.tokens,
                footprint.always_on.count.source == TokenSource::Exact,
                footprint.always_on.text_kind,
                footprint.on_invoke.tokens,
            );
            assert_eq!(
                footprint.always_on.text_kind,
                AlwaysOnTextKind::NotListed,
                "{} declares disable-model-invocation, so it has no listing line",
                skill.directory_name()
            );
            assert_eq!(footprint.always_on.count.tokens, 0, "a never-listed skill costs no always-on");
            assert_eq!(
                footprint.always_on.count.source,
                TokenSource::Exact,
                "the zero is certain, so it must not be marked an estimate"
            );
            assert!(
                footprint.on_invoke.tokens > 0,
                "it is still slash-invokable, so on-invoke is untouched"
            );
        }

        // Issue #27: the same facts have to survive the trip to the panel. The
        // gap this closes is that they did not -- discovery computed a manager
        // root and then dropped it at the DTO, leaving the source column (#30)
        // and removal (#31) with nothing to read.
        let mut managed = 0usize;
        let mut mismatched: Vec<(String, String)> = Vec::new();
        for skill in &discovery.skills {
            let report = SkillReport::from_parts(skill, &adapter.compute_footprint(skill), None, 0);

            assert_eq!(
                SkillId::from(report.id.clone()),
                skill.id,
                "{}: the ref the panel holds must resolve back to the skill it names",
                skill.directory_name()
            );
            assert_eq!(
                report.manager_root,
                skill.manager_root.as_ref().map(|p| p.display().to_string()),
                "{}: the manager root must reach the panel intact",
                skill.directory_name()
            );

            if report.manager_root.is_some() {
                managed += 1;
            }
            if report.name_mismatch {
                mismatched.push((report.id.name().to_string(), report.declared_name.clone()));
            }
        }
        eprintln!("\n=== what crosses to the panel (issue #27) ===");
        eprintln!("  {managed} of {} rows carry a manager root", discovery.skills.len());
        eprintln!("  {} rows declare a name other than their directory:", mismatched.len());
        for (dir, declared) in &mismatched {
            eprintln!("       {dir:<28} declares {declared}");
        }
        eprintln!();
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
        let resolved = adapter.scan_all(false);
        assert!(
            resolved.skills.iter().all(|s| s.on_demand.is_some()),
            "after the background fill no skill should still be pending"
        );
        assert!(adapter.pending_on_demand().is_empty(), "the fill must reach a steady state");
        eprintln!("background fill wrote at least one ceiling: {filled}");

        for skill in resolved.skills.iter().take(10) {
            eprintln!(
                "  [{:<8}] {:<28} always_on={:>4} (exact={}, text={:?})  on_invoke={:>5}  on_demand={:?}  usage={:?}",
                skill_kind_label(&skill.id),
                skill.id.name(),
                skill.always_on.tokens,
                skill.always_on.exact,
                skill.always_on_text,
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

        let report = adapter.scan_all(false);
        let grilling = report.skills.iter().find(|s| s.id.name() == "grilling").unwrap();

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
        let report = adapter.scan_all(false);
        let skill = report.skills.iter().find(|s| s.id.name() == "no-refs").unwrap();

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
        let report = adapter.scan_all(false);
        let skill = report.skills.iter().find(|s| s.id.name() == "has-refs").unwrap();

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
        let cold = adapter1.scan_all(false);
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

        let warm = adapter2.scan_all(false);
        let grilling = warm.skills.iter().find(|s| s.id.name() == "grilling").unwrap();
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
        let report = adapter.scan_all(false);
        assert!(report.skills.iter().all(|s| s.on_demand.is_some()), "every skill is resolved after the fill");
    }
}
