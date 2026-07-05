use super::footprint_text::{mtime_nanos, TranscriptRef};
use super::usage_cache::{ReadPlan, SqliteUsageCache, UsageRow};
use crate::domain::report::{AttributionSource, UsageReport};
use crate::domain::skill::{DiscoveredSkill, SkillId};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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
/// re-reads no transcripts and that a growth tailed rather than fully re-read.
#[derive(Default, Debug, Clone, Copy)]
pub struct UsageStats {
    pub files_total: usize,
    pub files_read: usize,
    /// Bytes actually read from disk this pass (a tail reads far fewer than a
    /// full re-read of the same grown file).
    pub bytes_read: u64,
    pub tail_reads: usize,
    /// Whole-file reads, INCLUDING a tail that fell back to Full because the
    /// prefix had been rewritten.
    pub full_reads: usize,
}

/// Incremental usage ingest over `transcripts` (issue #5 + #15). Takes the
/// already-enumerated main-thread refs `scan_all` built for the listing index
/// (so a scan enumerates the transcript dirs once, not twice) plus the set of
/// dirs whose enumeration actually SUCCEEDED (`enumerated_dirs`), which the
/// prune step needs to tell a real deletion from a transient read failure.
///
/// Two hygiene jobs wrap the per-file loop (issue #15, ADR 0024):
/// - **Prune by rebuild.** If a checkpointed transcript has genuinely vanished,
///   `wipe` both tables and let the loop re-ingest the present set. Rows carry
///   no per-path provenance and a `message.id` lives in many transcripts, so a
///   targeted delete is unsafe; a rebuild via `message.id` dedup is the only
///   correct prune (a still-present id survives, an only-in-vanished id drops).
/// - **Tail-read.** A file is opened only when its `(mtime, size)` changed; a
///   grown file with an intact prefix reads only its appended bytes.
///
/// Rows are written INSERT OR IGNORE, so any re-read is idempotent and both a
/// mis-tailed rewrite and a cross-file duplicate `message.id` collapse to one
/// count. Sub-agent files live under `subagents/` subdirs the enumeration never
/// descends into, so they stay excluded by default for free.
pub fn refresh_usage(
    transcripts: &[TranscriptRef],
    enumerated_dirs: &HashSet<PathBuf>,
    cache: &SqliteUsageCache,
) -> UsageStats {
    let mut stats = UsageStats::default();

    // Conditional full rebuild on a genuine vanish. The dir-scoped check inside
    // `has_vanished_checkpoint` already ignores dirs that failed to enumerate,
    // so an empty enumeration (total read failure) reports nothing vanished and
    // never wipes -- no separate "is_empty" guard is needed or correct.
    let seen: HashSet<String> =
        transcripts.iter().map(|t| t.path.to_string_lossy().into_owned()).collect();
    let enumerated: HashSet<String> =
        enumerated_dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect();
    if cache.has_vanished_checkpoint(&seen, &enumerated) {
        cache.wipe();
    }

    for transcript in transcripts {
        stats.files_total += 1;
        let path_key = transcript.path.to_string_lossy();
        let size = transcript.size as i64;
        let mnanos = mtime_nanos(transcript.mtime);

        // A file with no reliable mtime key can't be checkpointed, so it is
        // fully re-read every scan and never marked (unchanged from #5).
        let plan = match mnanos {
            Some(m) => cache.read_plan(&path_key, m, size),
            None => ReadPlan::Full,
        };

        // Resolve the plan to the bytes to parse: `base` is their byte offset
        // in the file, `slice_start` is where they begin within `bytes` (1 for a
        // tail, to drop the boundary newline). `is_tail` is only for accounting.
        let (base, bytes, slice_start, is_tail) = match plan {
            ReadPlan::Skip => continue,
            ReadPlan::Full => match fs::read(&transcript.path) {
                Ok(bytes) => (0u64, bytes, 0usize, false),
                Err(_) => continue,
            },
            ReadPlan::Tail(off) => match read_tail_bytes(&transcript.path, off) {
                // The byte at `off - 1` is the newline we last parsed past, so
                // the prefix is intact and only `[off..EOF]` is new.
                Some(buf) if buf.first() == Some(&b'\n') => (off, buf, 1usize, true),
                // Boundary byte isn't a newline: the prefix was rewritten, so
                // the append assumption is void -- re-read the whole file. This
                // is safe (never overcounts) because ingest dedups on the stable
                // message.id, but it counts as a full read.
                Some(_) => match fs::read(&transcript.path) {
                    Ok(bytes) => (0u64, bytes, 0usize, false),
                    Err(_) => continue,
                },
                None => continue,
            },
        };

        stats.files_read += 1;
        stats.bytes_read += bytes.len() as u64;
        if is_tail {
            stats.tail_reads += 1;
        } else {
            stats.full_reads += 1;
        }

        // Consume only up to and including the last newline; a partial trailing
        // line stays unparsed and is re-read (as a tail) once completed.
        let appended = &bytes[slice_start..];
        let consumed = appended.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
        // The consumed slice ends exactly on a '\n' (a char boundary), so it is
        // valid UTF-8 whenever the file is; a non-UTF-8 file is skipped without
        // advancing the gate, exactly as the old read_to_string path did.
        let Ok(text) = std::str::from_utf8(&appended[..consumed]) else { continue };
        cache.ingest(&parse_usage_rows(text));
        let new_off = base + consumed as u64;
        if let Some(m) = mnanos {
            cache.mark(&path_key, m, size, new_off as i64);
        }
    }

    stats
}

