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
            reconstructed: false,
        });
    }
    rows
}

/// Reconstruct attributed usage only for a transcript record whose Claude Code
/// build is STRICTLY below this version (issue #12). Conservative and
/// corpus-inferred: the changelog has no explicit entry for when
/// `attributionSkill` first shipped on main-thread `assistant` records, and its
/// earliest published coverage is 2.1.145, so builds at or above 2.1.146 are
/// assumed to compute native attribution. At or above the gate, an absent
/// `attributionSkill` means "no skill was active," and reconstructing there
/// would fabricate attribution the build deliberately withheld (ADR 0005). Err
/// LOWER, not higher: a lower gate reconstructs fewer builds and so can only
/// ever under-credit, never fabricate.
const ATTRIBUTION_GATE: (u32, u32, u32) = (2, 1, 146);

/// Parses a `major.minor.patch` version into a numeric tuple, or `None` for
/// anything that is not exactly three dot-separated integers (missing,
/// malformed, suffixed like `2.1.146-rc1`, or a 4+-component build). Numeric,
/// never lexical: string comparison mis-orders "2.1.9" as greater than
/// "2.1.146", so the components must be compared as numbers.
fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    // A fourth component means this isn't a plain major.minor.patch; treat it as
    // malformed rather than silently ignoring the tail.
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Whether a `version` string is strictly below the attribution gate. A missing
/// or unparseable version is treated as NOT below the gate, so an ambiguous
/// build never triggers reconstruction (the safe direction: a miss, never a
/// fabrication).
fn is_below_gate(version: Option<&str>) -> bool {
    match version.and_then(parse_version) {
        Some(v) => v < ATTRIBUTION_GATE,
        None => false,
    }
}

/// A minimal probe for the file-level version gate: just the top-level
/// `version`, so finding the build costs a small parse per candidate line
/// rather than a full `ReconRecord` deserialize.
#[derive(Deserialize)]
struct VersionProbe {
    version: Option<String>,
}

/// Whether the transcript's build is below the attribution gate, read from its
/// FIRST non-null `version` (MUST-FIX 4). Scans past a leading versionless
/// `mode`/`last-prompt` record and stops at the first version found -- so a
/// current-build file (the common case) returns `false` after only a line or
/// two, never a whole-file parse. No version anywhere is NOT below gate.
fn file_is_below_gate(content: &str) -> bool {
    for line in content.lines() {
        // Skip lines that can't carry a top-level version without paying a parse
        // (the `version` key is present on every real record that has one).
        if !line.contains("\"version\"") {
            continue;
        }
        let Ok(probe) = serde_json::from_str::<VersionProbe>(line) else { continue };
        if let Some(version) = probe.version {
            return is_below_gate(Some(&version));
        }
    }
    false
}

/// A transcript record, parsed for the RECONSTRUCTION walk (issue #12). Unlike
/// the native `UsageRecord`, this also carries the `isMeta` flag and the
/// `message.content` blocks needed to spot a `Skill` invoke and a fresh human
/// turn. The build `version` is not here: the gate is decided once per file via
/// the lighter `VersionProbe` before the walk begins. Kept a separate struct so
/// the hot native path (`parse_usage_rows`) stays lean and does not deserialize
/// content on every line (SHOULD-FIX 6).
#[derive(Deserialize)]
struct ReconRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "attributionSkill")]
    attribution_skill: Option<String>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    #[serde(rename = "isMeta", default)]
    is_meta: bool,
    message: Option<ReconMessage>,
}

#[derive(Deserialize)]
struct ReconMessage {
    id: Option<String>,
    usage: Option<UsageTokens>,
    content: Option<ReconContent>,
}

/// A record's `message.content`: a bare string for a typed human prompt, or an
/// array of blocks for an assistant turn (or a tool_result-bearing user turn).
#[derive(Deserialize)]
#[serde(untagged)]
enum ReconContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    input: Option<BlockInput>,
}

#[derive(Deserialize)]
struct BlockInput {
    /// The invoked skill's attribution string on a `Skill` tool_use block --
    /// byte-identical to the native `attributionSkill` (verified), so the same
    /// `UsageKey::from_attribution` join applies.
    skill: Option<String>,
}

