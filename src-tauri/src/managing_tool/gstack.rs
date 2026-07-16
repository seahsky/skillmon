//! gstack: detected, and incapable of a source removal that sticks (ADR 0027).
//!
//! Every claim here was read off the tool itself (`github.com/garrytan/gstack`,
//! public), not inferred from its output on one machine:
//!
//! - `setup`'s `link_claude_skill_dirs` (`setup:540-575`) loops over
//!   `"$gstack_dir"/*/`, `mkdir -p`s a real directory under `~/.claude/skills`,
//!   and links `"$gstack_dir/$dir_name/SKILL.md"` into it. So a shim is a real
//!   directory holding a symlinked `SKILL.md` -- the shape issue #25's detection
//!   missed -- and its `manager_root` resolves to the checkout root itself.
//! - The loop consults **no opt-out state**, and the only flags are
//!   `--prefix` / `--no-prefix`. There is no per-skill opt-out to defer to.
//! - `/gstack-upgrade` runs `git stash && git fetch && git reset --hard
//!   origin/main`, and every skill's `SKILL.md` is tracked -- so deleting content
//!   out of the checkout is not merely likely to revert, it is *guaranteed* to.
//!
//! Hence: entry removal only. Removing the gstack row itself is a different
//! thing entirely -- it has dependents, so it is a tool uninstall (ADR 0027),
//! and that is the only durable lever on those skills, since `/gstack-upgrade`
//! is itself a gstack skill.

use std::path::Path;

use super::{ManagingTool, SourceError};
use crate::domain::skill::DiscoveredSkill;

/// Stateless: gstack is recognized by the shape of the checkout it links out
/// of, so there is nothing to configure and nothing to resolve from the
/// environment. A checkout can sit anywhere (`--prefix` moves only the link
/// names, not the checkout), so a hardcoded path would be wrong; the marker
/// files travel with it.
pub struct GstackTool;

/// Files that sit at the root of a gstack checkout and, together, at no other
/// manager root skillmon is likely to meet. Verified against the public repo's
/// root listing rather than against one machine's install.
///
/// `SKILL.md` alone would match half the world; `setup` + `VERSION` beside it is
/// what makes the trio distinctive. A false positive costs nothing dangerous --
/// it withholds a source removal and explains why -- which is the direction to
/// err in.
const MARKERS: &[&str] = &["setup", "VERSION", "SKILL.md"];

/// The reason, phrased as the mechanism rather than a verdict, because the user
/// can check it: the tool really will `reset --hard` the file back.
const WHY_NOT: &str = "gstack rebuilds every skill it knows on each ./setup, and /gstack-upgrade runs \
                       git reset --hard, which restores any content deleted from its checkout. \
                       Removing the entry is durable until gstack is next run; removing gstack \
                       itself uninstalls the tool.";

impl ManagingTool for GstackTool {
    fn name(&self) -> &'static str {
        "gstack"
    }

    fn detects(&self, root: &Path) -> bool {
        MARKERS.iter().all(|marker| root.join(marker).is_file())
    }

    fn can_remove_source(&self) -> Option<&str> {
        Some(WHY_NOT)
    }

    /// Unreachable through the plan, which never offers a source removal a tool
    /// says it cannot make stick -- but stated here rather than left to that
    /// caller's discipline. A tool that answers `can_remove_source` with a
    /// reason must not also quietly do the thing.
    fn forget_source(&self, _skill: &DiscoveredSkill) -> Result<Option<String>, SourceError> {
        Ok(None)
    }

    fn relearn_source(&self, _state: &str) -> Result<(), SourceError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn checkout(markers: &[&str]) -> tempfile::TempDir {
        let tmp = tempdir().unwrap();
        for marker in markers {
            fs::write(tmp.path().join(marker), "x").unwrap();
        }
        tmp
    }

    /// The real checkout's shape: `setup`, `VERSION`, and `SKILL.md` at the
    /// root, as the public repo lists them.
    #[test]
    fn a_checkout_carrying_every_marker_is_gstack() {
        let tmp = checkout(MARKERS);
        assert!(GstackTool.detects(tmp.path()));
    }

    /// `SKILL.md` alone is every skill on the machine. Detection has to want all
    /// three, or gstack's "we cannot remove your source" would be told to
    /// everyone.
    #[test]
    fn a_directory_with_only_some_markers_is_not_gstack() {
        let tmp = checkout(&["SKILL.md"]);
        assert!(!GstackTool.detects(tmp.path()));

        let tmp = checkout(&["SKILL.md", "VERSION"]);
        assert!(!GstackTool.detects(tmp.path()));
    }

    /// `.agents`' manager root -- a bare directory of skill folders -- must not
    /// be claimed by gstack, or the capable tool would never be asked.
    #[test]
    fn an_unrelated_manager_root_is_not_gstack() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("tdd")).unwrap();
        assert!(!GstackTool.detects(tmp.path()));
    }

    /// A marker that is a *directory* is not a marker. `is_file()` rather than
    /// `exists()`: a skill named `setup` would otherwise help a random directory
    /// pass for a checkout.
    #[test]
    fn a_directory_named_like_a_marker_does_not_count() {
        let tmp = checkout(&["VERSION", "SKILL.md"]);
        fs::create_dir(tmp.path().join("setup")).unwrap();
        assert!(!GstackTool.detects(tmp.path()));
    }

    /// The whole point of the impl: it says no, and says why, in terms the user
    /// can go and verify.
    #[test]
    fn gstack_cannot_remove_a_source_and_names_the_mechanism() {
        let reason = GstackTool.can_remove_source().expect("gstack must refuse");
        assert!(reason.contains("reset --hard"), "the reason must name what reverts it: {reason}");
    }
}
