use crate::domain::footprint::Footprint;
use crate::domain::report::{ScanReport, SkillReport};
use crate::domain::skill::{DiscoveredSkill, DiscoveryResult};

/// Abstracts everything agent-specific: where skills live, how footprint is
/// read, how enable/disable is mutated (mutation methods land in a later
/// plan). v1 ships a single implementation, `ClaudeCodeAdapter` (ADR 0002).
pub trait HarnessAdapter {
    fn discover_skills(&self) -> DiscoveryResult;
    fn compute_footprint(&self, skill: &DiscoveredSkill) -> Footprint;

    /// Whether a user-supplied API key is configured, so `scan_all` can report
    /// it in the `ScanReport` and the panel shows the right settings state
    /// (issue #4). Defaults to `false`; a harness that supports exact counts
    /// overrides it. Never exposes the key itself, only its presence.
    fn api_key_present(&self) -> bool {
        false
    }

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
            api_key_present: self.api_key_present(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubAdapter;
    impl HarnessAdapter for StubAdapter {
        fn discover_skills(&self) -> DiscoveryResult {
            DiscoveryResult { skills: vec![], warnings: vec![], active_repo_path: None }
        }
        fn compute_footprint(&self, _skill: &DiscoveredSkill) -> Footprint {
            unreachable!("no skills to compute in this stub")
        }
    }

    #[test]
    fn default_scan_all_reports_no_api_key_until_an_adapter_overrides() {
        assert!(!StubAdapter.api_key_present(), "the trait default must claim no key");
        assert!(!StubAdapter.scan_all().api_key_present, "the default scan_all must wire the field");
    }
}
