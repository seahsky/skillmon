use super::footprint_text::{mtime_nanos, TranscriptRef};
use super::usage_cache::{SqliteUsageCache, UsageRow, UsageTotal};
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
    /// The record's top-level RFC3339 timestamp (`"...Z"`, ms precision UTC).
    /// Optional so a record without one parses instead of failing (issue #14).
    #[serde(default)]
    timestamp: Option<String>,
    message: Option<UsageMessage>,
}

/// Parses a transcript record's RFC3339 timestamp to unix epoch millis, e.g.
/// `2026-06-27T02:13:52.480Z` -> `1782526432480`. `None` for a malformed value
/// so the caller can fall back to 0 (oldest) rather than drop the row.
pub fn parse_iso8601_millis(s: &str) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;
    let odt = OffsetDateTime::parse(s, &Rfc3339).ok()?;
    i64::try_from(odt.unix_timestamp_nanos() / 1_000_000).ok()
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
pub fn parse_usage_rows(content: &str) -> Vec<UsageRow> {
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
            is_subagent: record.is_sidechain,
            work: usage.input_tokens.saturating_add(usage.output_tokens),
            cache_write: usage.cache_creation_input_tokens,
            cache_read: usage.cache_read_input_tokens,
            // Missing/malformed timestamp -> 0 (oldest); never drop the row, so
            // it still counts all-time and just never lands in a recent window.
            timestamp_millis: record.timestamp.as_deref().and_then(parse_iso8601_millis).unwrap_or(0),
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

/// Incremental usage ingest over `transcripts` (issue #5). Takes the
/// already-enumerated main-thread refs `scan_all` built for the listing index,
/// so a scan enumerates the transcript dirs once, not twice. A file is opened
/// only when its `(mtime, size)` changed since last scan; rows are written
/// INSERT OR IGNORE, so a re-read is idempotent and cross-file duplicate
/// `message.id`s (resume/branch/compact) count once. Sub-agent files live under
/// `subagents/` subdirs, which the enumeration does not descend into, so they
/// are excluded by default for free.
pub fn refresh_usage(transcripts: &[TranscriptRef], cache: &SqliteUsageCache) -> UsageStats {
    let mut stats = UsageStats::default();

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
        cache.ingest(&parse_usage_rows(&content));
        // A truncated trailing line fails serde and is skipped; completing it
        // changes `(mtime, size)`, so the file re-reads and the once-partial
        // record then counts exactly once.
        if let Some(m) = mnanos {
            cache.mark(&path_key, m, size);
        }
    }

    // Prune the checkpoint gate for vanished transcripts, but skip an empty
    // enumeration (a transient read failure) so the whole gate isn't wiped.
    if !transcripts.is_empty() {
        let seen: HashSet<String> =
            transcripts.iter().map(|t| t.path.to_string_lossy().into_owned()).collect();
        cache.retain(&seen);
    }
    stats
}