/// Reads `[off - 1 .. EOF]` of `path`: the byte before the append point plus
/// everything after it. The caller inspects `buf[0]` (the presumed boundary
/// newline) to confirm the prefix is intact before trusting the tail. `None`
/// on any I/O error, so the caller falls through to a re-read. `off` is always
/// `> 0` (only `read_plan`'s `Tail(off)` reaches here, and it never yields 0).
fn read_tail_bytes(path: &Path, off: u64) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(off - 1)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// The per-attribution totals folded into a lookup keyed by the join key, so
/// `scan_all` can attach each discovered skill's usage. Attribution strings
/// with no matching discovered skill simply never get looked up (dropped, not
/// fabricated).
fn usage_by_key(cache: &SqliteUsageCache) -> HashMap<UsageKey, UsageReport> {
    let mut map: HashMap<UsageKey, UsageReport> = HashMap::new();
    for total in cache.totals() {
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
    pub fn build(cache: &SqliteUsageCache) -> Self {
        UsageIndex { by_key: usage_by_key(cache) }
    }

    /// This skill's attributed usage, or `None` if no session touched it.
    pub fn for_skill(&self, skill: &DiscoveredSkill) -> Option<UsageReport> {
        self.by_key.get(&UsageKey::from_skill(skill)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::super::footprint_text::transcript_refs_by_recency;
    use super::super::usage_cache::UsageTotal;
    use super::*;
    use crate::domain::skill::{Frontmatter, SkillId};
    use serde_json::json;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};

    /// Enumerate a single dir fresh (re-stat'd so an append/rewrite is seen) and
    /// run one usage refresh over it -- the common case for these tests.
    fn scan(dir: &Path, cache: &SqliteUsageCache) -> UsageStats {
        let (refs, dirs) = transcript_refs_by_recency(&[dir.to_path_buf()]);
        refresh_usage(&refs, &dirs, cache)
    }

    /// Force a file's mtime to a fixed instant, so a test can drive `read_plan`
    /// past the `(mtime, size)` gate deterministically (an in-place same-size
    /// rewrite needs a distinct mtime to be seen at all).
    fn set_mtime(path: &Path, secs: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
    }

    fn append_line(path: &Path, line: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
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

        let cold = scan(&dir, &cache);
        assert_eq!(cold.files_read, 1);
        let cold_total = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;

        let warm = scan(&dir, &cache);
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
        scan(&dir, &cache);

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", assistant_line("m2", "u2", Some("grilling"), None, 7, 0, 0, 0)).unwrap();
        drop(f);

        let stats = scan(&dir, &cache);
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
        scan(&dir, &cache);
        let after_partial = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(after_partial, 10, "the partial line is skipped, not counted");

        // Complete the file; the once-partial record now counts exactly once.
        let m2 = assistant_line("m2", "u2", Some("grilling"), None, 5, 0, 0, 0);
        fs::write(&path, format!("{good}\n{m2}\n")).unwrap();
        scan(&dir, &cache);
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
        scan(&dir, &cache);

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
        let stats = scan(&dir, &cache);

        assert_eq!(stats.files_read, 1, "only the depth-1 main.jsonl is enumerated, never the subagents/ file");
        let total = UsageIndex::build(&cache).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(total, 10, "the sub-agent file's 999 tokens are excluded by default");
    }

    // ---- issue #15: byte-offset tail-reader ----

    /// This machine's grilling work total, the metric most #15 tests assert on.
    fn grilling_work(cache: &SqliteUsageCache) -> u64 {
        UsageIndex::build(cache)
            .for_skill(&skill(SkillId::Personal { name: "grilling".to_string() }))
            .map(|u| u.work)
            .unwrap_or(0)
    }

    #[test]
    fn an_append_tail_reads_only_the_appended_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let path = dir.join("s.jsonl");
        // A is deliberately several records, so a tail of the small append reads
        // far fewer bytes than a full re-read of the grown file would (AC2).
        let a_lines: Vec<String> = (0..5)
            .map(|i| assistant_line(&format!("a{i}"), &format!("u{i}"), Some("grilling"), None, 10, 0, 0, 0))
            .collect();
        write_transcript(&dir, "s.jsonl", &a_lines);
        set_mtime(&path, 1000);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        let cold = scan(&dir, &cache);
        assert_eq!(cold.full_reads, 1, "the cold pass is a full read");
        let a_size = fs::metadata(&path).unwrap().len();

        append_line(&path, &assistant_line("b1", "ub", Some("grilling"), None, 7, 0, 0, 0));
        set_mtime(&path, 2000);

        let stats = scan(&dir, &cache);
        assert_eq!(stats.tail_reads, 1, "a grown file with an intact prefix is tail-read");
        assert_eq!(stats.full_reads, 0);
        assert!(
            stats.bytes_read < a_size,
            "a tail reads fewer bytes than a full re-read of A ({} vs {a_size})",
            stats.bytes_read,
        );
        assert_eq!(grilling_work(&cache), 5 * 10 + 7, "all five A records once + the appended B");
    }

    #[test]
    fn a_compaction_shrink_triggers_a_full_re_read() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let path = dir.join("s.jsonl");
        write_transcript(&dir, "s.jsonl", &[
            assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0),
            assistant_line("m2", "u2", Some("grilling"), None, 20, 0, 0, 0),
            assistant_line("m3", "u3", Some("grilling"), None, 30, 0, 0, 0),
        ]);
        set_mtime(&path, 1000);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        scan(&dir, &cache);

        // A shrink (fewer bytes than before) is never an append.
        fs::write(&path, format!("{}\n", assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0))).unwrap();
        set_mtime(&path, 2000);
        let new_size = fs::metadata(&path).unwrap().len();

        let stats = scan(&dir, &cache);
        assert_eq!(stats.full_reads, 1, "a shrink forces a full re-read, never a tail");
        assert_eq!(stats.tail_reads, 0);
        assert_eq!(stats.bytes_read, new_size, "a full read reads the whole (shrunk) file");
    }

    #[test]
    fn a_same_size_rewrite_with_a_newer_mtime_triggers_a_full_re_read() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let path = dir.join("s.jsonl");
        let a = assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0);
        write_transcript(&dir, "s.jsonl", std::slice::from_ref(&a));
        set_mtime(&path, 1000);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        scan(&dir, &cache);
        let size = fs::metadata(&path).unwrap().len();

        // Rewrite in place: SAME byte size, different content, newer mtime.
        let b = assistant_line("m2", "u2", Some("grilling"), None, 20, 0, 0, 0);
        assert_eq!(a.len(), b.len(), "test setup: both lines must be the same byte length");
        fs::write(&path, format!("{b}\n")).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), size, "test setup: same byte size after rewrite");
        set_mtime(&path, 2000);

        let stats = scan(&dir, &cache);
        assert_eq!(stats.full_reads, 1, "a same-size in-place rewrite (mtime changed) is a full re-read");
        assert_eq!(stats.tail_reads, 0);
    }

    #[test]
    fn a_truncated_append_is_not_counted_until_completed_via_tail() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let path = dir.join("s.jsonl");
        write_transcript(&dir, "s.jsonl", &[assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0)]);
        set_mtime(&path, 1000);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        scan(&dir, &cache);

        // Append a complete record WITHOUT its line terminator: a mid-write
        // partial line from the tail-reader's point of view.
        let m2 = assistant_line("m2", "u2", Some("grilling"), None, 5, 0, 0, 0);
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            write!(f, "{m2}").unwrap();
        }
        set_mtime(&path, 2000);
        let partial = scan(&dir, &cache);
        assert_eq!(partial.tail_reads, 1, "the grown file is tail-read");
        assert_eq!(grilling_work(&cache), 10, "an unterminated trailing line is not counted yet");

        // Terminate the line: the once-partial record now counts exactly once,
        // re-read as a tail from the SAME offset (it never advanced past it).
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f).unwrap();
        }
        set_mtime(&path, 3000);
        let done = scan(&dir, &cache);
        assert_eq!(done.tail_reads, 1);
        assert_eq!(grilling_work(&cache), 15, "the completed record counts once");
    }

    #[test]
    fn a_grown_file_with_a_rewritten_prefix_falls_back_to_full() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let path = dir.join("s.jsonl");
        let a = assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0);
        write_transcript(&dir, "s.jsonl", std::slice::from_ref(&a));
        set_mtime(&path, 1000);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        scan(&dir, &cache);

        // Grow the FIRST line (trailing spaces keep m1's identity -- JSON ignores
        // them) so the stored offset now points inside the rewritten line: the
        // boundary byte is a space, not the old newline, forcing a Full fallback.
        let a_padded = format!("{a}          ");
        let b = assistant_line("m2", "u2", Some("grilling"), None, 7, 0, 0, 0);
        fs::write(&path, format!("{a_padded}\n{b}\n")).unwrap();
        set_mtime(&path, 2000);

        let stats = scan(&dir, &cache);
        assert_eq!(stats.full_reads, 1, "a rewritten prefix falls back to a full read");
        assert_eq!(stats.tail_reads, 0);
        assert_eq!(grilling_work(&cache), 17, "m1 (same id, deduped) + the new m2");
    }

    #[test]
    fn tail_and_full_incremental_yield_identical_totals() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let path = dir.join("s.jsonl");

        // Incremental store: a cold read, then a sequence of appends -- each a
        // tail -- across two skills and a plugin attribution.
        let incr = SqliteUsageCache::open_in_memory().unwrap();
        write_transcript(&dir, "s.jsonl", &[assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0)]);
        set_mtime(&path, 1000);
        assert_eq!(scan(&dir, &incr).full_reads, 1);

        append_line(&path, &assistant_line("m2", "u2", Some("grilling"), None, 20, 0, 0, 0));
        set_mtime(&path, 2000);
        assert_eq!(scan(&dir, &incr).tail_reads, 1);

        append_line(&path, &assistant_line("m3", "u3", Some("superpowers:executing-plans"), Some("superpowers"), 30, 0, 0, 0));
        set_mtime(&path, 3000);
        assert_eq!(scan(&dir, &incr).tail_reads, 1);

        append_line(&path, &assistant_line("m4", "u4", Some("grilling"), None, 5, 0, 0, 0));
        set_mtime(&path, 4000);
        assert_eq!(scan(&dir, &incr).tail_reads, 1);

        // Control: a fresh cache does ONE cold full read of the FINAL file.
        let control = SqliteUsageCache::open_in_memory().unwrap();
        let ctrl_stats = scan(&dir, &control);
        assert_eq!(ctrl_stats.full_reads, 1, "the control is a single cold pass");
        assert_eq!(ctrl_stats.tail_reads, 0);

        let key = |t: &UsageTotal| (t.attribution_skill.clone(), t.attribution_plugin.clone());
        let mut incr_totals = incr.totals();
        let mut ctrl_totals = control.totals();
        incr_totals.sort_by_key(key);
        ctrl_totals.sort_by_key(key);
        assert_eq!(incr_totals, ctrl_totals, "incremental tail reads must equal a cold full read");
    }

    #[test]
    fn a_vanished_transcript_is_rebuilt_without_losing_shared_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        // a: msgA; b: a resume holding msgA (again) + msgB; c: msgC.
        write_transcript(&dir, "a.jsonl", &[assistant_line("msgA", "ua", Some("grilling"), None, 10, 0, 0, 0)]);
        write_transcript(&dir, "b.jsonl", &[
            assistant_line("msgA", "ua-copy", Some("grilling"), None, 10, 0, 0, 0),
            assistant_line("msgB", "ub", Some("grilling"), None, 7, 0, 0, 0),
        ]);
        write_transcript(&dir, "c.jsonl", &[assistant_line("msgC", "uc", Some("grilling"), None, 5, 0, 0, 0)]);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        scan(&dir, &cache);
        assert_eq!(grilling_work(&cache), 22, "msgA(10, once across a+b) + msgB(7) + msgC(5)");

        // a and c vanish; b (which also holds msgA) survives.
        fs::remove_file(dir.join("a.jsonl")).unwrap();
        fs::remove_file(dir.join("c.jsonl")).unwrap();

        let stats = scan(&dir, &cache);
        assert!(stats.files_read >= 1, "the rebuild re-reads the surviving transcript");
        assert_eq!(
            grilling_work(&cache),
            17,
            "rebuild drops msgC (only in the vanished c) but keeps msgA (still in b) + msgB",
        );
    }

    #[test]
    fn a_rebuild_is_idempotent_on_the_next_warm_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        write_transcript(&dir, "a.jsonl", &[assistant_line("msgA", "ua", Some("grilling"), None, 10, 0, 0, 0)]);
        write_transcript(&dir, "b.jsonl", &[assistant_line("msgB", "ub", Some("grilling"), None, 7, 0, 0, 0)]);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        scan(&dir, &cache);

        fs::remove_file(dir.join("a.jsonl")).unwrap();
        let rebuilt = scan(&dir, &cache); // detects the vanish, wipes, re-ingests b
        assert!(rebuilt.files_read >= 1);
        assert_eq!(grilling_work(&cache), 7, "only msgB survives the rebuild");

        // Nothing changed since: no vanish, no re-read, identical total.
        let warm = scan(&dir, &cache);
        assert_eq!(warm.files_read, 0, "a warm rescan after a rebuild re-reads nothing");
        assert_eq!(grilling_work(&cache), 7);
    }

    #[test]
    fn an_empty_enumeration_does_not_wipe_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        write_transcript(&dir, "s.jsonl", &[assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0)]);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        scan(&dir, &cache);
        assert_eq!(grilling_work(&cache), 10);

        // A total enumeration failure (no dirs read) must NOT be read as "every
        // transcript vanished": the cumulative store is preserved.
        let no_dirs: HashSet<PathBuf> = HashSet::new();
        let stats = refresh_usage(&[], &no_dirs, &cache);
        assert_eq!(stats.files_read, 0);
        assert_eq!(grilling_work(&cache), 10, "an empty enumeration preserves the store");
    }

    #[test]
    fn a_transiently_unreadable_project_dir_does_not_wipe_usage_for_its_present_transcripts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("repo-a");
        let dir_b = tmp.path().join("repo-b");
        write_transcript(&dir_a, "a.jsonl", &[assistant_line("mA", "uA", Some("grilling"), None, 10, 0, 0, 0)]);
        write_transcript(&dir_b, "b.jsonl", &[assistant_line("mB", "uB", Some("grilling"), None, 20, 0, 0, 0)]);
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        let (refs, dirs) = transcript_refs_by_recency(&[dir_a.clone(), dir_b.clone()]);
        refresh_usage(&refs, &dirs, &cache);
        assert_eq!(grilling_work(&cache), 30);

        // repo-b becomes unreadable (here: removed, so read_dir fails -- the
        // portable stand-in for any transient enumeration failure). Its
        // checkpoint is "unknown", never a vanish, so nothing is wiped and mB's
        // usage survives even though b.jsonl is absent from this scan.
        fs::remove_dir_all(&dir_b).unwrap();
        let (refs2, dirs2) = transcript_refs_by_recency(&[dir_a.clone(), dir_b.clone()]);
        assert!(!dirs2.contains(&dir_b), "an unreadable dir is not in the enumerated set");
        refresh_usage(&refs2, &dirs2, &cache);

        assert_eq!(
            grilling_work(&cache),
            30,
            "a transiently unreadable dir must not wipe its transcripts' usage (data-loss guard)",
        );
    }
}
