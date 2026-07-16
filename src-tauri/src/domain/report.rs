use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::footprint::{AlwaysOnTextKind, Footprint, TokenSource};
use super::removal::{Retention, Tombstone, TrashUnit};
use super::skill::{DiscoveredSkill, SkillId};

/// One footprint layer, flattened for the IPC boundary. `exact` collapses
/// `TokenSource` to a bool because the UI only ever asks "is this the exact
/// tier or the estimate tier" (ADR 0006); the reference model that produced
/// an exact count is never surfaced (ADR 0018), so it isn't here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerReport {
    pub tokens: u32,
    pub exact: bool,
}

impl From<crate::domain::footprint::LayerCount> for LayerReport {
    fn from(count: crate::domain::footprint::LayerCount) -> Self {
        LayerReport { tokens: count.tokens, exact: count.source == TokenSource::Exact }
    }
}

/// One skill's identity at the IPC boundary: the domain's `SkillId` (CONTEXT.md
/// "Skill identity"), mirrored here rather than derived on `SkillId` itself so
/// the domain type stays free of the wire format, like `LayerReport` above.
///
/// It replaces the flat `(kind, name, repo_path, marketplace, plugin)` tuple the
/// panel used to reassemble -- all five were projections of this one value -- so
/// a row carries its identity as one thing it can hand back. Tagging it keeps a
/// plugin row's `marketplace` a `string` rather than a nullable the other two
/// kinds leave empty.
///
/// **A ref names a row; it is never a path to act on.** That is a narrower claim
/// than issue #27's framing, and deliberately so: a ref does not close the
/// window between the scan the panel rendered and the mutation that follows, and
/// no id can -- the filesystem moves underneath either one. What it buys is that
/// a stale ref *fails* instead of misfiring. A mutation resolves it against a
/// fresh scan and acts on the `DiscoveredSkill` it finds (ADR 0027), so
/// `repo_path` is part of the key and never an operand: a ref that no longer
/// matches aims no delete at all, rather than aiming one at the wrong directory.
///
/// The round trip is lossless. Every component reaches the domain as UTF-8
/// already -- discovery skips a skill whose directory name is not valid UTF-8
/// (`discovery/scan.rs`), `marketplace`/`plugin` are JSON object keys, and
/// `repo_path` is built from a transcript's JSON `cwd` string (ADR 0014) -- so
/// `display()` here has nothing to replace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum SkillRef {
    Personal { name: String },
    Project { repo_path: String, name: String },
    Plugin { marketplace: String, plugin: String, name: String },
}

impl SkillRef {
    /// The directory name -- the identity Claude Code renders and the user
    /// recognizes, not the frontmatter `name:` (ADR 0016). Every variant carries
    /// one, so callers need no match on the kind.
    ///
    /// Rust-side this is exercised only by the test suite, since the panel reads
    /// the field straight off the JSON; `allow(dead_code)` keeps it without
    /// masking a real regression, as on `HarnessAdapter`.
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        match self {
            SkillRef::Personal { name } => name,
            SkillRef::Project { name, .. } => name,
            SkillRef::Plugin { name, .. } => name,
        }
    }
}

impl From<&SkillId> for SkillRef {
    fn from(id: &SkillId) -> Self {
        match id {
            SkillId::Personal { name } => SkillRef::Personal { name: name.clone() },
            SkillId::Project { repo_path, name } => {
                SkillRef::Project { repo_path: repo_path.display().to_string(), name: name.clone() }
            }
            SkillId::Plugin { marketplace, plugin, name } => SkillRef::Plugin {
                marketplace: marketplace.clone(),
                plugin: plugin.clone(),
                name: name.clone(),
            },
        }
    }
}