/// The per-attribution totals folded into a lookup keyed by the join key, so
/// `scan_all` can attach each discovered skill's usage. Attribution strings
/// with no matching discovered skill simply never get looked up (dropped, not
/// fabricated). Takes the totals rather than the cache so the all-time and
/// windowed index builders share one folding path (issue #14).
fn usage_by_key(totals: Vec<UsageTotal>) -> HashMap<UsageKey, UsageReport> {
    let mut map: HashMap<UsageKey, UsageReport> = HashMap::new();
    for total in totals {
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
    /// All-time per-skill usage (issue #5): the shipped cumulative figures.
    pub fn build(cache: &SqliteUsageCache) -> Self {
        UsageIndex { by_key: usage_by_key(cache.totals()) }
    }

    /// Per-skill usage restricted to records at or after `cutoff_millis` (issue
    /// #14): the rolling-window counterpart, same folding, only the totals
    /// query is bounded. A record with a 0 timestamp (unparseable) never lands
    /// in a positive-cutoff window.
    pub fn build_windowed(cache: &SqliteUsageCache, cutoff_millis: i64) -> Self {
        UsageIndex { by_key: usage_by_key(cache.totals_since(cutoff_millis)) }
    }

    /// This skill's attributed usage, or `None` if no session touched it.
    pub fn for_skill(&self, skill: &DiscoveredSkill) -> Option<UsageReport> {
        self.by_key.get(&UsageKey::from_skill(skill)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::super::footprint_text::transcript_refs_by_recency;
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

    /// An assistant line carrying a top-level RFC3339 `timestamp`, built on the
    /// base fixture so only the timestamp field is added (issue #14).
    fn assistant_line_at(message_id: &str, skill: &str, timestamp: &str, work: u32) -> String {
        let mut rec: serde_json::Value =
            serde_json::from_str(&assistant_line(message_id, "u", Some(skill), None, work, 0, 0, 0)).unwrap();
        rec["timestamp"] = json!(timestamp);
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
        let rows = parse_usage_rows(&line);
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
        assert!(parse_usage_rows(&null_attr).is_empty());
        // A line with usage but type != assistant is ignored too.
        let user_line = json!({"type":"user","message":{"id":"m","usage":{"input_tokens":5}}}).to_string();
        assert!(parse_usage_rows(&user_line).is_empty());
    }

    #[test]
    fn dedup_is_by_message_id_not_record_uuid() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        // Same message.id, DIFFERENT record uuid (a resume copy): count once.
        cache.ingest(&parse_usage_rows(&assistant_line("msg_A", "uuid-1", Some("grilling"), None, 10, 20, 0, 0)));
        cache.ingest(&parse_usage_rows(&assistant_line("msg_A", "uuid-2", Some("grilling"), None, 10, 20, 0, 0)));
        let totals = cache.totals();
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
        cache.ingest(&parse_usage_rows(&content));
        assert_eq!(cache.totals()[0].work, 30, "one message must count once, not 90");
    }

    #[test]
    fn native_join_credits_personal_and_plugin_skills() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&parse_usage_rows(&assistant_line("m1", "u1", Some("grilling"), None, 100, 0, 0, 0)));
        cache.ingest(&parse_usage_rows(&assistant_line(
            "m2", "u2", Some("superpowers:executing-plans"), Some("superpowers"), 50, 0, 0, 0,
        )));
        let index = UsageIndex::build(&cache);

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
        cache.ingest(&parse_usage_rows(&assistant_line("m1", "u1", Some("impeccable:frontend-design"), Some("impeccable"), 43, 0, 0, 0)));
        cache.ingest(&parse_usage_rows(&assistant_line("m2", "u2", Some("frontend-design:frontend-design"), Some("frontend-design"), 187, 0, 0, 0)));
        let index = UsageIndex::build(&cache);

        let a = skill(SkillId::Plugin { marketplace: "mp".to_string(), plugin: "impeccable".to_string(), name: "frontend-design".to_string() });
        let b = skill(SkillId::Plugin { marketplace: "mp".to_string(), plugin: "frontend-design".to_string(), name: "frontend-design".to_string() });
        assert_eq!(index.for_skill(&a).unwrap().work, 43, "must not merge under a name-only join");
        assert_eq!(index.for_skill(&b).unwrap().work, 187);
    }

    #[test]
    fn attribution_with_no_matching_skill_is_dropped_not_fabricated() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&parse_usage_rows(&assistant_line("m1", "u1", Some("loop"), None, 100, 0, 0, 0)));
        let index = UsageIndex::build(&cache);
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

        let cold = refresh_usage(&refs(&dir), &cache);
        assert_eq!(cold.files_read, 1);
        let cold_total = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;

        let warm = refresh_usage(&refs(&dir), &cache);
        assert_eq!(warm.files_read, 0, "an unchanged transcript is not re-read");
        let warm_total = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
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
        refresh_usage(&refs(&dir), &cache);

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", assistant_line("m2", "u2", Some("grilling"), None, 7, 0, 0, 0)).unwrap();
        drop(f);

        let stats = refresh_usage(&refs(&dir), &cache);
        assert_eq!(stats.files_read, 1, "the grown file is re-read");
        let total = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
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
        refresh_usage(&refs(&dir), &cache);
        let after_partial = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(after_partial, 10, "the partial line is skipped, not counted");

        // Complete the file; the once-partial record now counts exactly once.
        let m2 = assistant_line("m2", "u2", Some("grilling"), None, 5, 0, 0, 0);
        fs::write(&path, format!("{good}\n{m2}\n")).unwrap();
        refresh_usage(&refs(&dir), &cache);
        let after_complete = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
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
        refresh_usage(&refs(&dir), &cache);

        let total = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
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
        let stats = refresh_usage(&refs(&dir), &cache);

        assert_eq!(stats.files_read, 1, "only the depth-1 main.jsonl is enumerated, never the subagents/ file");
        let total = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(total, 10, "the sub-agent file's 999 tokens are excluded by default");
    }

    #[test]
    fn parse_iso8601_millis_golden_values() {
        assert_eq!(parse_iso8601_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_millis("1970-01-01T00:00:01.500Z"), Some(1500));
        assert_eq!(parse_iso8601_millis("2000-01-01T00:00:00Z"), Some(946_684_800_000));
        assert_eq!(parse_iso8601_millis("2020-01-01T00:00:00Z"), Some(1_577_836_800_000));
        assert_eq!(parse_iso8601_millis("2026-06-27T02:13:52.480Z"), Some(1_782_526_432_480));
        assert_eq!(parse_iso8601_millis("not-a-timestamp"), None, "garbage -> None, never a bogus millis");
        assert_eq!(parse_iso8601_millis(""), None);
    }

    #[test]
    fn parses_timestamp_from_an_assistant_record() {
        let rows = parse_usage_rows(&assistant_line_at("m1", "grilling", "2026-06-27T02:13:52.480Z", 10));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp_millis, 1_782_526_432_480);
    }

    #[test]
    fn a_record_without_a_timestamp_defaults_to_zero_not_dropped() {
        // The base fixture carries no top-level timestamp.
        let rows = parse_usage_rows(&assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0));
        assert_eq!(rows.len(), 1, "a timestamp-less record is still counted, never dropped");
        assert_eq!(rows[0].timestamp_millis, 0, "it degrades to 0 (oldest)");
    }

    #[test]
    fn a_malformed_timestamp_defaults_to_zero() {
        let rows = parse_usage_rows(&assistant_line_at("m1", "grilling", "yesterday-ish", 10));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp_millis, 0, "an unparseable timestamp degrades to 0, row kept");
    }

    #[test]
    fn windowed_index_credits_only_in_window_usage() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&parse_usage_rows(&assistant_line_at("old", "grilling", "2020-01-01T00:00:00Z", 100)));
        cache.ingest(&parse_usage_rows(&assistant_line_at("new", "grilling", "2026-06-27T02:13:52.480Z", 40)));
        let g = skill(SkillId::Personal { name: "grilling".to_string() });

        // All-time sees both; a cutoff between the two records sees only the recent one.
        assert_eq!(UsageIndex::build(&cache).for_skill(&g).unwrap().work, 140);
        let cutoff = 1_600_000_000_000; // 2020-09, after the old record, before the new one
        assert_eq!(UsageIndex::build_windowed(&cache, cutoff).for_skill(&g).unwrap().work, 40);
    }
}
