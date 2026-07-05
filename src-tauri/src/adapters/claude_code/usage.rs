use super::footprint_text::{mtime_nanos, TranscriptRef};
use super::usage_cache::{SqliteUsageCache, UsageRow};
use crate::domain::report::{AttributionSource, UsageReport};
use crate::domain::skill::{DiscoveredSkill, SkillId};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;

/// Substring prefilter: only lines carrying a usage block are worth a full
/// parse, so the vast majority of transcript lines are skipped cheaply
/// (mirrors `footprint_text`'s `skill_listing` trick).
const USAGE_MARKER: &str = "\"usage\"";

#[derive(Deserialize)]
struct UsageRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "attributionSkill")]
    attribution_skill: Option<String>,
    #[serde(rename = "attributionPlugin")]
    attribution_plugin: Option<String>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    message: Option<UsageMessage>,
}

#[derive(Deserialize)]
struct UsageMessage {
    id: Option<String>,
    usage: Option<UsageTokens>,
}

#[derive(Deserialize)]
struct UsageTokens {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

/// Parses one transcript's `assistant` records into usage rows, native-first:
/// only records that carry a non-null `attributionSkill`, a `message.id`, and
/// a `message.usage` are emitted. A record with no attribution (absent key or
/// null value) credits nothing -- absence is not a reconstruction trigger,
/// since on current builds it means "no skill was active," and crediting a
/// walked stack there would fabricate attribution Claude withheld (ADR 0005).
/// `work = input + output` only; cache-write and cache-read are separate and
/// cache-read is never folded into work.
///
/// `force_subagent` stamps `is_subagent = true` regardless of the record's own
/// `isSidechain` (issue #13, grill D3): rows parsed out of a sub-agent-
/// enumerated file are sub-agent by provenance, so a record with a missing or
/// mislabeled `isSidechain` can't leak sub-agent tokens into the default
/// headline. Main-thread callers pass `false` and fall back to `isSidechain`.
pub fn parse_usage_rows(content: &str, force_subagent: bool) -> Vec<UsageRow> {
    let mut rows = Vec::new();
    for line in content.lines() {
        if !line.contains(USAGE_MARKER) {
            continue;
        }
        let Ok(record) = serde_json::from_str::<UsageRecord>(line) else { continue };
        if record.kind.as_deref() != Some("assistant") {
            continue;
        }
        let Some(attribution_skill) = record.attribution_skill else { continue };
        let Some(message) = record.message else { continue };
        let Some(message_id) = message.id else { continue };
        let Some(usage) = message.usage else { continue };

        rows.push(UsageRow {
            message_id,
            attribution_skill,
            attribution_plugin: record.attribution_plugin,
            is_subagent: force_subagent || record.is_sidechain,
            work: usage.input_tokens.saturating_add(usage.output_tokens),
            cache_write: usage.cache_creation_input_tokens,
            cache_read: usage.cache_read_input_tokens,
        });
    }
    rows
}

/// The join key between an `attributionSkill` string and a discovered skill.
/// Plugin skills attribute as `plugin:name` with `attributionPlugin` set;
/// personal/project skills attribute as a bare `name` with `attributionPlugin`
/// null. Marketplace is absent from attribution, so it is deliberately not in
/// the key: two plugins named `frontend-design` are told apart by their
/// plugin, and a personal `deploy` and a project `deploy` would collide (a
/// documented MVP limitation, ADR 0005 / ADR 0024).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UsageKey {
    plugin: Option<String>,
    name: String,
}

impl UsageKey {
    fn from_attribution(attribution_skill: &str, attribution_plugin: Option<&str>) -> Self {
        match attribution_skill.split_once(':') {
            Some((prefix, name)) => UsageKey {
                // The record's own attributionPlugin is authoritative; the
                // prefix is only a fallback for a malformed record.
                plugin: attribution_plugin.map(str::to_string).or_else(|| Some(prefix.to_string())),
                name: name.to_string(),
            },
            None => UsageKey { plugin: attribution_plugin.map(str::to_string), name: attribution_skill.to_string() },
        }
    }

