use crate::domain::footprint::Footprint;
use crate::domain::skill::{DiscoveredSkill, DiscoveryResult};

/// Abstracts everything agent-specific: where skills live, how footprint is
/// read, how enable/disable is mutated (mutation methods land in a later
/// plan). v1 ships a single implementation, `ClaudeCodeAdapter` (ADR 0002).
pub trait HarnessAdapter {
    fn discover_skills(&self) -> DiscoveryResult;
    fn compute_footprint(&self, skill: &DiscoveredSkill) -> Footprint;
}