/// The way back in, for the mutation commands that take a ref as a parameter
/// (issue #31). Infallible: a ref carries no state the domain has to validate,
/// and whether it names a skill that still exists is a lookup against a fresh
/// scan, not a parse.
impl From<SkillRef> for SkillId {
    fn from(reference: SkillRef) -> Self {
        match reference {
            SkillRef::Personal { name } => SkillId::Personal { name },
            SkillRef::Project { repo_path, name } => {
                SkillId::Project { repo_path: PathBuf::from(repo_path), name }
            }
            SkillRef::Plugin { marketplace, plugin, name } => {
                SkillId::Plugin { marketplace, plugin, name }
            }
        }
    }
}

/// How a usage figure was attributed (ADR 0005). `Native` trusts Claude Code's
/// own `attributionSkill`/`attributionPlugin` (issue #5); `Reconstructed` is a
/// version-gated in-order walk over a pre-attribution transcript that credited
/// each turn to the skill then holding the wheel (issue #12). A skill is tagged
/// `Reconstructed` if ANY of its contributing turns was reconstructed, so the
/// lower-confidence framing is sticky and never overstated (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AttributionSource {
    Native,
    Reconstructed,
}

/// Attributed session usage for one skill (ADR 0005): a demoted, deliberately
/// fuzzy proxy, never blended with the exact footprint (ADR 0003). `work` is
/// input + output tokens (the headline); `cache_write` and `cache_read` are
/// separate buckets, and `cache_read` is never folded into `work` (it
/// dominates 10-100x). These are tokens spent *during* a skill, not *by* it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    // u64, not u32: these are cumulative all-time sums (ADR 0024), and cache_read
    // dominates 10-100x, so a heavy top skill's total can exceed u32::MAX and
    // must never silently wrap.
    pub work: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub attribution_source: AttributionSource,
}

/// One row the tray panel renders: a skill's identity, liveness, and its
/// three footprint layers. Harness-neutral (ADR 0002) and serializable for
/// the Tauri IPC boundary. Deliberately thin -- field-level shaping for the
/// panel (badges, sort keys, `~` labels) is the UI work's concern; this is
/// only the honest data it renders from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillReport {
    /// The row's identity, and the handle the panel hands back to name it in a
    /// mutation (issue #31). Carries the directory name, and the repo or
    /// marketplace/plugin that qualifies it.
    pub id: SkillRef,
    pub live: bool,
    pub always_on: LayerReport,
    /// Where the always-on text came from (ADR 0016). Only the always-on layer
    /// carries this. `notListed` means the skill is kept out of the listing by
    /// `disable-model-invocation`, so `always_on.tokens` is a certain zero
    /// rather than a low-confidence guess (issue #24).
    pub always_on_text: AlwaysOnTextKind,
    pub on_invoke: LayerReport,
    /// `None` (a JSON `null`) means the on-demand ceiling is still being
    /// computed off the interactive scan (issue #11); the panel renders a
    /// pending affordance, never a `0`. `Some(LayerReport { tokens: 0, .. })`
    /// is the resolved "no bundled files" state, kept distinct from pending.
    pub on_demand: Option<LayerReport>,
    /// Attributed session usage (issue #5), or `None` when no session has
    /// touched this skill. `None` (a null in the panel) means "untouched," not
    /// "attributed zero," so a zero figure is never fabricated for a skill no
    /// session used.
    pub usage: Option<UsageReport>,
    /// The frontmatter `name:` -- the label the model is shown. When it diverges
    /// from the directory name in `id`, the panel surfaces both rather than
    /// silently picking one (CONTEXT.md "Declared name"), so both cross.
    pub declared_name: String,
    /// Whether `declared_name` diverges from the directory name. Computed here
    /// rather than left to the panel because what counts as divergence is the
    /// domain's rule, not a rendering one.
    pub name_mismatch: bool,
    /// The directory owning this skill's real content, or `None` when the skill
    /// owns it itself (ADR 0026). A path, deliberately: no basename rule turns
    /// one into a product name (`gstack` reads well, `skills` -- from
    /// `~/.agents/skills` -- says nothing), and skillmon reads no managing
    /// tool's manifest to do better.
    ///
    /// `None` means unmanaged, which is **not** the same as safe to remove: the
    /// row other skills resolve into is itself unmanaged. Read it with
    /// `provides_for`, never alone (ADR 0026).
    pub manager_root: Option<String>,
    /// How many discovered skills resolve into this one's directory (CONTEXT.md
    /// "Dependent skill"). The other half of the pair `manager_root` must never
    /// be collapsed into: unmanaged **and** `provides_for: 46` is the single
    /// most destructive row on a real machine, and either fact alone describes
    /// it as harmless (ADR 0026).
    ///
    /// A **floor**, never a total: only discovered skills are counted, and
    /// skillmon scans Claude Code's paths alone (ADR 0027). The panel must not
    /// present it as exhaustive.
    pub provides_for: u32,
}

