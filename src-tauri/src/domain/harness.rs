use crate::domain::footprint::Footprint;
use crate::domain::report::{ScanReport, SkillReport};
use crate::domain::skill::{DiscoveredSkill, DiscoveryResult};

/// Abstracts everything agent-specific: where skills live, how footprint is
/// read, how enable/disable is mutated (mutation methods land in a later
/// plan). v1 ships a single implementation, `ClaudeCodeAdapter` (ADR 0002).
pub trait HarnessAdapter {
    fn discover_skills(&self) -> DiscoveryResult;
    fn compute_footprint(&self, skill: &DiscoveredSkill) -> Footprint;

    /// One full pass: discover every skill, compute each footprint, and
    /// flatten to the serializable `ScanReport` the IPC boundary returns.
    /// Lives here, not on the concrete adapter, because it is pure
    /// orchestration over the two methods above plus harness-neutral DTOs
    /// (ADR 0002) -- a second harness adapter inherits it unchanged. Footprint
    /// reuse is inherent: `compute_footprint` hashes each layer's text and the
    /// cache serves an unchanged hash without re-tokenizing, so an unchanged
    /// skill costs a lookup, not a recompute (ADR 0019).
    fn scan_all(&self) -> ScanReport {
        let discovery = self.discover_skills();
        let skills = discovery
            .skills
            .iter()
            .map(|skill| SkillReport::from_parts(skill, &self.compute_footprint(skill)))
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
        }
    }
}