    fn from_skill(skill: &DiscoveredSkill) -> Self {
        match &skill.id {
            SkillId::Personal { name } => UsageKey { plugin: None, name: name.clone() },
            SkillId::Project { name, .. } => UsageKey { plugin: None, name: name.clone() },
            SkillId::Plugin { plugin, name, .. } => UsageKey { plugin: Some(plugin.clone()), name: name.clone() },
        }
    }
}

/// Read accounting for the usage pass, so a test can assert a warm rescan
/// re-reads no transcripts.
#[derive(Default, Debug, Clone, Copy)]
pub struct UsageStats {
    pub files_total: usize,
    pub files_read: usize,
}

/// Incremental usage ingest (issue #5, extended for the sub-agent toggle in
/// issue #13). Takes the already-enumerated main-thread refs `scan_all` built
/// for the listing index plus, when the user opted in, the sub-agent refs, so a
/// scan enumerates each dir once. A file is opened only when its `(mtime, size)`
/// changed since last scan; rows are written INSERT OR IGNORE, so a re-read is
/// idempotent and cross-file duplicate `message.id`s (resume/branch/compact)
/// count once.
///
/// The two ref lists carry provenance: `main_transcripts` parse with
/// `force_subagent = false` (they fall back to `isSidechain`), while
/// `subagent_transcripts` parse with `force_subagent = true` so their rows are
/// always tagged `is_subagent` and stay out of the default headline (grill D3).
/// The checkpoint gate is pruned over the UNION of both lists, so a toggle-off
/// scan (empty sub-agent list) prunes the previous run's sub-agent checkpoints
/// -- their `message_usage` rows persist (INSERT OR IGNORE never overwrites), so
/// re-reading them on a later toggle-on stays correct, just not instant (D7).
pub fn refresh_usage(
    main_transcripts: &[TranscriptRef],
    subagent_transcripts: &[TranscriptRef],
    cache: &SqliteUsageCache,
) -> UsageStats {
    let mut stats = UsageStats::default();
    ingest_transcripts(main_transcripts, false, cache, &mut stats);
    ingest_transcripts(subagent_transcripts, true, cache, &mut stats);

    // Prune the checkpoint gate for vanished transcripts, but skip a wholly
    // empty enumeration (a transient read failure) so the whole gate isn't wiped.
    if !main_transcripts.is_empty() || !subagent_transcripts.is_empty() {
        let seen: HashSet<String> = main_transcripts
            .iter()
            .chain(subagent_transcripts)
            .map(|t| t.path.to_string_lossy().into_owned())
            .collect();
        cache.retain(&seen);
    }
    stats
}

/// Reads and ingests each changed transcript in `transcripts`, stamping every
/// emitted row's `is_subagent` when `force_subagent` (its provenance). Shared
/// by the main and sub-agent passes so they gate, read, and mark identically.
fn ingest_transcripts(
    transcripts: &[TranscriptRef],
    force_subagent: bool,
    cache: &SqliteUsageCache,
    stats: &mut UsageStats,
) {
    for transcript in transcripts {
        stats.files_total += 1;
        let path_key = transcript.path.to_string_lossy();
        let size = transcript.size as i64;
        let mnanos = mtime_nanos(transcript.mtime);

        if let Some(m) = mnanos {
            if cache.is_fresh(&path_key, m, size) {
                continue;
            }
        }
        stats.files_read += 1;
        let Ok(content) = fs::read_to_string(&transcript.path) else { continue };
        cache.ingest(&parse_usage_rows(&content, force_subagent));
        // A truncated trailing line fails serde and is skipped; completing it
        // changes `(mtime, size)`, so the file re-reads and the once-partial
        // record then counts exactly once.
        if let Some(m) = mnanos {
            cache.mark(&path_key, m, size);
        }
    }
}