impl SkillReport {
    pub fn from_parts(
        skill: &DiscoveredSkill,
        footprint: &Footprint,
        usage: Option<UsageReport>,
        provides_for: u32,
    ) -> Self {
        SkillReport {
            id: SkillRef::from(&skill.id),
            live: skill.live,
            always_on: footprint.always_on.count.into(),
            always_on_text: footprint.always_on.text_kind,
            on_invoke: footprint.on_invoke.into(),
            on_demand: footprint.on_demand.map(Into::into),
            usage,
            declared_name: skill.frontmatter.declared_name.clone(),
            name_mismatch: skill.name_mismatch(),
            manager_root: skill.manager_root.as_ref().map(|p| p.display().to_string()),
            provides_for,
        }
    }
}

/// The full result of a scan: every discovered skill with its footprint,
/// plus the non-fatal warnings discovery collected and the active repo (so
/// the UI can label which repo's project skills are counted as co-resident,
/// DESIGN.md UX decision #5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanReport {
    pub skills: Vec<SkillReport>,
    pub warnings: Vec<String>,
    pub active_repo_path: Option<String>,
    /// Whether an API key is configured, so the panel shows the right settings
    /// state and, since the badges already reflect exact-vs-estimate, the whole
    /// key-presence UI flips from one `list_skills` payload (issue #4). Only a
    /// boolean crosses the IPC boundary; the key itself never does.
    pub api_key_present: bool,
    /// Which window the per-skill `usage` figures cover: `None` = all-time
    /// (issue #5's shipped cumulative numbers, the default view), `Some(24)` =
    /// the last 24h. Lets the panel label the usage sub-line honestly (issue
    /// #14). The 24h budget toast is independent of this and always 24h.
    pub usage_window_hours: Option<u32>,
}

/// One row of the removed view (ADR 0029): a staged removal the user can undo
/// or reclaim.
///
/// Carries no paths. A trash unit is named by its id, for the reason `SkillRef`
/// gives: the panel's handle on a mutation should be a name, never an operand,
/// so a stale one aims no delete at all rather than aiming one at a stray path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashUnitReport {
    pub id: i64,
    /// `disabled` never appears in the removed view's purge affordances --
    /// it is retained indefinitely, and `empty_trash` skips it (ADR 0027).
    pub retention: Retention,
    /// Unix epoch millis. The panel subtracts "now" to render an age; the core
    /// holds no wall clock (issue #14).
    pub removed_at_millis: i64,
    /// The row the user acted on, so the panel can label the unit and, once the
    /// skill is reinstalled, line it back up with its history.
    pub primary: SkillRef,
    /// The primary's frontmatter `name:`, kept because the files are gone: after
    /// a purge there is no `SKILL.md` left to read a label out of.
    pub declared_name: String,
    pub entry_count: u32,
    /// Whether this was a tool uninstall rather than a skill removal (ADR 0027),
    /// which is what the "47 entries" framing hangs off.
    pub tool_uninstall: bool,
    /// What purging this reclaims -- the number that makes explicit purge work
    /// instead of a timer (ADR 0029).
    ///
    /// A **floor**, never the managing tool's total disk cost: skillmon scans
    /// only Claude Code's paths, so a tool's entries for other agents are
    /// neither cascaded nor counted. The view must not claim otherwise.
    pub bytes: u64,
}

