use super::footprint_text::{mtime_nanos, TranscriptRef};
use super::usage_cache::{ReadPlan, SqliteUsageCache, UsageRow, UsageTotal};
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
            reconstructed: false,
            // Missing/malformed timestamp -> 0 (oldest); never drop the row, so
            // it still counts all-time and just never lands in a recent window.
            timestamp_millis: record.timestamp.as_deref().and_then(parse_iso8601_millis).unwrap_or(0),
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
    /// Read only by the parent-spawn walk (issue #19), which must carry a
    /// natively-attributed spawn's plugin through to the rolled-up credit.
    #[serde(rename = "attributionPlugin")]
    attribution_plugin: Option<String>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    #[serde(rename = "isMeta", default)]
    is_meta: bool,
    /// The record's top-level RFC3339 timestamp, so a reconstructed credit
    /// carries the same window position as a native one (issue #12 + #14).
    #[serde(default)]
    timestamp: Option<String>,
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
    /// A `tool_use` block's own id (`toolu_...`), the join key a sub-agent's
    /// `agent-<id>.meta.json` records as its `toolUseId` (issue #19).
    id: Option<String>,
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
                                // No native field to read on a below-gate record,
                                // so this is the prefix-only derivation.
                                attribution_plugin: plugin_for(skill, None),
                                is_subagent: record.is_sidechain,
                                work: usage.input_tokens.saturating_add(usage.output_tokens),
                                cache_write: usage.cache_creation_input_tokens,
                                cache_read: usage.cache_read_input_tokens,
                                reconstructed: true,
                                // Same missing/malformed -> 0 rule as the native
                                // path, so a reconstructed credit windows honestly.
                                timestamp_millis: record.timestamp.as_deref().and_then(parse_iso8601_millis).unwrap_or(0),
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

/// The names Claude Code's sub-agent spawn tool has shipped under: `Agent` on
/// current builds, `Task` on older ones. Only a `tool_use` under one of these
/// can spawn the `agent-<id>.jsonl` file the roll-up credits, so a matching id
/// on any other tool is ignored rather than trusted (issue #19).
const SPAWN_TOOLS: [&str; 2] = ["Agent", "Task"];

/// A skill's `attributionPlugin`: the record's own field when it has one, else
/// the `plugin:name` prefix. A reconstructed credit has no record field to
/// read, so it derives the plugin from the prefix alone; a native one prefers
/// what the record actually says.
fn plugin_for(skill: &str, native_plugin: Option<&str>) -> Option<String> {
    native_plugin.map(str::to_string).or_else(|| skill.split_once(':').map(|(p, _)| p.to_string()))
}

/// One sub-agent spawn's resolved attribution: the skill that was holding the
/// wheel when the spawn was issued, and that skill's plugin. A named pair
/// rather than a bare tuple, so the spawn map below reads as the
/// `tool_use_attribution(toolUseId -> skill, plugin)` view issue #19 names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpawnAttribution {
    skill: String,
    plugin: Option<String>,
}

/// A parent transcript's spawns, keyed by the `tool_use` block id a sub-agent's
/// `meta.json` records as its `toolUseId`.
type SpawnMap = HashMap<String, SpawnAttribution>;