/// The per-attribution totals folded into a lookup keyed by the join key, so
/// `scan_all` can attach each discovered skill's usage. Attribution strings
/// with no matching discovered skill simply never get looked up (dropped, not
/// fabricated).
fn usage_by_key(cache: &SqliteUsageCache, include_subagents: bool) -> HashMap<UsageKey, UsageReport> {
    let mut map: HashMap<UsageKey, UsageReport> = HashMap::new();
    for total in cache.totals(include_subagents) {
        let key = UsageKey::from_attribution(&total.attribution_skill, total.attribution_plugin.as_deref());
        let entry = map.entry(key).or_insert(UsageReport {
            work: 0,
            cache_write: 0,
            cache_read: 0,
            attribution_source: AttributionSource::Native,
        });
        entry.work = entry.work.saturating_add(total.work);
        entry.cache_write = entry.cache_write.saturating_add(total.cache_write);
        entry.cache_read = entry.cache_read.saturating_add(total.cache_read);
    }
    map
}

/// A ready-to-query view of usage, built once per scan.
pub struct UsageIndex {
    by_key: HashMap<UsageKey, UsageReport>,
}

impl UsageIndex {
    pub fn build(cache: &SqliteUsageCache, include_subagents: bool) -> Self {
        UsageIndex { by_key: usage_by_key(cache, include_subagents) }
    }

    /// This skill's attributed usage, or `None` if no session touched it.
    pub fn for_skill(&self, skill: &DiscoveredSkill) -> Option<UsageReport> {
        self.by_key.get(&UsageKey::from_skill(skill)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::super::footprint_text::{subagent_transcript_refs, transcript_refs_by_recency};
    use super::*;
    use crate::domain::skill::{Frontmatter, SkillId};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    /// Enumerate a single dir into the `TranscriptRef`s `refresh_usage` now
    /// takes, re-stat'd fresh so an appended/rewritten file is seen.
    fn refs(dir: &Path) -> Vec<TranscriptRef> {
        transcript_refs_by_recency(&[dir.to_path_buf()])
    }

    #[allow(clippy::too_many_arguments)] // a test fixture builder; each arg is a distinct transcript field
    fn assistant_line(message_id: &str, uuid: &str, skill: Option<&str>, plugin: Option<&str>, input: u32, output: u32, cw: u32, cr: u32) -> String {
        let mut rec = json!({
            "type": "assistant",
            "uuid": uuid,
            "message": {
                "id": message_id,
                "role": "assistant",
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_creation_input_tokens": cw,
                    "cache_read_input_tokens": cr
                }
            }
        });
        match skill {
            Some(s) => rec["attributionSkill"] = json!(s),
            None => rec["attributionSkill"] = serde_json::Value::Null, // key present, value null
        }
        if let Some(p) = plugin {
            rec["attributionPlugin"] = json!(p);
        }
        rec.to_string()
    }

    /// An `assistant_line` carrying `isSidechain: true` -- the shape a sub-agent
    /// transcript actually writes (100% of real sub-agent records, 0% of main).
    #[allow(clippy::too_many_arguments)]
    fn subagent_assistant_line(message_id: &str, uuid: &str, skill: Option<&str>, plugin: Option<&str>, input: u32, output: u32, cw: u32, cr: u32) -> String {
        let mut rec: serde_json::Value =
            serde_json::from_str(&assistant_line(message_id, uuid, skill, plugin, input, output, cw, cr)).unwrap();
        rec["isSidechain"] = json!(true);
        rec.to_string()
    }

    fn skill(id: SkillId) -> DiscoveredSkill {
        DiscoveredSkill {
            id,
            dir_path: PathBuf::from("/tmp/x"),
            skill_md_path: PathBuf::from("/tmp/x/SKILL.md"),
            frontmatter: Frontmatter {
                declared_name: "x".to_string(),
                description: "d".to_string(),
                raw_block: String::new(),
            },
            body: String::new(),
            is_symlink: false,
            symlink_target: None,
            on_demand_files: vec![],
            live: true,
        }
    }

    #[test]
    fn parses_buckets_and_never_folds_cache_read_into_work() {
        let line = assistant_line("msg_1", "u1", Some("grilling"), None, 291, 938, 13781, 35154);
        let rows = parse_usage_rows(&line, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].work, 1229, "work is input + output only");
        assert_eq!(rows[0].cache_write, 13781);
        assert_eq!(rows[0].cache_read, 35154);
        assert_ne!(rows[0].work, 1229 + 35154, "cache_read must never be in work");
    }