impl From<&TrashUnit> for TrashUnitReport {
    fn from(unit: &TrashUnit) -> Self {
        TrashUnitReport {
            id: unit.id.0,
            retention: unit.retention,
            removed_at_millis: unit.removed_at_millis,
            primary: SkillRef::from(&unit.primary.skill_id),
            declared_name: unit.primary.declared_name.clone(),
            entry_count: unit.entry_count() as u32,
            tool_uninstall: unit.is_tool_uninstall(),
            bytes: unit.bytes(),
        }
    }
}

/// One "(removed)" row of the removed view (DESIGN.md UX #6).
///
/// A tombstone outlives the trash unit that produced it, so this is the *only*
/// handle the panel has on a skill whose bytes have been reclaimed. Its `id` is
/// what lines the row back up with its retained usage history, and what a
/// reinstall matches on to restore continuity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TombstoneReport {
    pub id: SkillRef,
    /// The frontmatter `name:` as of removal. Frozen deliberately: there is no
    /// `SKILL.md` left to re-read, so this is the last thing that knows what the
    /// user called it.
    pub declared_name: String,
    pub removed_at_millis: i64,
}

impl From<&Tombstone> for TombstoneReport {
    fn from(tombstone: &Tombstone) -> Self {
        TombstoneReport {
            id: SkillRef::from(&tombstone.skill_id),
            declared_name: tombstone.declared_name.clone(),
            removed_at_millis: tombstone.removed_at_millis,
        }
    }
}

/// What an `empty_trash` actually reclaimed, so the panel reports the real
/// figure rather than echoing back the one it offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeSummary {
    pub units: u32,
    pub bytes: u64,
    /// Units that could not be reclaimed and are still staged. Carried rather
    /// than folded into an error, because a sweep that freed 1.1 GB and failed
    /// on one tree did not fail -- but it did not fully succeed either, and the
    /// panel must be able to say so instead of claiming a clean sweep.
    pub failed: u32,
}

