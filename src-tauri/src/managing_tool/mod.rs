//! What a managing tool can and cannot make stick (ADR 0027).
//!
//! A seam **parallel to `HarnessAdapter`, not inside it**. Discovery rightly
//! lives in `adapters/claude_code/`, because where an entry points is a Claude
//! Code fact. But `~/.agents` serves 14 agents, and gstack installs into Codex,
//! Factory, and OpenCode paths too (`setup:585-663`) -- so what a tool can
//! remove is not a fact about Claude Code, and a second harness would inherit a
//! copy-paste of it.
//!
//! The boundary with ADR 0026 is worth stating, because the two look like they
//! contradict each other. *Detection* of a manager root is structural and
//! tool-agnostic, and stays that way: it has to answer for tools nobody has
//! heard of. *Removal* is where tool-specific knowledge is allowed, because it
//! does not -- an unknown tool simply gets entry-only removal, honestly labeled.
//!
//! Nothing here removes anything. A source is staged into its skill's trash
//! entry by `removal::remove`, so that one unit still means one undo (ADR 0029);
//! a tool's job is to say whether that removal can be made to stick, and to do
//! the bookkeeping that makes it -- see `forget_source`.

pub mod agents;
pub mod gstack;

use std::io;
use std::path::{Path, PathBuf};

use crate::domain::skill::DiscoveredSkill;

