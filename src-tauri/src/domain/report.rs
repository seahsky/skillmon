use serde::Serialize;

use super::footprint::{Footprint, TextConfidence, TokenSource};
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

/// The kind of skill, driving the UI's grouping and the identity fields that
/// are populated below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillKind {
    Personal,
    Project,
    Plugin,
}

/// One row the tray panel renders: a skill's identity, liveness, and its
/// three footprint layers. Harness-neutral (ADR 0002) and serializable for
/// the Tauri IPC boundary. Deliberately thin -- field-level shaping for the
/// panel (badges, sort keys, `~` labels) is the UI work's concern; this is
/// only the honest data it renders from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillReport {
    pub kind: SkillKind,
    /// The directory name -- the identity Claude Code actually renders and
    /// the user recognizes, not the frontmatter `name:` (ADR 0016).
    pub name: String,
    pub live: bool,
    pub always_on: LayerReport,
    /// `true` when always-on text came from a real transcript, `false` when
    /// reconstructed from frontmatter because no session has listed the
    /// skill yet (ADR 0016). Only the always-on layer carries this.
    pub always_on_native: bool,
    pub on_invoke: LayerReport,
    pub on_demand: LayerReport,
    /// Populated for `Project` skills: the repo the skill belongs to.
    pub repo_path: Option<String>,
    /// Populated for `Plugin` skills: the owning marketplace and plugin.
    pub marketplace: Option<String>,
    pub plugin: Option<String>,
}

impl SkillReport {
    pub fn from_parts(skill: &DiscoveredSkill, footprint: &Footprint) -> Self {
        let (kind, repo_path, marketplace, plugin) = match &skill.id {
            SkillId::Personal { .. } => (SkillKind::Personal, None, None, None),
            SkillId::Project { repo_path, .. } => {
                (SkillKind::Project, Some(repo_path.display().to_string()), None, None)
            }
            SkillId::Plugin { marketplace, plugin, .. } => {
                (SkillKind::Plugin, None, Some(marketplace.clone()), Some(plugin.clone()))
            }
        };

        SkillReport {
            kind,
            name: skill.directory_name().to_string(),
            live: skill.live,
            always_on: footprint.always_on.count.into(),
            always_on_native: footprint.always_on.confidence == TextConfidence::Native,
            on_invoke: footprint.on_invoke.into(),
            on_demand: footprint.on_demand.into(),
            repo_path,
            marketplace,
            plugin,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::footprint::{AlwaysOnFootprint, LayerCount};
    use crate::domain::skill::{Frontmatter, SkillId};
    use std::path::PathBuf;

    fn skill_with_id(id: SkillId) -> DiscoveredSkill {
        DiscoveredSkill {
            id,
            dir_path: PathBuf::from("/tmp/x"),
            skill_md_path: PathBuf::from("/tmp/x/SKILL.md"),
            frontmatter: Frontmatter {
                declared_name: "x".to_string(),
                description: "d".to_string(),
                raw_block: "name: x\ndescription: d".to_string(),
            },
            body: "body".to_string(),
            is_symlink: false,
            symlink_target: None,
            on_demand_files: vec![],
            live: true,
        }
    }

    fn sample_footprint() -> Footprint {
        Footprint {
            always_on: AlwaysOnFootprint {
                count: LayerCount { tokens: 10, source: TokenSource::Exact },
                confidence: TextConfidence::Native,
            },
            on_invoke: LayerCount { tokens: 200, source: TokenSource::Estimate },
            on_demand: LayerCount { tokens: 0, source: TokenSource::Exact },
        }
    }

    #[test]
    fn personal_skill_report_leaves_repo_and_plugin_identity_empty() {
        let skill = skill_with_id(SkillId::Personal { name: "grilling".to_string() });
        let report = SkillReport::from_parts(&skill, &sample_footprint());

        assert_eq!(report.kind, SkillKind::Personal);
        assert_eq!(report.name, "grilling");
        assert!(report.always_on.exact);
        assert!(report.always_on_native);
        assert!(!report.on_invoke.exact);
        assert_eq!(report.repo_path, None);
        assert_eq!(report.marketplace, None);
        assert_eq!(report.plugin, None);
    }

    #[test]
    fn plugin_skill_report_carries_marketplace_and_plugin() {
        let skill = skill_with_id(SkillId::Plugin {
            marketplace: "official".to_string(),
            plugin: "superpowers".to_string(),
            name: "brainstorming".to_string(),
        });
        let report = SkillReport::from_parts(&skill, &sample_footprint());

        assert_eq!(report.kind, SkillKind::Plugin);
        assert_eq!(report.marketplace.as_deref(), Some("official"));
        assert_eq!(report.plugin.as_deref(), Some("superpowers"));
        assert_eq!(report.repo_path, None);
    }

    #[test]
    fn project_skill_report_carries_repo_path() {
        let skill = skill_with_id(SkillId::Project {
            repo_path: PathBuf::from("/home/me/repo"),
            name: "deploy".to_string(),
        });
        let report = SkillReport::from_parts(&skill, &sample_footprint());

        assert_eq!(report.kind, SkillKind::Project);
        assert_eq!(report.repo_path.as_deref(), Some("/home/me/repo"));
    }

    #[test]
    fn scan_report_serializes_to_camel_case_json() {
        let report = ScanReport {
            skills: vec![],
            warnings: vec!["a warning".to_string()],
            active_repo_path: Some("/repo".to_string()),
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["activeRepoPath"], "/repo");
        assert_eq!(json["warnings"][0], "a warning");
    }
}