/// Is this `user` record a fresh human turn (which clears the active skill)?
/// True only for a typed prompt (`content` is a string) or a block turn with no
/// `tool_result`, and never for a meta record (a caveat/system injection) --
/// those are not the user taking the wheel back, so they must not clear the
/// current skill.
fn is_fresh_human_turn(record: &ReconRecord) -> bool {
    if record.is_meta {
        return false;
    }
    match record.message.as_ref().and_then(|m| m.content.as_ref()) {
        // A typed prompt (text the user actually sent) hands the wheel back. An
        // empty string is a degenerate non-turn, so it does not clear -- the
        // conservative direction, and never seen from a real prompt.
        Some(ReconContent::Text(text)) => !text.trim().is_empty(),
        // A block turn is the user only when it carries no `tool_result` (a
        // tool_result is the harness returning a tool's output, not the user).
        Some(ReconContent::Blocks(blocks)) => {
            !blocks.iter().any(|b| b.kind.as_deref() == Some("tool_result"))
        }
        None => false,
    }
}

/// Reconstructs attributed usage for a PRE-ATTRIBUTION transcript (issue #12),
/// the version-gated fallback to native attribution. Walks the file IN APPEND
/// ORDER (not the `parentUuid` tree: append order is causally sound and the
/// pre-attribution target files are monotonic), tracking the single skill
/// currently holding the wheel:
///
/// - a fresh human turn clears it (the user took the wheel back);
/// - each `assistant` record is credited BEFORE any skill it invokes, so an
///   invoking turn's own tokens belong to the skill that was already active,
///   not the one it is about to start -- matching native semantics exactly;
/// - a credit is emitted only when native attribution is ABSENT (else native
///   handles it) and a skill is active.
///
/// The build gate is checked ONCE, at the FILE level, from the first non-null
/// `version` (the leading `mode`/`last-prompt` record often has none) --
/// MUST-FIX 4. This is both correct (files are single-version) and the
/// performance guard: a current-build transcript bails after that cheap early
/// scan and never pays the every-line parse. Only for a genuinely below-gate
/// file is the prefilter dropped and every line walked -- there a missed `Skill`
/// push or human-turn clear would silently miscredit the whole tail, and such
/// files are rare and tiny (SHOULD-FIX 6).
///
/// Every emitted row is flagged `reconstructed: true`.
pub fn reconstruct_usage_rows(content: &str) -> Vec<UsageRow> {
    if !file_is_below_gate(content) {
        return Vec::new();
    }

    let mut rows = Vec::new();
    // The single skill currently holding the wheel (never a stack: we only ever
    // read the top and clear the whole thing, so the stack framing would only
    // invite a future "pop on tool_result" bug -- SHOULD-FIX 7).
    let mut current_skill: Option<String> = None;

    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<ReconRecord>(line) else { continue };
        match record.kind.as_deref() {
            Some("user") => {
                if is_fresh_human_turn(&record) {
                    current_skill = None;
                }
            }
            Some("assistant") => {
                // CREDIT BEFORE PUSH: this record's tokens belong to whatever
                // skill was active when it was produced, not to one it invokes.
                // The whole file is below-gate, so an absent attributionSkill is
                // a reconstruction candidate (never a "no skill active" signal a
                // current build would have emitted).
                if record.attribution_skill.is_none() {
                    if let (Some(skill), Some(message)) = (&current_skill, &record.message) {
                        if let (Some(id), Some(usage)) = (&message.id, &message.usage) {
                            rows.push(UsageRow {
                                message_id: id.clone(),
                                attribution_skill: skill.clone(),
                                attribution_plugin: skill.split_once(':').map(|(p, _)| p.to_string()),
                                is_subagent: record.is_sidechain,
                                work: usage.input_tokens.saturating_add(usage.output_tokens),
                                cache_write: usage.cache_creation_input_tokens,
                                cache_read: usage.cache_read_input_tokens,
                                reconstructed: true,
                            });
                        }
                    }
                }
                // THEN push: a `Skill` invoke in this record switches the active
                // skill for the turns that follow it.
                if let Some(ReconContent::Blocks(blocks)) = record.message.as_ref().and_then(|m| m.content.as_ref()) {
                    for block in blocks {
                        if block.kind.as_deref() == Some("tool_use") && block.name.as_deref() == Some("Skill") {
                            if let Some(skill) = block.input.as_ref().and_then(|i| i.skill.as_ref()) {
                                current_skill = Some(skill.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
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
        // Reconstruct attributed usage for pre-attribution builds (issue #12).
        // Native rows land first, so a message that already has native
        // attribution is never displaced by a reconstructed guess; within one
        // file the two passes never emit the same `message.id` (a record either
        // carries `attributionSkill` or is a reconstruction candidate, never
        // both), so this only adds credits native left on the table.
        cache.ingest(&reconstruct_usage_rows(&content));
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
        // Reconstructed is sticky across a fold: a key stays Native only while
        // every contributing total is native, and any reconstructed one
        // downgrades it for good (it is never upgraded back). This holds even
        // when two distinct attribution strings collapse to one `UsageKey`
        // (ADR 0003 honesty).
        if total.reconstructed {
            entry.attribution_source = AttributionSource::Reconstructed;
        }
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

    // ---- issue #12: version gate ----

    #[test]
    fn below_gate_is_true_only_strictly_under_2_1_146() {
        assert!(is_below_gate(Some("2.1.145")), "2.1.145 is below the gate");
        assert!(!is_below_gate(Some("2.1.146")), "the gate itself is NOT below (exclusive)");
        assert!(!is_below_gate(Some("2.1.200")), "a later build is not below");
    }

    #[test]
    fn version_compare_is_numeric_not_lexical() {
        // The whole reason `parse_version` returns a numeric tuple: a string
        // compare would order "2.1.9" AFTER "2.1.146" and wrongly gate it out.
        assert!(is_below_gate(Some("2.1.9")), "9 < 146 numerically, so 2.1.9 is below the gate");
    }

    #[test]
    fn missing_or_malformed_version_is_not_below_gate() {
        assert!(!is_below_gate(None), "a missing version never reconstructs");
        assert!(!is_below_gate(Some("garbage")));
        assert!(!is_below_gate(Some("2.1")), "a two-component version is malformed");
        assert!(!is_below_gate(Some("2.1.x")));
        assert!(!is_below_gate(Some("2.1.146-rc1")), "a suffixed build is malformed, not below");
        assert!(!is_below_gate(Some("2.1.100.1")), "a four-component build is malformed");
    }

    /// A below-gate `assistant` record with NO `attributionSkill`, a
    /// `message.id` + `usage`, and optionally a `Skill` tool_use block.
    fn recon_assistant(message_id: &str, version: &str, invokes: Option<&str>, input: u32, output: u32) -> String {
        let mut content = vec![json!({"type": "text", "text": "working"})];
        if let Some(sk) = invokes {
            content.push(json!({"type": "tool_use", "name": "Skill", "input": {"skill": sk}}));
        }
        json!({
            "type": "assistant",
            "version": version,
            "uuid": message_id,
            "message": {
                "id": message_id,
                "role": "assistant",
                "content": content,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        })
        .to_string()
    }

    /// A fresh human turn: a typed prompt (string content), which clears the
    /// active skill.
    fn human_turn(version: &str, text: &str) -> String {
        json!({"type": "user", "version": version, "message": {"role": "user", "content": text}}).to_string()
    }

    /// A user record carrying a `tool_result` block -- the harness returning a
    /// tool's output, NOT the user taking the wheel back, so it must not clear.
    fn tool_result_turn(version: &str) -> String {
        json!({
            "type": "user",
            "version": version,
            "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}
        })
        .to_string()
    }

    /// The versionless leading record a real transcript opens with (a `mode` or
    /// `last-prompt` line): the file gate must scan PAST it to the first real
    /// version, not conclude "no version" from the first line.
    fn mode_record() -> String {
        json!({"type": "mode", "mode": "default"}).to_string()
    }

    fn recon_total(rows: &[UsageRow], skill: &str) -> u32 {
        rows.iter().filter(|r| r.attribution_skill == skill).map(|r| r.work).sum()
    }

    // ---- issue #12: the reconstruction walk (AC1) ----

    #[test]
    fn pre_attribution_skill_invoke_credits_following_turns_reconstructed() {
        let content = [
            human_turn("2.1.100", "please help"),
            recon_assistant("m_invoke", "2.1.100", Some("grilling"), 3, 4), // invokes; own tokens credited to prior (none)
            recon_assistant("m1", "2.1.100", None, 10, 5),
            recon_assistant("m2", "2.1.100", None, 20, 0),
        ]
        .join("\n");
        let rows = reconstruct_usage_rows(&content);

        assert_eq!(recon_total(&rows, "grilling"), 35, "m1 (15) + m2 (20) credited to grilling");
        assert!(rows.iter().all(|r| r.reconstructed), "every reconstructed row is flagged");
        assert!(!rows.iter().any(|r| r.message_id == "m_invoke"), "the invoking turn had no prior skill, so no row");
    }

    #[test]
    fn reconstructed_plugin_skill_derives_plugin_from_prefix() {
        let content = [
            human_turn("2.1.100", "go"),
            recon_assistant("m_invoke", "2.1.100", Some("superpowers:executing-plans"), 0, 0),
            recon_assistant("m1", "2.1.100", None, 40, 0),
        ]
        .join("\n");
        let rows = reconstruct_usage_rows(&content);

        let credited = rows.iter().find(|r| r.message_id == "m1").unwrap();
        assert_eq!(credited.attribution_skill, "superpowers:executing-plans");
        assert_eq!(credited.attribution_plugin.as_deref(), Some("superpowers"), "plugin derived from the prefix");
    }

    #[test]
    fn invoking_turn_credited_to_prior_top_not_new_skill_when_none_active() {
        // No human turn, no prior skill: the first assistant record invokes a
        // skill, but its own tokens have no prior owner, so nothing is credited.
        let content = recon_assistant("m_invoke", "2.1.100", Some("grilling"), 100, 100);
        let rows = reconstruct_usage_rows(&content);
        assert!(rows.is_empty(), "credit-before-push with an empty current emits no row");
    }

    #[test]
    fn a_fresh_human_turn_clears_the_active_skill() {
        let content = [
            human_turn("2.1.100", "start"),
            recon_assistant("m_invoke", "2.1.100", Some("grilling"), 0, 0),
            recon_assistant("m1", "2.1.100", None, 10, 0), // credited to grilling
            human_turn("2.1.100", "new task"),             // clears
            recon_assistant("m2", "2.1.100", None, 50, 0), // no active skill -> not credited
        ]
        .join("\n");
        let rows = reconstruct_usage_rows(&content);

        assert_eq!(recon_total(&rows, "grilling"), 10, "only m1 (before the fresh turn) is credited");
        assert!(!rows.iter().any(|r| r.message_id == "m2"), "post-clear turn is uncredited");
    }

    #[test]
    fn a_tool_result_user_record_does_not_clear() {
        let content = [
            human_turn("2.1.100", "start"),
            recon_assistant("m_invoke", "2.1.100", Some("grilling"), 0, 0),
            tool_result_turn("2.1.100"),                   // must NOT clear grilling
            recon_assistant("m1", "2.1.100", None, 12, 0), // still credited to grilling
        ]
        .join("\n");
        let rows = reconstruct_usage_rows(&content);
        assert_eq!(recon_total(&rows, "grilling"), 12, "a tool_result turn leaves the active skill in place");
    }

    #[test]
    fn a_nested_skill_invoke_switches_credit_to_the_innermost() {
        let content = [
            human_turn("2.1.100", "start"),
            recon_assistant("m_outer", "2.1.100", Some("outer"), 0, 0),
            recon_assistant("m1", "2.1.100", None, 10, 0), // credited to outer
            recon_assistant("m_inner", "2.1.100", Some("inner"), 0, 0), // credit-before-push: to outer, then switch
            recon_assistant("m2", "2.1.100", None, 30, 0), // credited to inner
        ]
        .join("\n");
        let rows = reconstruct_usage_rows(&content);

        assert_eq!(recon_total(&rows, "outer"), 10, "turns before the inner invoke stay with outer");
        assert_eq!(recon_total(&rows, "inner"), 30, "turns after switch to inner");
    }

    // ---- issue #12: version gating (AC2) ----

    #[test]
    fn a_current_build_with_absent_attribution_credits_nothing() {
        // 2.1.168 is at/above the gate: an absent attributionSkill means "no
        // skill active," so even a Skill-invoke-shaped file reconstructs nothing.
        let content = [
            human_turn("2.1.168", "start"),
            recon_assistant("m_invoke", "2.1.168", Some("grilling"), 0, 0),
            recon_assistant("m1", "2.1.168", None, 999, 0),
        ]
        .join("\n");
        let rows = reconstruct_usage_rows(&content);
        assert!(rows.is_empty(), "at/above the gate, absence is never reconstructed");
    }

    #[test]
    fn the_file_gate_is_read_from_the_first_non_null_version() {
        // MUST-FIX 4: the gate is a FILE-level decision from the first non-null
        // `version`, scanning past a versionless leading `mode` record. Here that
        // first real version is below-gate, so the file reconstructs.
        let content = [
            mode_record(), // no version -- must not be read as "no build"
            human_turn("2.1.100", "start"),
            recon_assistant("m_invoke", "2.1.100", Some("grilling"), 0, 0),
            recon_assistant("m1", "2.1.100", None, 15, 0),
        ]
        .join("\n");
        let rows = reconstruct_usage_rows(&content);
        assert_eq!(recon_total(&rows, "grilling"), 15, "a versionless leading record does not hide the below-gate build");
    }

    #[test]
    fn an_at_gate_first_version_blocks_the_whole_file() {
        // The gate is exclusive at the FILE level: a build exactly at 2.1.146
        // reconstructs nothing, even with a Skill-invoke-shaped body.
        let content = [
            human_turn("2.1.146", "start"),
            recon_assistant("m_invoke", "2.1.146", Some("grilling"), 0, 0),
            recon_assistant("m1", "2.1.146", None, 50, 0),
        ]
        .join("\n");
        assert!(reconstruct_usage_rows(&content).is_empty(), "at-gate file is never walked");
    }

    // ---- issue #12: end-to-end through the store and index (AC4) ----

    #[test]
    fn reconstructed_usage_flows_through_index_with_reconstructed_source() {
        let content = [
            human_turn("2.1.100", "start"),
            recon_assistant("m_invoke", "2.1.100", Some("grilling"), 0, 0),
            recon_assistant("m1", "2.1.100", None, 25, 0),
        ]
        .join("\n");
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&reconstruct_usage_rows(&content));

        let report = UsageIndex::build(&cache)
            .for_skill(&skill(SkillId::Personal { name: "grilling".to_string() }))
            .unwrap();
        assert_eq!(report.work, 25);
        assert_eq!(report.attribution_source, AttributionSource::Reconstructed, "the index surfaces the reconstructed source");
    }

    #[test]
    fn a_skill_with_both_native_and_reconstructed_rows_reports_reconstructed() {
        // Native credit for one message, reconstructed for another, same skill:
        // the folded report is downgraded to Reconstructed (sticky, ADR 0003).
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&parse_usage_rows(&assistant_line("m_native", "u1", Some("grilling"), None, 10, 0, 0, 0)));
        let recon = [
            human_turn("2.1.100", "start"),
            recon_assistant("m_invoke", "2.1.100", Some("grilling"), 0, 0),
            recon_assistant("m_recon", "2.1.100", None, 7, 0),
        ]
        .join("\n");
        cache.ingest(&reconstruct_usage_rows(&recon));

        let report = UsageIndex::build(&cache)
            .for_skill(&skill(SkillId::Personal { name: "grilling".to_string() }))
            .unwrap();
        assert_eq!(report.work, 17);
        assert_eq!(report.attribution_source, AttributionSource::Reconstructed);
    }
}