/// A failure to read or rewrite a managing tool's own state file.
///
/// Separate from `io::Error` because the interesting cases are not I/O: a lock
/// file in a version skillmon does not understand is a refusal, not a fault, and
/// it has to reach the user as one. Editing a third-party tool's state on a
/// guess about its shape is exactly the overreach ADR 0027 exists to prevent.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error(
        "{tool} keeps its state at {path} in a format this build does not understand \
         (found version {found}, expected {supported}); removing the source could corrupt it"
    )]
    UnsupportedState { tool: &'static str, path: PathBuf, found: String, supported: u32 },
    #[error("{path} is not shaped like the state file {tool} writes ({reason}); leaving it alone")]
    MalformedState { tool: &'static str, path: PathBuf, reason: String },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A tool that installs skills into a harness's scan root and can put them back.
///
/// Implementations are stateless over the filesystem: they hold the paths they
/// own, and read/write only their own state file.
pub trait ManagingTool {
    /// A stable label, for the reason string the panel shows. Not a display
    /// name derived from a path -- ADR 0026 refuses those, and rightly:
    /// `~/.agents/skills` basenames to "skills", which says nothing.
    fn name(&self) -> &'static str;

    /// Whether this tool owns `root` -- a `manager_root` as discovery derived
    /// it (ADR 0026), already canonical.
    fn detects(&self, root: &Path) -> bool;

    /// `None` = this tool can make a source removal stick. `Some(reason)` = it
    /// cannot, and this is why.
    ///
    /// A **reason, never a bare `false`**, and that is deliberate: the panel has
    /// to explain why an option is absent, or its absence reads as a bug.
    fn can_remove_source(&self) -> Option<&str>;

    /// The directory holding this skill's real content -- what a source removal
    /// stages into the unit.
    ///
    /// Defaults to where `SKILL.md` actually resolves, which is the same rule
    /// discovery derives `manager_root` from, one level down (`manager_root` is
    /// this directory's *parent*). A tool whose unit of installation is bigger
    /// than one directory overrides it.
    fn source_of(&self, skill: &DiscoveredSkill) -> Option<PathBuf> {
        resolved_content_dir(skill)
    }

    /// Forgets a skill in the tool's own bookkeeping, so a staged source
    /// removal is not undone the next time the tool runs.
    ///
    /// Returns the state it dropped, opaque to skillmon and stored verbatim on
    /// the trash entry, so `relearn_source` can put it back on restore.
    /// `Ok(None)` means there was nothing recorded to drop -- already-absent is
    /// success, not failure, since the caller's goal is the absence.
    ///
    /// Reversing this matters more than it looks: ADR 0027 rejected
    /// "delete the target by default" partly *because* it silently desyncs
    /// `.skill-lock.json`. A restore that left the lock pruned would commit the
    /// same sin one step later.
    fn forget_source(&self, skill: &DiscoveredSkill) -> Result<Option<String>, SourceError>;

    /// Puts back what `forget_source` dropped, from its own returned state.
    fn relearn_source(&self, state: &str) -> Result<(), SourceError>;
}

/// Where a skill's `SKILL.md` actually resolves to, as a directory.
///
/// One rule covers both shapes a managed skill takes, with no branch on where
/// the link sits (issue #25): a symlinked entry directory (`.agents`:
/// `tdd -> ~/.agents/skills/tdd`) and a real directory holding a symlinked
/// `SKILL.md` (gstack's shims, `setup:571`) both resolve out of the scan root.
pub fn resolved_content_dir(skill: &DiscoveredSkill) -> Option<PathBuf> {
    Some(std::fs::canonicalize(&skill.skill_md_path).ok()?.parent()?.to_path_buf())
}

/// The tools skillmon knows how to ask, in detection order.
///
/// A registry rather than a free `detect()` because the tools are not all
/// discoverable from a path alone: `.agents` reads an environment variable to
/// find its own lock file, so where that tool lives is a composition-root fact,
/// resolved once (`from_env`) and injected -- which is also what lets the tests
/// build one over a tempdir.
pub struct ManagingTools {
    tools: Vec<Box<dyn ManagingTool + Send + Sync>>,
}

impl ManagingTools {
    pub fn new(tools: Vec<Box<dyn ManagingTool + Send + Sync>>) -> Self {
        ManagingTools { tools }
    }

    /// The real machine's tools.
    pub fn from_env() -> Self {
        let mut tools: Vec<Box<dyn ManagingTool + Send + Sync>> = vec![Box::new(gstack::GstackTool)];
        if let Some(agents) = agents::AgentsTool::from_env() {
            tools.push(Box::new(agents));
        }
        ManagingTools::new(tools)
    }

    /// The tool owning `root`, or `None` for a manager skillmon does not know.
    ///
    /// `None` is a first-class answer, not a failure: an unknown tool gets
    /// entry-only removal, honestly labeled (ADR 0027). It is also the *safe*
    /// answer -- an unrecognized manager is never offered a source removal, so a
    /// misdetection can only under-offer.
    pub fn for_root(&self, root: &Path) -> Option<&(dyn ManagingTool + Send + Sync)> {
        self.tools.iter().map(|t| t.as_ref()).find(|t| t.detects(root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct FakeTool {
        root: PathBuf,
    }

    impl ManagingTool for FakeTool {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn detects(&self, root: &Path) -> bool {
            root == self.root
        }
        fn can_remove_source(&self) -> Option<&str> {
            None
        }
        fn forget_source(&self, _skill: &DiscoveredSkill) -> Result<Option<String>, SourceError> {
            Ok(None)
        }
        fn relearn_source(&self, _state: &str) -> Result<(), SourceError> {
            Ok(())
        }
    }

    #[test]
    fn a_registry_resolves_a_manager_root_to_the_tool_that_owns_it() {
        let tools = ManagingTools::new(vec![Box::new(FakeTool { root: PathBuf::from("/home/me/.agents/skills") })]);

        assert_eq!(tools.for_root(Path::new("/home/me/.agents/skills")).map(|t| t.name()), Some("fake"));
    }

    /// An unknown manager is a first-class answer (entry-only, honestly
    /// labeled), not an error -- and it is the safe direction, since a
    /// misdetection can only withhold a source removal, never perform one.
    #[test]
    fn an_unknown_manager_root_resolves_to_no_tool() {
        let tools = ManagingTools::new(vec![Box::new(FakeTool { root: PathBuf::from("/home/me/.agents/skills") })]);

        assert!(tools.for_root(Path::new("/home/me/some-other-tool")).is_none());
    }
}