/// The user-configurable usage-toast settings (issue #14), round-tripped by the
/// `get_usage_settings` / `set_usage_settings` commands. Deserialized on the way
/// in, so it derives `Deserialize` unlike the other read-only report DTOs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSettings {
    /// The rolling-24h attributed-work budget toast, on by default.
    pub budget_enabled: bool,
    /// The attributed work-token ceiling per 24h.
    pub budget_work_tokens: u64,
    /// Per-skill anomaly toasts, off by default (DESIGN.md UX #4).
    pub anomaly_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::footprint::{AlwaysOnFootprint, LayerCount};
    use crate::domain::skill::{Frontmatter, SkillId};
    use std::path::PathBuf;

    fn skill_with_id(id: SkillId) -> DiscoveredSkill {
        let declared_name = id.name().to_string();
        DiscoveredSkill {
            id,
            dir_path: PathBuf::from("/tmp/x"),
            canonical_dir: PathBuf::from("/tmp/x"),
            skill_md_path: PathBuf::from("/tmp/x/SKILL.md"),
            frontmatter: Frontmatter {
                // Matches the directory name, so the default fixture is an
                // ordinary skill and only the mismatch tests opt into divergence.
                declared_name,
                description: "d".to_string(),
                raw_block: "name: x\ndescription: d".to_string(),
                model_invocable: true,
            },
            body: "body".to_string(),
            manager_root: None,
            on_demand_files: vec![],
            live: true,
        }
    }

    fn sample_footprint() -> Footprint {
        Footprint {
            always_on: AlwaysOnFootprint {
                count: LayerCount { tokens: 10, source: TokenSource::Exact },
                text_kind: AlwaysOnTextKind::Native,
            },
            on_invoke: LayerCount { tokens: 200, source: TokenSource::Estimate },
            on_demand: Some(LayerCount { tokens: 0, source: TokenSource::Exact }),
        }
    }

    #[test]
    fn personal_skill_report_carries_only_a_name_as_its_identity() {
        let skill = skill_with_id(SkillId::Personal { name: "grilling".to_string() });
        let report = SkillReport::from_parts(&skill, &sample_footprint(), None, 0);

        assert_eq!(report.id, SkillRef::Personal { name: "grilling".to_string() });
        assert_eq!(report.id.name(), "grilling");
        assert!(report.always_on.exact);
        assert_eq!(report.always_on_text, AlwaysOnTextKind::Native);
        assert!(!report.on_invoke.exact);
    }

    #[test]
    fn plugin_skill_report_carries_marketplace_and_plugin() {
        let skill = skill_with_id(SkillId::Plugin {
            marketplace: "official".to_string(),
            plugin: "superpowers".to_string(),
            name: "brainstorming".to_string(),
        });
        let report = SkillReport::from_parts(&skill, &sample_footprint(), None, 0);

        assert_eq!(
            report.id,
            SkillRef::Plugin {
                marketplace: "official".to_string(),
                plugin: "superpowers".to_string(),
                name: "brainstorming".to_string(),
            }
        );
    }

    #[test]
    fn project_skill_report_carries_repo_path() {
        let skill = skill_with_id(SkillId::Project {
            repo_path: PathBuf::from("/home/me/repo"),
            name: "deploy".to_string(),
        });
        let report = SkillReport::from_parts(&skill, &sample_footprint(), None, 0);

        assert_eq!(
            report.id,
            SkillRef::Project { repo_path: "/home/me/repo".to_string(), name: "deploy".to_string() }
        );
    }

    /// The identity contract issue #27 turns on: a ref the panel holds and hands
    /// back must name the same skill it was minted from. A mutation resolves the
    /// ref against a fresh scan by comparing `SkillId`s (ADR 0027), so anything
    /// lost here would silently fail to match -- or, worse, match a sibling.
    #[test]
    fn a_skill_ref_round_trips_through_json_back_to_the_same_skill_id() {
        let ids = [
            SkillId::Personal { name: "grilling".to_string() },
            SkillId::Project { repo_path: PathBuf::from("/home/me/repo"), name: "deploy".to_string() },
            SkillId::Plugin {
                marketplace: "official".to_string(),
                plugin: "superpowers".to_string(),
                name: "brainstorming".to_string(),
            },
        ];

        for id in ids {
            let json = serde_json::to_string(&SkillRef::from(&id)).unwrap();
            let parsed: SkillRef = serde_json::from_str(&json).unwrap();
            assert_eq!(SkillId::from(parsed), id, "ref did not round-trip: {json}");
        }
    }

    /// Two skills sharing a directory name are distinct rows, so the qualifying
    /// fields have to be part of the wire identity, not just the Rust one.
    #[test]
    fn same_named_skills_from_different_marketplaces_are_distinct_refs() {
        let a = SkillRef::from(&SkillId::Plugin {
            marketplace: "official".to_string(),
            plugin: "superpowers".to_string(),
            name: "brainstorming".to_string(),
        });
        let b = SkillRef::from(&SkillId::Plugin {
            marketplace: "community".to_string(),
            plugin: "superpowers".to_string(),
            name: "brainstorming".to_string(),
        });

        assert_ne!(a, b);
        assert_ne!(serde_json::to_value(&a).unwrap(), serde_json::to_value(&b).unwrap());
    }

    #[test]
    fn skill_ref_serializes_tagged_and_camel_cased() {
        let personal = SkillRef::from(&SkillId::Personal { name: "grilling".to_string() });
        let json = serde_json::to_value(&personal).unwrap();
        assert_eq!(json["kind"], "personal");
        assert_eq!(json["name"], "grilling");
        // A personal row carries no marketplace key at all, rather than a null
        // the panel would have to narrow away.
        assert!(json.get("marketplace").is_none());

        let project = SkillRef::from(&SkillId::Project {
            repo_path: PathBuf::from("/home/me/repo"),
            name: "deploy".to_string(),
        });
        let json = serde_json::to_value(&project).unwrap();
        assert_eq!(json["kind"], "project");
        assert_eq!(json["repoPath"], "/home/me/repo");

        let plugin = SkillRef::from(&SkillId::Plugin {
            marketplace: "official".to_string(),
            plugin: "superpowers".to_string(),
            name: "brainstorming".to_string(),
        });
        let json = serde_json::to_value(&plugin).unwrap();
        assert_eq!(json["kind"], "plugin");
        assert_eq!(json["marketplace"], "official");
        assert_eq!(json["plugin"], "superpowers");
    }

    #[test]
    fn manager_root_crosses_as_a_path_and_is_null_when_unmanaged() {
        let unmanaged = skill_with_id(SkillId::Personal { name: "vercel-react".to_string() });
        let report = SkillReport::from_parts(&unmanaged, &sample_footprint(), None, 0);
        assert_eq!(report.manager_root, None);
        assert_eq!(serde_json::to_value(&report).unwrap()["managerRoot"], serde_json::Value::Null);

        // gstack's dominant shape: a real dir whose SKILL.md links into the
        // checkout (issue #25). The panel shows the path, never an invented
        // product name (ADR 0026).
        let managed = DiscoveredSkill {
            manager_root: Some(PathBuf::from("/home/me/.claude/skills/gstack")),
            ..skill_with_id(SkillId::Personal { name: "ship".to_string() })
        };
        let report = SkillReport::from_parts(&managed, &sample_footprint(), None, 0);
        assert_eq!(report.manager_root.as_deref(), Some("/home/me/.claude/skills/gstack"));
        assert_eq!(
            serde_json::to_value(&report).unwrap()["managerRoot"],
            "/home/me/.claude/skills/gstack"
        );
    }

    /// The pair ADR 0026 refuses to collapse, on the row where collapsing it is
    /// dangerous: gstack is unmanaged *and* the thing 46 skills resolve into, so
    /// a panel reading `managerRoot: null` alone would call the most destructive
    /// entry on disk safe to delete.
    #[test]
    fn provides_for_crosses_beside_manager_root_and_is_zero_for_an_ordinary_skill() {
        let ordinary = skill_with_id(SkillId::Personal { name: "vercel-react".to_string() });
        let report = SkillReport::from_parts(&ordinary, &sample_footprint(), None, 0);
        assert_eq!(serde_json::to_value(&report).unwrap()["providesFor"], 0);

        let checkout = skill_with_id(SkillId::Personal { name: "gstack".to_string() });
        let report = SkillReport::from_parts(&checkout, &sample_footprint(), None, 46);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["providesFor"], 46);
        assert_eq!(json["managerRoot"], serde_json::Value::Null);
    }

    /// The real divergence on the reference machine: a directory named
    /// `connect-chrome` whose frontmatter declares `open-gstack-browser`. Both
    /// names cross so the panel can show both (CONTEXT.md "Declared name").
    #[test]
    fn a_declared_name_that_diverges_from_the_directory_crosses_flagged() {
        let mut skill = skill_with_id(SkillId::Personal { name: "connect-chrome".to_string() });
        skill.frontmatter.declared_name = "open-gstack-browser".to_string();
        let report = SkillReport::from_parts(&skill, &sample_footprint(), None, 0);

        assert_eq!(report.id.name(), "connect-chrome");
        assert_eq!(report.declared_name, "open-gstack-browser");
        assert!(report.name_mismatch);

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["declaredName"], "open-gstack-browser");
        assert_eq!(json["nameMismatch"], true);
    }

    #[test]
    fn an_agreeing_declared_name_is_not_flagged_as_a_mismatch() {
        let skill = skill_with_id(SkillId::Personal { name: "grilling".to_string() });
        let report = SkillReport::from_parts(&skill, &sample_footprint(), None, 0);

        assert_eq!(report.declared_name, "grilling");
        assert!(!report.name_mismatch);
    }

    #[test]
    fn usage_none_serializes_null_and_some_serializes_camel_case() {
        let skill = skill_with_id(SkillId::Personal { name: "grilling".to_string() });

        let without = SkillReport::from_parts(&skill, &sample_footprint(), None, 0);
        assert_eq!(serde_json::to_value(&without).unwrap()["usage"], serde_json::Value::Null);

        let usage = UsageReport {
            work: 1229,
            cache_write: 13781,
            cache_read: 35154,
            attribution_source: AttributionSource::Native,
        };
        let with = SkillReport::from_parts(&skill, &sample_footprint(), Some(usage), 0);
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["usage"]["work"], 1229);
        assert_eq!(json["usage"]["cacheWrite"], 13781);
        assert_eq!(json["usage"]["cacheRead"], 35154);
        assert_eq!(json["usage"]["attributionSource"], "native");

        // The reconstructed seam (issue #12) serializes to its own camelCase
        // tag, so the UI can flag the lower-confidence figure (ADR 0005).
        let reconstructed = UsageReport {
            work: 500,
            cache_write: 0,
            cache_read: 0,
            attribution_source: AttributionSource::Reconstructed,
        };
        let with_recon = SkillReport::from_parts(&skill, &sample_footprint(), Some(reconstructed), 0);
        let recon_json = serde_json::to_value(&with_recon).unwrap();
        assert_eq!(recon_json["usage"]["attributionSource"], "reconstructed");
    }

    #[test]
    fn a_pending_on_demand_serializes_to_json_null_not_zero() {
        // The pending contract at the IPC boundary (issue #11): a `None`
        // on-demand must reach the panel as `null` so it can render the
        // "computing…" affordance, never as a `0` that would read as an exact
        // "nothing to load" ceiling.
        let skill = skill_with_id(SkillId::Personal { name: "grilling".to_string() });
        let footprint = Footprint {
            always_on: AlwaysOnFootprint {
                count: LayerCount { tokens: 10, source: TokenSource::Estimate },
                text_kind: AlwaysOnTextKind::Native,
            },
            on_invoke: LayerCount { tokens: 20, source: TokenSource::Estimate },
            on_demand: None,
        };
        let report = SkillReport::from_parts(&skill, &footprint, None, 0);
        let json = serde_json::to_value(&report).unwrap();

        assert_eq!(json["onDemand"], serde_json::Value::Null);
        assert!(json.get("onDemand").is_some(), "the key is present, just null");
    }

    #[test]
    fn scan_report_serializes_to_camel_case_json() {
        let report = ScanReport {
            skills: vec![],
            warnings: vec!["a warning".to_string()],
            active_repo_path: Some("/repo".to_string()),
            api_key_present: true,
            usage_window_hours: None,
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["activeRepoPath"], "/repo");
        assert_eq!(json["warnings"][0], "a warning");
        assert_eq!(json["apiKeyPresent"], true);
    }

    #[test]
    fn scan_report_serializes_usage_window_hours() {
        let all_time = ScanReport {
            skills: vec![],
            warnings: vec![],
            active_repo_path: None,
            api_key_present: false,
            usage_window_hours: None,
        };
        // All-time serializes as an explicit null, not an omitted key, so the
        // panel can distinguish "all-time" from a malformed payload.
        assert_eq!(serde_json::to_value(&all_time).unwrap()["usageWindowHours"], serde_json::Value::Null);

        let windowed = ScanReport { usage_window_hours: Some(24), ..all_time };
        assert_eq!(serde_json::to_value(&windowed).unwrap()["usageWindowHours"], 24);
    }

    #[test]
    fn usage_settings_round_trips_camel_case() {
        let settings = UsageSettings { budget_enabled: true, budget_work_tokens: 250_000, anomaly_enabled: false };
        let json = serde_json::to_value(settings).unwrap();
        assert_eq!(json["budgetEnabled"], true);
        assert_eq!(json["budgetWorkTokens"], 250_000);
        assert_eq!(json["anomalyEnabled"], false);
        // The set command deserializes the same shape back.
        let back: UsageSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back, settings);
    }
}