    #[test]
    fn a_record_with_no_attribution_credits_nothing() {
        // Null value (key present) and a Skill-invoke-shaped line still yield
        // no rows: native-only never fabricates attribution.
        let null_attr = assistant_line("msg_1", "u1", None, None, 10, 20, 0, 0);
        assert!(parse_usage_rows(&null_attr, false).is_empty());
        // A line with usage but type != assistant is ignored too.
        let user_line = json!({"type":"user","message":{"id":"m","usage":{"input_tokens":5}}}).to_string();
        assert!(parse_usage_rows(&user_line, false).is_empty());
    }

    #[test]
    fn dedup_is_by_message_id_not_record_uuid() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        // Same message.id, DIFFERENT record uuid (a resume copy): count once.
        cache.ingest(&parse_usage_rows(&assistant_line("msg_A", "uuid-1", Some("grilling"), None, 10, 20, 0, 0), false));
        cache.ingest(&parse_usage_rows(&assistant_line("msg_A", "uuid-2", Some("grilling"), None, 10, 20, 0, 0), false));
        let totals = cache.totals(false);
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].work, 30, "keying on uuid would double this to 60");
    }

    #[test]
    fn one_message_split_across_content_blocks_counts_once() {
        // Same message.id repeated (thinking/text/tool_use split), identical usage.
        let content = [
            assistant_line("msg_A", "u1", Some("grilling"), None, 10, 20, 0, 0),
            assistant_line("msg_A", "u1", Some("grilling"), None, 10, 20, 0, 0),
            assistant_line("msg_A", "u1", Some("grilling"), None, 10, 20, 0, 0),
        ]
        .join("\n");
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&parse_usage_rows(&content, false));
        assert_eq!(cache.totals(false)[0].work, 30, "one message must count once, not 90");
    }

    #[test]
    fn native_join_credits_personal_and_plugin_skills() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&parse_usage_rows(&assistant_line("m1", "u1", Some("grilling"), None, 100, 0, 0, 0), false));
        cache.ingest(&parse_usage_rows(&assistant_line(
            "m2", "u2", Some("superpowers:executing-plans"), Some("superpowers"), 50, 0, 0, 0,
        ), false));
        let index = UsageIndex::build(&cache, false);

        let personal = skill(SkillId::Personal { name: "grilling".to_string() });
        assert_eq!(index.for_skill(&personal).unwrap().work, 100);

        let plugin = skill(SkillId::Plugin {
            marketplace: "official".to_string(),
            plugin: "superpowers".to_string(),
            name: "executing-plans".to_string(),
        });
        // Credited even though the record carries no marketplace.
        assert_eq!(index.for_skill(&plugin).unwrap().work, 50);
    }

    #[test]
    fn two_plugins_with_the_same_skill_name_are_told_apart_by_plugin() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&parse_usage_rows(&assistant_line("m1", "u1", Some("impeccable:frontend-design"), Some("impeccable"), 43, 0, 0, 0), false));
        cache.ingest(&parse_usage_rows(&assistant_line("m2", "u2", Some("frontend-design:frontend-design"), Some("frontend-design"), 187, 0, 0, 0), false));
        let index = UsageIndex::build(&cache, false);

        let a = skill(SkillId::Plugin { marketplace: "mp".to_string(), plugin: "impeccable".to_string(), name: "frontend-design".to_string() });
        let b = skill(SkillId::Plugin { marketplace: "mp".to_string(), plugin: "frontend-design".to_string(), name: "frontend-design".to_string() });
        assert_eq!(index.for_skill(&a).unwrap().work, 43, "must not merge under a name-only join");
        assert_eq!(index.for_skill(&b).unwrap().work, 187);
    }

    #[test]
    fn attribution_with_no_matching_skill_is_dropped_not_fabricated() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&parse_usage_rows(&assistant_line("m1", "u1", Some("loop"), None, 100, 0, 0, 0), false));
        let index = UsageIndex::build(&cache, false);
        // A discovered skill that was never attributed gets None, not a zero row.
        let other = skill(SkillId::Personal { name: "grilling".to_string() });
        assert!(index.for_skill(&other).is_none());
    }

    fn write_transcript(dir: &Path, name: &str, lines: &[String]) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), format!("{}\n", lines.join("\n"))).unwrap();
    }

    #[test]
    fn warm_rescan_reads_no_files_and_totals_are_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        write_transcript(&dir, "s.jsonl", &[assistant_line("m1", "u1", Some("grilling"), None, 10, 5, 0, 0)]);
        let cache = SqliteUsageCache::open_in_memory().unwrap();

        let cold = refresh_usage(&refs(&dir), &[], &cache);
        assert_eq!(cold.files_read, 1);
        let cold_total = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;

        let warm = refresh_usage(&refs(&dir), &[], &cache);
        assert_eq!(warm.files_read, 0, "an unchanged transcript is not re-read");
        let warm_total = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(cold_total, warm_total, "warm totals must be byte-identical (idempotent)");
        assert_eq!(warm_total, 15);
    }

    #[test]
    fn an_appended_record_re_reads_only_that_file_and_counts_the_new_message_once() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let path = dir.join("s.jsonl");
        write_transcript(&dir, "s.jsonl", &[assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0)]);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        refresh_usage(&refs(&dir), &[], &cache);

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", assistant_line("m2", "u2", Some("grilling"), None, 7, 0, 0, 0)).unwrap();
        drop(f);

        let stats = refresh_usage(&refs(&dir), &[], &cache);
        assert_eq!(stats.files_read, 1, "the grown file is re-read");
        let total = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(total, 17, "m1 (10) counted once + m2 (7); no double count of m1 on the re-read");
    }

    #[test]
    fn a_truncated_final_line_is_safe_and_counts_once_when_completed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let path = dir.join("s.jsonl");
        fs::create_dir_all(&dir).unwrap();
        let good = assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0);
        // Second line is a truncated (mid-write) JSON object.
        fs::write(&path, format!("{good}\n{{\"type\":\"assist")).unwrap();
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        refresh_usage(&refs(&dir), &[], &cache);
        let after_partial = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(after_partial, 10, "the partial line is skipped, not counted");

        // Complete the file; the once-partial record now counts exactly once.
        let m2 = assistant_line("m2", "u2", Some("grilling"), None, 5, 0, 0, 0);
        fs::write(&path, format!("{good}\n{m2}\n")).unwrap();
        refresh_usage(&refs(&dir), &[], &cache);
        let after_complete = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(after_complete, 15);
    }

    #[test]
    fn the_same_message_id_across_two_files_counts_once() {
        // File b is a resume of a and re-contains a's message.id, exercising
        // the cross-file dedup end to end (not just at the ingest layer).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        write_transcript(&dir, "a.jsonl", &[assistant_line("msg_A", "u1", Some("grilling"), None, 10, 0, 0, 0)]);
        write_transcript(
            &dir,
            "b.jsonl",
            &[
                assistant_line("msg_A", "u1-copy", Some("grilling"), None, 10, 0, 0, 0), // resume copy
                assistant_line("msg_B", "u2", Some("grilling"), None, 7, 0, 0, 0),        // new
            ],
        );
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        refresh_usage(&refs(&dir), &[], &cache);

        let total = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(total, 17, "msg_A (10) counted once across both files + msg_B (7)");
    }

    #[test]
    fn a_subagent_file_in_a_subdir_is_not_read_by_the_default_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        write_transcript(&dir, "main.jsonl", &[assistant_line("m_main", "u1", Some("grilling"), None, 10, 0, 0, 0)]);
        // A sub-agent transcript nested under <session>/subagents/ (where Claude
        // Code writes them). The depth-1 enumeration must never descend here.
        let sub_dir = dir.join("session-x").join("subagents");
        write_transcript(&sub_dir, "agent-1.jsonl", &[assistant_line("m_sub", "u2", Some("grilling"), None, 999, 0, 0, 0)]);

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        let stats = refresh_usage(&refs(&dir), &[], &cache);

        assert_eq!(stats.files_read, 1, "only the depth-1 main.jsonl is enumerated, never the subagents/ file");
        let total = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(total, 10, "the sub-agent file's 999 tokens are excluded by default");
    }

    // ---- issue #13: sub-agent enumeration + the include toggle ----

    fn grilling() -> DiscoveredSkill {
        skill(SkillId::Personal { name: "grilling".to_string() })
    }

    #[test]
    fn subagent_transcript_refs_collects_agent_files_at_both_depths() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        let subagents = project_dir.join("session-x").join("subagents");
        // Depth 1: <session>/subagents/agent-*.jsonl
        write_transcript(&subagents, "agent-1.jsonl", &[subagent_assistant_line("m1", "u1", Some("grilling"), None, 1, 0, 0, 0)]);
        // Depth 2: <session>/subagents/workflows/wf_*/agent-*.jsonl
        let wf = subagents.join("workflows").join("wf_abc");
        write_transcript(&wf, "agent-2.jsonl", &[subagent_assistant_line("m2", "u2", Some("grilling"), None, 1, 0, 0, 0)]);

        let refs = subagent_transcript_refs(std::slice::from_ref(&project_dir));
        assert_eq!(refs.len(), 2, "collects agent-*.jsonl from subagents/ and subagents/workflows/wf_*/");
    }

    #[test]
    fn subagent_enumeration_excludes_journal_and_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo-a");
        let subagents = project_dir.join("session-x").join("subagents");
        write_transcript(&subagents, "agent-1.jsonl", &[subagent_assistant_line("m1", "u1", Some("grilling"), None, 1, 0, 0, 0)]);
        // A sibling meta file (wrong extension) and a journal (wrong prefix)
        // both live in the same dir and must be skipped.
        fs::write(subagents.join("agent-1.meta.json"), "{}").unwrap();
        fs::write(subagents.join("journal.jsonl"), "{}\n").unwrap();

        let refs = subagent_transcript_refs(std::slice::from_ref(&project_dir));
        assert_eq!(refs.len(), 1, "only agent-*.jsonl counts; agent-*.meta.json and journal.jsonl are excluded");
        assert!(refs[0].path.ends_with("agent-1.jsonl"));
    }

    #[test]
    fn included_subagent_work_is_credited_via_native_attribution() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        write_transcript(&dir, "main.jsonl", &[assistant_line("m_main", "u1", Some("grilling"), None, 10, 0, 0, 0)]);
        let subagents = dir.join("session-x").join("subagents");
        write_transcript(&subagents, "agent-1.jsonl", &[subagent_assistant_line("m_sub", "u2", Some("grilling"), None, 999, 0, 0, 0)]);

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        refresh_usage(&refs(&dir), &subagent_transcript_refs(std::slice::from_ref(&dir)), &cache);

        let default = UsageIndex::build(&cache, false).for_skill(&grilling()).unwrap().work;
        assert_eq!(default, 10, "sub-agent work stays out of the default headline");
        let included = UsageIndex::build(&cache, true).for_skill(&grilling()).unwrap().work;
        assert_eq!(included, 1009, "toggle on adds the sub-agent file's own 999 to the main 10");
    }

    #[test]
    fn an_unattributed_subagent_record_contributes_nothing_even_when_included() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let subagents = dir.join("session-x").join("subagents");
        // isSidechain true but attributionSkill null: native-first credits it nothing.
        let mut rec: serde_json::Value =
            serde_json::from_str(&assistant_line("m_sub", "u1", None, None, 999, 0, 0, 0)).unwrap();
        rec["isSidechain"] = json!(true);
        write_transcript(&subagents, "agent-1.jsonl", &[rec.to_string()]);

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        refresh_usage(&[], &subagent_transcript_refs(std::slice::from_ref(&dir)), &cache);

        assert!(
            UsageIndex::build(&cache, true).for_skill(&grilling()).is_none(),
            "an unattributed sub-agent record credits nothing, even with the toggle on"
        );
    }

    #[test]
    fn subagent_provenance_forces_is_subagent_even_when_the_sidechain_flag_is_absent() {
        // grill D3: a row parsed out of a sub-agent-enumerated file is
        // sub-agent by provenance, so even a main-shaped record (no isSidechain)
        // must stay out of the default headline and surface only when included.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let subagents = dir.join("session-x").join("subagents");
        write_transcript(&subagents, "agent-1.jsonl", &[assistant_line("m_sub", "u1", Some("grilling"), None, 777, 0, 0, 0)]);

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        refresh_usage(&[], &subagent_transcript_refs(std::slice::from_ref(&dir)), &cache);

        assert!(
            UsageIndex::build(&cache, false).for_skill(&grilling()).is_none(),
            "provenance stamps is_subagent=true, so the default headline excludes it despite the missing flag"
        );
        assert_eq!(
            UsageIndex::build(&cache, true).for_skill(&grilling()).unwrap().work,
            777,
            "it is credited only when sub-agents are included"
        );
    }

    #[test]
    fn subagent_dedup_is_by_message_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let subagents = dir.join("session-x").join("subagents");
        write_transcript(
            &subagents,
            "agent-1.jsonl",
            &[
                subagent_assistant_line("m_sub", "u1", Some("grilling"), None, 50, 0, 0, 0),
                subagent_assistant_line("m_sub", "u2", Some("grilling"), None, 50, 0, 0, 0), // same message.id
            ],
        );
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        refresh_usage(&[], &subagent_transcript_refs(std::slice::from_ref(&dir)), &cache);

        assert_eq!(
            UsageIndex::build(&cache, true).for_skill(&grilling()).unwrap().work,
            50,
            "a repeated sub-agent message.id counts once, not twice"
        );
    }

    #[test]
    fn subagent_cost_is_summed_from_the_file_not_parent_tool_use_result() {
        // Regression guard (grill D9): usage is summed from the sub-agent file's
        // OWN message.usage, never a parent's toolUseResult.totalTokens. It holds
        // by construction (the parser never reads toolUseResult); this pins it so
        // a future change that starts reading toolUseResult trips a red test.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let subagents = dir.join("session-x").join("subagents");
        let own = subagent_assistant_line("m_sub", "u1", Some("grilling"), None, 999, 0, 0, 0);
        // A parent-style toolUseResult carrying a wildly larger total that must
        // never be credited (it passes the "usage" prefilter but is type=user).
        let tool_use_result =
            json!({"type":"user","toolUseResult":{"totalTokens":999_999,"usage":{"input_tokens":999_999,"output_tokens":0}}}).to_string();
        write_transcript(&subagents, "agent-1.jsonl", &[own, tool_use_result]);

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        refresh_usage(&[], &subagent_transcript_refs(std::slice::from_ref(&dir)), &cache);

        assert_eq!(
            UsageIndex::build(&cache, true).for_skill(&grilling()).unwrap().work,
            999,
            "credited from the file's own usage (999), never the toolUseResult total (999,999)"
        );
    }
}