/// Maps each sub-agent spawn in a PARENT transcript from its `tool_use` block
/// id to the skill that was holding the wheel when the spawn was issued --
/// the `tool_use_attribution(toolUseId -> skill, plugin)` view issue #19 needs,
/// derived per scan rather than persisted (no new schema for a walk that fires
/// on no current build).
///
/// Resolves each spawn native-first, exactly as ADR 0005 resolves everything
/// else: the record's own `attributionSkill` when present, else the active
/// skill issue #12's walk reconstructs. The fallback is what makes the roll-up
/// reachable at all -- its version gate admits only pre-attribution builds, and
/// on those the parent turn has no native `attributionSkill` by the gate's own
/// definition, so a native-only read would resolve nothing, always.
///
/// The walk rules are #12's, and deliberately so -- same append order, same
/// human-turn clear, same CREDIT-BEFORE-PUSH: a spawn is credited to the skill
/// already active, never to one the same record starts. A spawn with no skill
/// either way is omitted, so the caller drops it rather than guessing.
fn parent_spawn_attributions(content: &str) -> SpawnMap {
    let mut spawns = SpawnMap::new();
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
                // Snapshot the wheel-holder BEFORE walking this record's blocks,
                // so a `Skill` push here cannot back-date itself onto a spawn in
                // the same record (credit-before-push).
                let attribution = record.attribution_skill.clone().or_else(|| current_skill.clone());
                let Some(ReconContent::Blocks(blocks)) = record.message.as_ref().and_then(|m| m.content.as_ref())
                else {
                    continue;
                };
                for block in blocks {
                    if block.kind.as_deref() != Some("tool_use") {
                        continue;
                    }
                    match block.name.as_deref() {
                        Some(name) if SPAWN_TOOLS.contains(&name) => {
                            if let (Some(id), Some(skill)) = (&block.id, &attribution) {
                                let plugin = plugin_for(skill, record.attribution_plugin.as_deref());
                                spawns.insert(id.clone(), SpawnAttribution { skill: skill.clone(), plugin });
                            }
                        }
                        Some("Skill") => {
                            if let Some(skill) = block.input.as_ref().and_then(|i| i.skill.as_ref()) {
                                current_skill = Some(skill.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    spawns
}

/// Rolls a PRE-ATTRIBUTION sub-agent file's own work up to `skill`, the skill
/// that spawned it (issue #19). The version gate is the whole safety argument:
/// above it a sub-agent record self-attributes, so an absent `attributionSkill`
/// means "no skill was active in that turn" (measured across all 1,455 real
/// sub-agent files: absence spans the same builds as presence) and rolling up
/// there would fabricate attribution the harness deliberately withheld.
///
/// Only records LACKING their own attribution are credited -- native
/// own-attribution stays authoritative, so the roll-up can neither displace a
/// native credit nor double-count a message the native pass already counted.
///
/// Work is summed from THIS file's own `message.usage`, never a parent's
/// `toolUseResult.totalTokens` (ADR 0005), and dedups by `message.id` at the
/// store. Every row is `is_subagent` (so the include toggle gates it) and
/// `reconstructed` (so the credit is honestly labeled).
///
/// Unlike #12's walk this is STATELESS per record -- every unattributed record
/// credits the same spawning skill -- so it is safe on a tail read, which a
/// whole-file-stateful walk would not be.
fn rollup_subagent_rows(content: &str, skill: &str, plugin: Option<&str>) -> Vec<UsageRow> {
    if !file_is_below_gate(content) {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<ReconRecord>(line) else { continue };
        if record.kind.as_deref() != Some("assistant") || record.attribution_skill.is_some() {
            continue;
        }
        let Some(message) = record.message else { continue };
        let (Some(id), Some(usage)) = (message.id, message.usage) else { continue };
        rows.push(UsageRow {
            message_id: id,
            attribution_skill: skill.to_string(),
            attribution_plugin: plugin.map(str::to_string),
            // Sub-agent by provenance, exactly as the native sub-agent pass
            // stamps it (grill D3): never trust the record's own isSidechain.
            is_subagent: true,
            work: usage.input_tokens.saturating_add(usage.output_tokens),
            cache_write: usage.cache_creation_input_tokens,
            cache_read: usage.cache_read_input_tokens,
            reconstructed: true,
            timestamp_millis: record.timestamp.as_deref().and_then(parse_iso8601_millis).unwrap_or(0),
        });
    }
    rows
}

/// The `toolUseId` recorded in a sub-agent file's sibling
/// `agent-<id>.meta.json` -- the id of the parent `tool_use` block that spawned
/// it. `None` when the sidecar is absent, unreadable, or carries no
/// `toolUseId`: a workflow sub-agent has no such linkage (1,227 of 1,455 real
/// files), and with no linkage there is nothing to roll up.
fn spawn_tool_use_id(subagent_path: &Path) -> Option<String> {
    // `agent-<id>.jsonl` -> `agent-<id>.meta.json` (the stem is the whole
    // `agent-<id>`, so swapping the extension lands on the sidecar).
    let content = fs::read_to_string(subagent_path.with_extension("meta.json")).ok()?;
    serde_json::from_str::<AgentMeta>(&content).ok()?.tool_use_id
}

#[derive(Deserialize)]
struct AgentMeta {
    #[serde(rename = "toolUseId")]
    tool_use_id: Option<String>,
}

/// The transcript of the session that spawned `subagent_path`. Claude Code
/// writes a session's sub-agents under `<project_dir>/<session>/subagents/`
/// (workflow ones one level deeper), and the session's own transcript is the
/// sibling `<project_dir>/<session>.jsonl` -- so the session dir is the parent
/// of the `subagents` ancestor, at either depth. `None` when there is no
/// `subagents` ancestor, which means this is not a sub-agent path at all.
fn parent_transcript_path(subagent_path: &Path) -> Option<PathBuf> {
    let session_dir = subagent_path
        .ancestors()
        .find(|a| a.file_name().and_then(|n| n.to_str()) == Some("subagents"))?
        .parent()?;
    // Append, never `with_extension`: the session dir is the file stem in full,
    // so a `.` anywhere in it would otherwise be eaten as an extension.
    let mut path = session_dir.as_os_str().to_os_string();
    path.push(".jsonl");
    Some(PathBuf::from(path))
}

/// The rolled-up rows for one sub-agent file (issue #19), or `None` when the
/// roll-up does not apply -- an above-gate build, a missing linkage, an
/// unreadable parent, or a spawn whose own turn was unattributed.
///
/// `memo` caches each parent transcript's spawn map for the scan, so N
/// sub-agent files sharing one session read and walk that parent exactly once,
/// and re-reads it even when the checkpoint says the parent is fresh (the
/// roll-up needs the parent's CONTENT, not its usage rows).
///
/// An UNREADABLE parent is deliberately not cached: it is "unknown", never "no
/// spawns". Caching the failure would conflate the two and silently strip the
/// credit from every sibling sub-agent of that session for the whole scan --
/// the same error-is-not-absence distinction `enumerated_dirs` draws for the
/// prune (ADR 0024). Not caching it costs at most one retry per sibling and
/// lets a transient failure recover within the pass.
fn rollup_rows_for(path: &Path, text: &str, memo: &mut HashMap<PathBuf, SpawnMap>) -> Option<Vec<UsageRow>> {
    // Gate FIRST. Every file on a real machine is above it, so the common case
    // bails after one cheap version probe -- never touching the meta sidecar or
    // re-reading a parent transcript.
    if !file_is_below_gate(text) {
        return None;
    }
    let tool_use_id = spawn_tool_use_id(path)?;
    let parent_path = parent_transcript_path(path)?;
    if !memo.contains_key(&parent_path) {
        let content = fs::read_to_string(&parent_path).ok()?;
        memo.insert(parent_path.clone(), parent_spawn_attributions(&content));
    }
    let spawn = memo.get(&parent_path)?.get(&tool_use_id)?;
    Some(rollup_subagent_rows(text, &spawn.skill, spawn.plugin.as_deref()))
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

/// Incremental usage ingest (issue #5, extended for the sub-agent toggle in
/// issue #13 and the prune + tail-reader in issue #15). Takes the already-
/// enumerated main-thread refs `scan_all` built for the listing index plus,
/// when the user opted in, the sub-agent refs (so a scan enumerates each dir
/// once), plus the set of dirs whose enumeration actually SUCCEEDED
/// (`enumerated_dirs`), which the prune step needs to tell a real deletion from
/// a transient read failure.
///
/// The two ref lists carry provenance: `main_transcripts` parse with
/// `force_subagent = false` (they fall back to `isSidechain`), while
/// `subagent_transcripts` parse with `force_subagent = true` so their rows are
/// always tagged `is_subagent` and stay out of the default headline (grill D3).
///
/// Two hygiene jobs wrap the per-file loop (issue #15, ADR 0024):
/// - **Prune by rebuild.** If a checkpointed transcript has genuinely vanished
///   (absent from a dir that actually enumerated), `wipe` both tables and let
///   the loop re-ingest the present set. Rows carry no per-path provenance and a
///   `message.id` lives in many transcripts, so a targeted delete is unsafe; a
///   rebuild via `message.id` dedup is the only correct prune (a still-present
///   id survives, an only-in-vanished id drops). Pruned over the UNION of both
///   ref lists, so a toggle-off scan collects the previous run's sub-agent
///   checkpoints too.
/// - **Tail-read.** A file is opened only when its `(mtime, size)` changed; a
///   grown file with an intact prefix reads only its appended bytes.
///
/// Rows are written INSERT OR IGNORE (native rows `ON CONFLICT DO UPDATE`), so
/// any re-read is idempotent and both a mis-tailed rewrite and a cross-file
/// duplicate `message.id` collapse to one count.
pub fn refresh_usage(
    main_transcripts: &[TranscriptRef],
    subagent_transcripts: &[TranscriptRef],
    enumerated_dirs: &HashSet<PathBuf>,
    cache: &SqliteUsageCache,
) -> UsageStats {
    let mut stats = UsageStats::default();

    // Conditional full rebuild on a genuine vanish (issue #15, ADR 0024), done
    // BEFORE the per-file loop so a wipe drops every checkpoint and the loop then
    // re-reads (Full) and re-ingests the surviving present set from scratch --
    // INSERT OR IGNORE dedup makes that rebuild re-derive correct totals (a
    // still-present `message.id` survives, an only-in-vanished one drops).
    // Pruned over the UNION of both ref lists. The dir-scoped check inside
    // `has_vanished_checkpoint` already ignores dirs that failed to enumerate,
    // so an empty enumeration (total read failure) reports nothing vanished and
    // never wipes -- no separate "is_empty" guard is needed or correct.
    let seen: HashSet<String> = main_transcripts
        .iter()
        .chain(subagent_transcripts)
        .map(|t| t.path.to_string_lossy().into_owned())
        .collect();
    let enumerated: HashSet<String> =
        enumerated_dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect();
    if cache.has_vanished_checkpoint(&seen, &enumerated) {
        cache.wipe();
    }

    ingest_transcripts(main_transcripts, false, cache, &mut stats);
    ingest_transcripts(subagent_transcripts, true, cache, &mut stats);

    stats
}

/// Reads and ingests each changed transcript in `transcripts`, stamping every
/// emitted row's `is_subagent` when `force_subagent` (its provenance). Shared
/// by the main and sub-agent passes so they gate, read, and mark identically.
/// A file is opened only when its `(mtime, size)` changed; a grown file with an
/// intact prefix is tail-read (issue #15), and only a MAIN-thread FULL read runs
/// the whole-file-stateful reconstruction pass (issue #12).
fn ingest_transcripts(
    transcripts: &[TranscriptRef],
    force_subagent: bool,
    cache: &SqliteUsageCache,
    stats: &mut UsageStats,
) {
    // Parent spawn maps for the roll-up (issue #19), memoized for this pass so
    // the many sub-agent files of one session walk their shared parent once.
    // Stays empty on the main pass and on every current-build sub-agent pass,
    // since the gate bails before a parent is ever read.
    let mut parent_spawns: HashMap<PathBuf, SpawnMap> = HashMap::new();

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
        // line stays unparsed and is re-read (as a tail) once completed. It
        // changes `(mtime, size)`, so the file re-reads and the once-partial
        // record then counts exactly once.
        let appended = &bytes[slice_start..];
        let consumed = appended.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
        // The consumed slice ends exactly on a '\n' (a char boundary), so it is
        // valid UTF-8 whenever the file is; a non-UTF-8 file is skipped without
        // advancing the gate, exactly as the old read_to_string path did.
        let Ok(text) = std::str::from_utf8(&appended[..consumed]) else { continue };
        cache.ingest(&parse_usage_rows(text, force_subagent));
        // Reconstruct attributed usage for pre-attribution builds (issue #12),
        // but ONLY on a MAIN-thread FULL read (issue #15 integration).
        // `reconstruct_usage_rows` is whole-file-stateful (it carries the active
        // skill across lines), so a byte-offset TAIL read -- which starts mid-
        // file -- would miscredit; only a Full read sees the whole transcript
        // (`text` is then the whole file). Safe because below-gate (pre-
        // attribution) files are static and always read Full, while the active
        // growing file is above-gate and reconstruction no-ops there anyway.
        // Native rows land first (a message with native attribution is never
        // displaced by a guess), and within one file the two passes never emit
        // the same `message.id`. Sub-agent reconstruction (the parentUuid walk)
        // stays a deferred follow-up (ADR 0005), so that pass parses native rows
        // alone.
        if !force_subagent && !is_tail {
            cache.ingest(&reconstruct_usage_rows(text));
        }
        // The sub-agent pass's counterpart (issue #19): roll a pre-attribution
        // sub-agent file's unattributed work up to the skill that spawned it.
        // Unlike the walk above this needs no `!is_tail` guard -- it is
        // stateless per record (every unattributed record credits the same
        // spawning skill), so a tail that starts mid-file still credits
        // correctly. Native rows land first and are never displaced: the
        // roll-up skips any record carrying its own attribution, and its rows
        // are INSERT OR IGNORE regardless.
        if force_subagent {
            if let Some(rows) = rollup_rows_for(&transcript.path, text, &mut parent_spawns) {
                cache.ingest(&rows);
            }
        }
        let new_off = base + consumed as u64;
        if let Some(m) = mnanos {
            cache.mark(&path_key, m, size, new_off as i64);
        }
    }
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
/// fabricated). Takes the already-computed totals rather than the cache so the
/// all-time and windowed index builders (issue #14) share one folding path, and
/// the sub-agent include toggle (issue #13) is decided by the caller's choice of
/// `totals` / `totals_since` query.
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
    /// All-time per-skill usage (issue #5), honoring the sub-agent include
    /// toggle (issue #13): the shipped cumulative figures.
    pub fn build(cache: &SqliteUsageCache, include_subagents: bool) -> Self {
        UsageIndex { by_key: usage_by_key(cache.totals(include_subagents)) }
    }

    /// Per-skill usage restricted to records at or after `cutoff_millis` (issue
    /// #14): the rolling-window counterpart, same folding and same sub-agent
    /// toggle, only the totals query is bounded. A record with a 0 timestamp
    /// (unparseable) never lands in a positive-cutoff window.
    pub fn build_windowed(cache: &SqliteUsageCache, cutoff_millis: i64, include_subagents: bool) -> Self {
        UsageIndex { by_key: usage_by_key(cache.totals_since(cutoff_millis, include_subagents)) }
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
    use super::super::usage_cache::UsageTotal;
    use crate::domain::report::AttributionSource;
    use std::collections::HashSet;
    use std::time::{Duration, UNIX_EPOCH};

    /// Enumerate a single dir into the `TranscriptRef`s `refresh_usage` now
    /// takes, re-stat'd fresh so an appended/rewritten file is seen.
    fn refs(dir: &Path) -> Vec<TranscriptRef> {
        transcript_refs_by_recency(&[dir.to_path_buf()]).0
    }

    /// Enumerate a single dir fresh (re-stat'd so an append/rewrite is seen)
    /// and run one MAIN-thread usage refresh over it -- the common case here.
    fn scan(dir: &Path, cache: &SqliteUsageCache) -> UsageStats {
        let (refs, dirs) = transcript_refs_by_recency(&[dir.to_path_buf()]);
        refresh_usage(&refs, &[], &dirs, cache)
    }

    /// The successfully-enumerated dir set for a single dir, threaded into the
    /// merged `refresh_usage`'s prune arg from the sub-agent tests.
    fn enumerated(dir: &Path) -> HashSet<PathBuf> {
        transcript_refs_by_recency(&[dir.to_path_buf()]).1
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

    /// An assistant line carrying a top-level RFC3339 `timestamp`, built on the
    /// base fixture so only the timestamp field is added (issue #14).
    fn assistant_line_at(message_id: &str, skill: &str, timestamp: &str, work: u32) -> String {
        let mut rec: serde_json::Value =
            serde_json::from_str(&assistant_line(message_id, "u", Some(skill), None, work, 0, 0, 0)).unwrap();
        rec["timestamp"] = json!(timestamp);
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
                model_invocable: true,
            },
            body: String::new(),
            manager_root: None,
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

        let cold = scan(&dir, &cache);
        assert_eq!(cold.files_read, 1);
        let cold_total = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;

        let warm = scan(&dir, &cache);
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
        scan(&dir, &cache);

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", assistant_line("m2", "u2", Some("grilling"), None, 7, 0, 0, 0)).unwrap();
        drop(f);

        let stats = scan(&dir, &cache);
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
        scan(&dir, &cache);
        let after_partial = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
        assert_eq!(after_partial, 10, "the partial line is skipped, not counted");

        // Complete the file; the once-partial record now counts exactly once.
        let m2 = assistant_line("m2", "u2", Some("grilling"), None, 5, 0, 0, 0);
        fs::write(&path, format!("{good}\n{m2}\n")).unwrap();
        scan(&dir, &cache);
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
        scan(&dir, &cache);

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
        let stats = scan(&dir, &cache);

        assert_eq!(stats.files_read, 1, "only the depth-1 main.jsonl is enumerated, never the subagents/ file");
        let total = UsageIndex::build(&cache, false).for_skill(&skill(SkillId::Personal { name: "grilling".to_string() })).unwrap().work;
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
        let rows = parse_usage_rows(&assistant_line_at("m1", "grilling", "2026-06-27T02:13:52.480Z", 10), false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp_millis, 1_782_526_432_480);
    }

    #[test]
    fn a_record_without_a_timestamp_defaults_to_zero_not_dropped() {
        // The base fixture carries no top-level timestamp.
        let rows = parse_usage_rows(&assistant_line("m1", "u1", Some("grilling"), None, 10, 0, 0, 0), false);
        assert_eq!(rows.len(), 1, "a timestamp-less record is still counted, never dropped");
        assert_eq!(rows[0].timestamp_millis, 0, "it degrades to 0 (oldest)");
    }

    #[test]
    fn a_malformed_timestamp_defaults_to_zero() {
        let rows = parse_usage_rows(&assistant_line_at("m1", "grilling", "yesterday-ish", 10), false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp_millis, 0, "an unparseable timestamp degrades to 0, row kept");
    }

    #[test]
    fn windowed_index_credits_only_in_window_usage() {
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        cache.ingest(&parse_usage_rows(&assistant_line_at("old", "grilling", "2020-01-01T00:00:00Z", 100), false));
        cache.ingest(&parse_usage_rows(&assistant_line_at("new", "grilling", "2026-06-27T02:13:52.480Z", 40), false));
        let g = skill(SkillId::Personal { name: "grilling".to_string() });

        // All-time sees both; a cutoff between the two records sees only the recent one.
        assert_eq!(UsageIndex::build(&cache, false).for_skill(&g).unwrap().work, 140);
        let cutoff = 1_600_000_000_000; // 2020-09, after the old record, before the new one
        assert_eq!(UsageIndex::build_windowed(&cache, cutoff, false).for_skill(&g).unwrap().work, 40);
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
        refresh_usage(&refs(&dir), &subagent_transcript_refs(std::slice::from_ref(&dir)), &enumerated(&dir), &cache);

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
        refresh_usage(&[], &subagent_transcript_refs(std::slice::from_ref(&dir)), &enumerated(&dir), &cache);

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
        refresh_usage(&[], &subagent_transcript_refs(std::slice::from_ref(&dir)), &enumerated(&dir), &cache);

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
        refresh_usage(&[], &subagent_transcript_refs(std::slice::from_ref(&dir)), &enumerated(&dir), &cache);

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
        refresh_usage(&[], &subagent_transcript_refs(std::slice::from_ref(&dir)), &enumerated(&dir), &cache);

        assert_eq!(
            UsageIndex::build(&cache, true).for_skill(&grilling()).unwrap().work,
            999,
            "credited from the file's own usage (999), never the toolUseResult total (999,999)"
        );
    }

    // ---- issue #15: byte-offset tail-reader ----

    /// This machine's grilling work total, the metric most #15 tests assert on.
    fn grilling_work(cache: &SqliteUsageCache) -> u64 {
        UsageIndex::build(cache, false)
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
        let mut incr_totals = incr.totals(false);
        let mut ctrl_totals = control.totals(false);
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
        let stats = refresh_usage(&[], &[], &no_dirs, &cache);
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
        refresh_usage(&refs, &[], &dirs, &cache);
        assert_eq!(grilling_work(&cache), 30);

        // repo-b becomes unreadable (here: removed, so read_dir fails -- the
        // portable stand-in for any transient enumeration failure). Its
        // checkpoint is "unknown", never a vanish, so nothing is wiped and mB's
        // usage survives even though b.jsonl is absent from this scan.
        fs::remove_dir_all(&dir_b).unwrap();
        let (refs2, dirs2) = transcript_refs_by_recency(&[dir_a.clone(), dir_b.clone()]);
        assert!(!dirs2.contains(&dir_b), "an unreadable dir is not in the enumerated set");
        refresh_usage(&refs2, &[], &dirs2, &cache);

        assert_eq!(
            grilling_work(&cache),
            30,
            "a transiently unreadable dir must not wipe its transcripts' usage (data-loss guard)",
        );
    }

    // ---- issue #15 x #12 integration: reconstruction gates to Full reads ----

    #[test]
    fn reconstruction_runs_on_a_full_read_but_not_on_a_tail_read() {
        // A below-gate (pre-attribution) transcript reconstructs on the cold Full
        // read; appending a native record then Tail-reads only the appended bytes
        // and must NOT re-run the whole-file-stateful reconstruction walk -- a
        // tail starts mid-file and would miscredit (the merged #15 x #12 rule).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo-a");
        let path = dir.join("s.jsonl");

        // Pre-attribution build (< 2.1.146), NO native attributionSkill anywhere:
        // a human turn, a `Skill` invoke of grilling, then a turn the walk credits.
        let human = json!({
            "type": "user", "version": "2.1.145",
            "message": {"role": "user", "content": "help me"}
        });
        let invoke = json!({
            "type": "assistant", "version": "2.1.145", "uuid": "m_inv",
            "message": {"id": "m_inv", "role": "assistant",
                "content": [{"type": "tool_use", "name": "Skill", "input": {"skill": "grilling"}}],
                "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        });
        let turn = json!({
            "type": "assistant", "version": "2.1.145", "uuid": "m1",
            "message": {"id": "m1", "role": "assistant",
                "content": [{"type": "text", "text": "asking a question"}],
                "usage": {"input_tokens": 30, "output_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        });
        write_transcript(&dir, "s.jsonl", &[human.to_string(), invoke.to_string(), turn.to_string()]);
        set_mtime(&path, 1000);

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        let cold = scan(&dir, &cache);
        assert_eq!(cold.full_reads, 1, "the cold pass is a Full read");
        assert_eq!(cold.tail_reads, 0);
        assert_eq!(grilling_work(&cache), 40, "the post-invoke turn is reconstructed on the Full read");
        assert_eq!(
            UsageIndex::build(&cache, false).for_skill(&grilling()).unwrap().attribution_source,
            AttributionSource::Reconstructed,
            "the Full-read credit is honestly labeled reconstructed",
        );

        // Append a NATIVE record (carries attributionSkill). The file grows, so
        // the next scan Tail-reads only the appended bytes.
        append_line(&path, &assistant_line("m2", "u2", Some("grilling"), None, 5, 0, 0, 0));
        set_mtime(&path, 2000);

        let stats = scan(&dir, &cache);
        assert_eq!(stats.tail_reads, 1, "the grown file is tail-read");
        assert_eq!(stats.full_reads, 0);
        assert_eq!(
            grilling_work(&cache),
            45,
            "the tail landed only the native m2 (5); reconstruction did NOT re-run on the tail",
        );
    }

    // ---- issue #19: version-gated parent-spawn roll-up for sub-agent usage ----

    /// The expected resolution of one spawn, spelled without the struct noise.
    fn attr(skill: &str, plugin: Option<&str>) -> SpawnAttribution {
        SpawnAttribution { skill: skill.to_string(), plugin: plugin.map(str::to_string) }
    }

    /// A pre-attribution build (< the gate): the only window the roll-up runs
    /// in, and the only one where a sub-agent record's missing attribution
    /// means "too old to attribute" rather than "no skill was active".
    const OLD: &str = "2.1.145";
    /// A current build (>= the gate), where the child self-attributes.
    const NEW: &str = "2.1.197";

    /// An assistant record spawning a sub-agent. `tool` is the spawn tool's
    /// name (`Agent` on current builds, `Task` on older ones); `skill` is the
    /// record's NATIVE `attributionSkill`, which a below-gate build never
    /// writes.
    fn spawn_line(version: &str, tool: &str, tool_use_id: &str, skill: Option<&str>) -> String {
        let mut rec = json!({
            "type": "assistant", "version": version, "uuid": "u_spawn",
            "message": {"id": "m_spawn", "role": "assistant",
                "content": [{"type": "tool_use", "id": tool_use_id, "name": tool, "input": {}}],
                "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        });
        if let Some(s) = skill {
            rec["attributionSkill"] = json!(s);
        }
        rec.to_string()
    }

    /// An assistant record whose `Skill` tool_use hands the wheel to `skill`.
    fn skill_invoke_line(version: &str, skill: &str) -> String {
        json!({
            "type": "assistant", "version": version, "uuid": "u_inv",
            "message": {"id": "m_inv", "role": "assistant",
                "content": [{"type": "tool_use", "id": "tu_inv", "name": "Skill", "input": {"skill": skill}}],
                "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        })
        .to_string()
    }

    /// A typed human prompt, which hands the wheel back (clears the skill).
    fn human_line(version: &str) -> String {
        json!({"type": "user", "version": version, "message": {"role": "user", "content": "help me"}}).to_string()
    }

    /// A sub-agent assistant record with NO own attribution -- the roll-up's
    /// target. `isSidechain` is set, as every real sub-agent record carries it.
    fn unattributed_subagent_line(version: &str, message_id: &str, work: u32) -> String {
        json!({
            "type": "assistant", "version": version, "isSidechain": true, "uuid": message_id,
            "timestamp": "2026-06-27T02:13:52.480Z",
            "message": {"id": message_id, "role": "assistant",
                "content": [{"type": "text", "text": "working"}],
                "usage": {"input_tokens": work, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        })
        .to_string()
    }

    /// Lays out the real on-disk shape Claude Code writes for a session that
    /// spawned one sub-agent:
    ///   `<project_dir>/<session>.jsonl`                       -- the parent
    ///   `<project_dir>/<session>/subagents/agent-<id>.jsonl`  -- the child
    ///   `<project_dir>/<session>/subagents/agent-<id>.meta.json`
    /// `meta_tool_use_id` is `None` to model a workflow sub-agent, whose meta
    /// sidecar carries no `toolUseId` at all. Returns the project dir.
    fn write_session(
        tmp: &Path,
        parent_lines: &[String],
        child_lines: &[String],
        meta_tool_use_id: Option<&str>,
    ) -> PathBuf {
        let project_dir = tmp.join("repo-a");
        let session = "12d57136-d085-4c6a-8d3d-1c3f156c9ea2";
        write_transcript(&project_dir, &format!("{session}.jsonl"), parent_lines);
        let subagents = project_dir.join(session).join("subagents");
        write_transcript(&subagents, "agent-abc123.jsonl", child_lines);
        let meta = match meta_tool_use_id {
            Some(id) => json!({"agentType": "Explore", "toolUseId": id, "spawnDepth": 1}),
            None => json!({"agentType": "Explore", "spawnDepth": 1}),
        };
        fs::write(subagents.join("agent-abc123.meta.json"), meta.to_string()).unwrap();
        project_dir
    }

    /// Run one sub-agent-only usage pass over `project_dir`. Main-thread refs
    /// are deliberately empty, so any credit observed came from the roll-up
    /// reading the parent directly -- never from the parent being ingested as a
    /// main transcript in its own right.
    fn subagent_scan(project_dir: &Path, cache: &SqliteUsageCache) -> UsageStats {
        let dirs = [project_dir.to_path_buf()];
        refresh_usage(&[], &subagent_transcript_refs(&dirs), &enumerated(project_dir), cache)
    }

    #[test]
    fn parent_transcript_path_resolves_the_sibling_session_transcript() {
        // Depth 1: <project>/<session>/subagents/agent-x.jsonl
        let direct = Path::new("/p/repo-a/sess-1/subagents/agent-x.jsonl");
        assert_eq!(parent_transcript_path(direct), Some(PathBuf::from("/p/repo-a/sess-1.jsonl")));
        // Depth 2: a workflow sub-agent still belongs to the same session.
        let workflow = Path::new("/p/repo-a/sess-1/subagents/workflows/wf_abc/agent-y.jsonl");
        assert_eq!(parent_transcript_path(workflow), Some(PathBuf::from("/p/repo-a/sess-1.jsonl")));
        // A path with no `subagents` ancestor has no derivable parent.
        assert_eq!(parent_transcript_path(Path::new("/p/repo-a/sess-1.jsonl")), None);
    }

    #[test]
    fn spawn_tool_use_id_reads_the_meta_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_session(tmp.path(), &[], &[], Some("toolu_01FFX17C8n4PWFhydJu7S9AP"));
        let child = dir.join("12d57136-d085-4c6a-8d3d-1c3f156c9ea2/subagents/agent-abc123.jsonl");
        assert_eq!(spawn_tool_use_id(&child), Some("toolu_01FFX17C8n4PWFhydJu7S9AP".to_string()));

        // A workflow sub-agent's sidecar carries no toolUseId: no linkage, so
        // no roll-up (the 1,227-file case measured in issue #19).
        let dir2 = write_session(&tmp.path().join("b"), &[], &[], None);
        let child2 = dir2.join("12d57136-d085-4c6a-8d3d-1c3f156c9ea2/subagents/agent-abc123.jsonl");
        assert_eq!(spawn_tool_use_id(&child2), None);
    }

    #[test]
    fn parent_spawn_attributions_maps_a_native_agent_spawn_to_its_skill() {
        // The `Agent` spawn tool on a build that natively attributes the turn.
        let parent = spawn_line(NEW, "Agent", "toolu_1", Some("claude-md-generator"));
        let spawns = parent_spawn_attributions(&parent);
        assert_eq!(spawns.get("toolu_1"), Some(&attr("claude-md-generator", None)));
    }

    #[test]
    fn parent_spawn_attributions_reads_a_plugin_spawns_plugin() {
        let mut rec: serde_json::Value =
            serde_json::from_str(&spawn_line(NEW, "Agent", "toolu_1", Some("superpowers:executing-plans"))).unwrap();
        rec["attributionPlugin"] = json!("superpowers");
        let spawns = parent_spawn_attributions(&rec.to_string());
        assert_eq!(spawns.get("toolu_1"), Some(&attr("superpowers:executing-plans", Some("superpowers"))));
    }

    #[test]
    fn parent_spawn_attributions_falls_back_to_the_reconstructed_active_skill() {
        // A pre-attribution parent: no native attributionSkill anywhere, so the
        // spawn's skill comes from issue #12's walk (the `Skill` invoke above
        // it). `Task` is the older builds' spawn tool name.
        let parent = [
            human_line(OLD),
            skill_invoke_line(OLD, "grilling"),
            spawn_line(OLD, "Task", "toolu_1", None),
        ]
        .join("\n");
        let spawns = parent_spawn_attributions(&parent);
        assert_eq!(spawns.get("toolu_1"), Some(&attr("grilling", None)));
    }

    #[test]
    fn parent_spawn_attributions_clears_the_skill_on_a_fresh_human_turn() {
        // The user took the wheel back before the spawn: nothing was active, so
        // the spawn is unattributed and must not inherit the stale skill.
        let parent = [
            skill_invoke_line(OLD, "grilling"),
            human_line(OLD),
            spawn_line(OLD, "Task", "toolu_1", None),
        ]
        .join("\n");
        assert_eq!(parent_spawn_attributions(&parent).get("toolu_1"), None);
    }

    #[test]
    fn parent_spawn_attributions_credits_a_spawn_before_a_same_record_skill_push() {
        // One record both spawns an agent AND invokes a new skill. Credit-
        // before-push (issue #12's rule): the spawn belongs to the skill that
        // was ALREADY active, never the one this same record starts.
        let record = json!({
            "type": "assistant", "version": OLD, "uuid": "u1",
            "message": {"id": "m1", "role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Task", "input": {}},
                {"type": "tool_use", "id": "tu_inv", "name": "Skill", "input": {"skill": "ship"}}
            ], "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        });
        let parent = [skill_invoke_line(OLD, "grilling"), record.to_string()].join("\n");
        assert_eq!(
            parent_spawn_attributions(&parent).get("toolu_1"),
            Some(&attr("grilling", None)),
            "the spawn belongs to the already-active grilling, not the ship it pushes",
        );
    }

    #[test]
    fn parent_spawn_attributions_ignores_a_non_spawn_tool_use() {
        // A `Read` tool_use is not a sub-agent spawn and must never enter the map.
        let parent = [skill_invoke_line(OLD, "grilling"), spawn_line(OLD, "Read", "toolu_1", None)].join("\n");
        assert!(!parent_spawn_attributions(&parent).contains_key("toolu_1"));
    }

    #[test]
    fn a_below_gate_subagent_rolls_up_to_its_spawning_skill() {
        // AC1: a pre-attribution sub-agent file with no own attribution, whose
        // spawning parent turn IS attributed (here via issue #12's walk), gets
        // its OWN work credited to that skill, tagged reconstructed.
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_session(
            tmp.path(),
            &[human_line(OLD), skill_invoke_line(OLD, "grilling"), spawn_line(OLD, "Task", "toolu_1", None)],
            &[unattributed_subagent_line(OLD, "m_sub", 999)],
            Some("toolu_1"),
        );

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        subagent_scan(&dir, &cache);

        let rolled = UsageIndex::build(&cache, true).for_skill(&grilling()).unwrap();
        assert_eq!(rolled.work, 999, "the sub-agent file's own work rolls up to the spawning skill");
        assert_eq!(
            rolled.attribution_source,
            AttributionSource::Reconstructed,
            "a rolled-up credit is honestly labeled reconstructed, never native",
        );
    }

    #[test]
    fn a_natively_attributed_parent_wins_over_the_reconstruction_fallback() {
        // AC1 read literally: "whose spawning parent turn IS attributed". The
        // combination is synthetic -- a below-gate build writes no native
        // attribution, so a native parent and a below-gate child cannot co-occur
        // on a real machine -- but it is the only way to drive the native branch
        // end to end, and it pins the priority order: where a parent carries its
        // own attributionSkill, THAT wins and the walk is never consulted. Here
        // the walk would say `ship`; native says `grilling`.
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_session(
            tmp.path(),
            &[skill_invoke_line(OLD, "ship"), spawn_line(OLD, "Task", "toolu_1", Some("grilling"))],
            &[unattributed_subagent_line(OLD, "m_sub", 999)],
            Some("toolu_1"),
        );

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        subagent_scan(&dir, &cache);

        assert_eq!(
            UsageIndex::build(&cache, true).for_skill(&grilling()).unwrap().work,
            999,
            "the parent's own attributionSkill wins; the reconstructed `ship` never gets a look in",
        );
        let shipped = skill(SkillId::Personal { name: "ship".to_string() });
        assert!(UsageIndex::build(&cache, true).for_skill(&shipped).is_none());
    }

    #[test]
    fn an_unreadable_parent_is_not_cached_as_having_no_spawns() {
        // Error is not absence (the distinction ADR 0024 draws for the prune).
        // Two sub-agent files share one session; the parent is missing when the
        // first is processed and present for the second. Caching the failed read
        // as an empty spawn map would strip the second's credit too.
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_session(
            tmp.path(),
            &[skill_invoke_line(OLD, "grilling"), spawn_line(OLD, "Task", "toolu_1", None)],
            &[unattributed_subagent_line(OLD, "m_a", 10)],
            Some("toolu_1"),
        );
        let session = "12d57136-d085-4c6a-8d3d-1c3f156c9ea2";
        let subagents = dir.join(session).join("subagents");
        write_transcript(&subagents, "agent-zzz.jsonl", &[unattributed_subagent_line(OLD, "m_b", 7)]);
        fs::write(subagents.join("agent-zzz.meta.json"), json!({"toolUseId": "toolu_1"}).to_string()).unwrap();

        // Drive the memo directly: an unreadable parent must leave no entry
        // behind, so a later sibling still resolves once the parent is readable.
        let mut memo: HashMap<PathBuf, SpawnMap> = HashMap::new();
        let child_a = subagents.join("agent-abc123.jsonl");
        let text = fs::read_to_string(&child_a).unwrap();
        let parent = dir.join(format!("{session}.jsonl"));
        let saved = fs::read_to_string(&parent).unwrap();
        fs::remove_file(&parent).unwrap();

        assert!(rollup_rows_for(&child_a, &text, &mut memo).is_none(), "an unreadable parent rolls up nothing");
        assert!(memo.is_empty(), "the failed read must NOT be cached as an empty spawn map");

        fs::write(&parent, saved).unwrap();
        let rows = rollup_rows_for(&child_a, &text, &mut memo).expect("the retry resolves once the parent is back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attribution_skill, "grilling");
    }

    #[test]
    fn rollup_subagent_rows_gates_itself_on_the_build_version() {
        // The gate is the roll-up's whole safety argument, so it lives in the
        // pure function -- not only in the caller's fast path (which skips the
        // sidecar and parent I/O). Pins it directly: `rollup_rows_for`'s early
        // bail must never be the ONLY thing standing between a current build
        // and a fabricated credit.
        let current = unattributed_subagent_line(NEW, "m_sub", 999);
        assert!(
            rollup_subagent_rows(&current, "grilling", None).is_empty(),
            "an above-gate file must roll up nothing even when handed a skill directly",
        );
        let old = unattributed_subagent_line(OLD, "m_sub", 999);
        assert_eq!(rollup_subagent_rows(&old, "grilling", None).len(), 1, "a below-gate file still rolls up");
    }

    #[test]
    fn a_current_build_subagent_with_no_attribution_credits_nothing() {
        // AC2: the version gate. Same linkage, same attributed parent, but a
        // current build -- where an absent child attribution means "no skill was
        // active". Rolling up here would fabricate what the harness withheld.
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_session(
            tmp.path(),
            &[spawn_line(NEW, "Agent", "toolu_1", Some("grilling"))],
            &[unattributed_subagent_line(NEW, "m_sub", 999)],
            Some("toolu_1"),
        );

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        subagent_scan(&dir, &cache);

        assert!(
            UsageIndex::build(&cache, true).for_skill(&grilling()).is_none(),
            "on a current build the roll-up must never fire, even with a linked, attributed parent",
        );
    }

    #[test]
    fn an_unattributed_spawning_parent_credits_nothing() {
        // The parent turn itself has no skill (native or reconstructed): the
        // linkage exists but resolves to nothing, so the credit is dropped.
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_session(
            tmp.path(),
            &[human_line(OLD), spawn_line(OLD, "Task", "toolu_1", None)],
            &[unattributed_subagent_line(OLD, "m_sub", 999)],
            Some("toolu_1"),
        );

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        subagent_scan(&dir, &cache);
        assert_eq!(cache.totals(true).len(), 0, "an unattributed parent credits nothing, never a guess");
    }

    #[test]
    fn a_subagent_with_no_meta_sidecar_credits_nothing() {
        // A workflow sub-agent (no toolUseId): no linkage to walk, so no credit.
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_session(
            tmp.path(),
            &[skill_invoke_line(OLD, "grilling"), spawn_line(OLD, "Task", "toolu_1", None)],
            &[unattributed_subagent_line(OLD, "m_sub", 999)],
            None,
        );

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        subagent_scan(&dir, &cache);
        assert_eq!(cache.totals(true).len(), 0, "no toolUseId linkage means no roll-up");
    }

    #[test]
    fn a_subagent_record_with_its_own_attribution_is_not_rolled_up() {
        // Native own-attribution stays authoritative: only records LACKING it
        // are roll-up candidates, so the roll-up can never displace a native
        // credit or double-count the same message.
        let rows = rollup_subagent_rows(
            &[
                unattributed_subagent_line(OLD, "m_none", 10),
                json!({
                    "type": "assistant", "version": OLD, "isSidechain": true, "uuid": "m_own",
                    "attributionSkill": "ship",
                    "message": {"id": "m_own", "role": "assistant",
                        "usage": {"input_tokens": 500, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
                })
                .to_string(),
            ]
            .join("\n"),
            "grilling",
            None,
        );
        assert_eq!(rows.len(), 1, "only the unattributed record rolls up");
        assert_eq!(rows[0].message_id, "m_none");
        assert_eq!(rows[0].work, 10);
    }

    #[test]
    fn rolled_up_rows_dedup_by_message_id_and_never_read_tool_use_result() {
        // AC3. The same message.id repeated (a content-block split) counts once,
        // and a toolUseResult total in the file is never the source of a sum.
        let tmp = tempfile::tempdir().unwrap();
        let tool_use_result =
            json!({"type":"user","version":OLD,"toolUseResult":{"totalTokens":999_999,"usage":{"input_tokens":999_999,"output_tokens":0}}}).to_string();
        let dir = write_session(
            tmp.path(),
            &[skill_invoke_line(OLD, "grilling"), spawn_line(OLD, "Task", "toolu_1", None)],
            &[
                unattributed_subagent_line(OLD, "m_sub", 50),
                unattributed_subagent_line(OLD, "m_sub", 50), // same message.id
                tool_use_result,
            ],
            Some("toolu_1"),
        );

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        subagent_scan(&dir, &cache);
        assert_eq!(
            UsageIndex::build(&cache, true).for_skill(&grilling()).unwrap().work,
            50,
            "one message counts once (50), and the toolUseResult total (999,999) is never read",
        );
    }

    #[test]
    fn rolled_up_rows_are_subagent_tagged_so_the_include_toggle_gates_them() {
        // AC4: a rolled-up credit is sub-agent work by provenance, so it stays
        // out of the default headline and surfaces only with the toggle on.
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_session(
            tmp.path(),
            &[skill_invoke_line(OLD, "grilling"), spawn_line(OLD, "Task", "toolu_1", None)],
            &[unattributed_subagent_line(OLD, "m_sub", 999)],
            Some("toolu_1"),
        );

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        subagent_scan(&dir, &cache);

        assert!(
            UsageIndex::build(&cache, false).for_skill(&grilling()).is_none(),
            "the default headline excludes rolled-up sub-agent work",
        );
        assert_eq!(UsageIndex::build(&cache, true).for_skill(&grilling()).unwrap().work, 999);
    }

    /// Exercises the roll-up against this machine's real `~/.claude` -- the
    /// CLAUDE.md verification bar, which tempdir unit tests cannot meet. The
    /// headline assertion is a NEGATIVE one, and it is the point of issue #19:
    /// every real transcript is far above the gate, so the roll-up must fire on
    /// zero files and leave every total byte-identical. A regression that
    /// widened the gate would light this up immediately. Run by hand:
    /// `cargo test --manifest-path src-tauri/Cargo.toml
    /// adapters::claude_code::usage::tests::real_claude_home_subagent_rollup -- --ignored --exact --nocapture`
    #[test]
    #[ignore]
    fn real_claude_home_subagent_rollup() {
        use crate::adapters::claude_code::paths;

        let projects = paths::projects_dir(&paths::default_claude_home());
        let project_dirs: Vec<PathBuf> = fs::read_dir(&projects)
            .expect("a real ~/.claude/projects")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        let (main_refs, dirs) = transcript_refs_by_recency(&project_dirs);
        let sub_refs = subagent_transcript_refs(&project_dirs);

        // How many real sub-agent files the gate actually admits, and how many
        // carry the meta linkage at all -- the two populations issue #19 rests on.
        let below_gate = sub_refs
            .iter()
            .filter(|r| fs::read_to_string(&r.path).map(|c| file_is_below_gate(&c)).unwrap_or(false))
            .count();
        let linked = sub_refs.iter().filter(|r| spawn_tool_use_id(&r.path).is_some()).count();
        eprintln!("\n=== issue #19 against the real ~/.claude ===");
        eprintln!("  project dirs      {}", project_dirs.len());
        eprintln!("  main transcripts  {}", main_refs.len());
        eprintln!("  sub-agent files   {}", sub_refs.len());
        eprintln!("  ... with a meta toolUseId linkage  {linked}");
        eprintln!("  ... below the attribution gate     {below_gate}");

        // Totals with sub-agents included, computed through the full pass. The
        // roll-up runs here; on a current-build corpus it must contribute
        // nothing, so these are exactly the native-first totals of issue #13.
        let cache = SqliteUsageCache::open_in_memory().unwrap();
        refresh_usage(&main_refs, &sub_refs, &dirs, &cache);
        let rolled: Vec<UsageTotal> =
            cache.totals(true).into_iter().filter(|t| t.reconstructed).collect();

        assert_eq!(
            below_gate, 0,
            "no real transcript is pre-attribution, so the roll-up must be inert on this corpus",
        );
        assert!(
            rolled.is_empty(),
            "the roll-up fabricated {} reconstructed credit(s) on a current-build corpus: {:?}",
            rolled.len(),
            rolled.iter().map(|t| &t.attribution_skill).collect::<Vec<_>>(),
        );
        eprintln!("  reconstructed credits (must be 0)  {}", rolled.len());
    }

    #[test]
    fn the_roll_up_never_runs_on_main_thread_transcripts() {
        // The roll-up is the sub-agent pass's job alone. A main-thread file that
        // happens to sit next to a meta sidecar must never be rolled up.
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_session(
            tmp.path(),
            &[skill_invoke_line(OLD, "grilling"), spawn_line(OLD, "Task", "toolu_1", None)],
            &[unattributed_subagent_line(OLD, "m_sub", 999)],
            Some("toolu_1"),
        );

        let cache = SqliteUsageCache::open_in_memory().unwrap();
        // Main-thread pass only: the parent's own turns reconstruct via #12,
        // but the sub-agent file is never enumerated, so its 999 never lands.
        let (main_refs, dirs) = transcript_refs_by_recency(std::slice::from_ref(&dir));
        refresh_usage(&main_refs, &[], &dirs, &cache);

        let work = UsageIndex::build(&cache, true).for_skill(&grilling()).map(|u| u.work).unwrap_or(0);
        assert_eq!(work, 0, "the parent's spawn turn carries 0 work, and no sub-agent file was scanned");
    }
}
