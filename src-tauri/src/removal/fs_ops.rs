//! The filesystem primitives every reversible removal is built from (ADR 0029).
//!
//! One rule runs through all three of them, and it is ADR 0027's: **skillmon
//! operates on the entry, never through it.** A symlink entry is moved as a
//! link, copied as a link, sized as a link, and deleted as a link. Nothing here
//! ever follows one, which is also why none of these walks needs the
//! visited-canonical-path guard ADR 0028's on-demand walk needs -- a walk that
//! cannot enter a symlink cannot find a cycle.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Moves an entry, preserving what it is.
///
/// Tries `rename(2)` first, which is atomic and free. `rename` is atomic only
/// within one filesystem, so a cross-device failure -- and *only* that failure,
/// never a permission or not-found error, which must surface as themselves --
/// falls back to copy/fsync/swap/unlink (ADR 0007, ADR 0029).
pub fn move_entry(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        // Stable since Rust 1.85, under this crate's 1.89 MSRV. Matching the
        // kind rather than a raw EXDEV/ERROR_NOT_SAME_DEVICE errno keeps the
        // check portable without a `libc` dependency.
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => copy_swap_unlink(from, to),
        Err(e) => Err(e),
    }
}

/// ADR 0007's documented fallback, in its load-bearing order: copy into a
/// staging path *inside the destination directory* (so the swap that follows is
/// same-device and therefore atomic), fsync it so the bytes are durable, swap it
/// into place, and only then unlink the source.
///
/// Unlinking last is the whole point. A crash anywhere before the last step
/// leaves the entry both live and staged -- a duplicate the next scan reads as
/// "still installed", which is recoverable. Any order that unlinks earlier can
/// lose the entry outright.
///
/// Not `pub`, but exercised directly by this module's tests: provoking a real
/// EXDEV in a unit test would need a second mounted filesystem, so the fallback
/// is tested by calling it, not by arranging for `rename` to fail.
fn copy_swap_unlink(from: &Path, to: &Path) -> io::Result<()> {
    let staging = staging_path(to)?;
    let staging_dir = staging.parent().expect("staging_path always nests under a parent");
    // A crashed earlier attempt can have left one of these behind. It belongs to
    // no unit (nothing records a staging path), so clearing it is safe.
    delete_if_exists(&staging)?;
    fs::create_dir_all(staging_dir)?;

    copy_entry(from, &staging)?;
    sync_tree(&staging)?;
    fs::rename(&staging, to)?;
    // Persist the directory entry itself, not just the file contents: without
    // this the swap can be lost while the copied bytes survive.
    if let Some(parent) = to.parent() {
        sync_dir(parent)?;
    }
    remove_dir_if_empty(staging_dir);
    delete_entry(from)
}

/// The staging slot for a cross-device move: `<to's parent>/.skillmon-partial/<to's name>`.
///
/// Two constraints pull against each other here, and the nesting is what
/// satisfies both.
///
/// It must sit **inside `to`'s directory** so that it lands on `to`'s
/// filesystem -- that is the entire point, since the swap that follows has to be
/// a same-device `rename` to be atomic. A staging area anywhere else could be on
/// a third device and merely move the EXDEV problem.
///
/// But it must **not be a sibling of `to`**, because `to` is not always in the
/// trash. A restore's destination is back under the scan root, so a sibling
/// staging path would be `~/.claude/skills/.tdd.skillmon-partial` -- and
/// discovery filters nothing by name (`discovery/scan.rs`), so a crash between
/// the copy and the swap would leave a *discoverable* bogus skill sitting in
/// context, which no later purge would ever clean up. Nesting one level down
/// puts it at depth 2, where personal-skill discovery (depth-1 only, DESIGN.md)
/// cannot see it, on either side of the move.
fn staging_path(to: &Path) -> io::Result<PathBuf> {
    let name = to.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{} has no file name", to.display()))
    })?;
    let parent = to.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{} has no parent directory", to.display()))
    })?;
    Ok(parent.join(STAGING_DIR_NAME).join(name))
}

/// Dot-prefixed so a human who finds one after a crash can tell what it is, and
/// so it sorts out of the way.
const STAGING_DIR_NAME: &str = ".skillmon-partial";

/// Recursively copies an entry, reproducing a symlink as a symlink.
///
/// Resolving a link here would be the exact overreach ADR 0027 forbids,
/// reintroduced as an error handler: the rename path moves a pointer, so the
/// fallback that stands in for it must move a pointer too -- otherwise crossing
/// a device boundary would silently turn "remove this shim" into "copy 1.1 GB of
/// the tool's checkout".
fn copy_entry(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.is_symlink() {
        return symlink_to(&fs::read_link(from)?, to);
    }
    if !meta.is_dir() {
        fs::copy(from, to)?;
        return Ok(());
    }
    fs::create_dir_all(to)?;
    for child in fs::read_dir(from)? {
        let child = child?;
        copy_entry(&child.path(), &to.join(child.file_name()))?;
    }
    Ok(())
}

/// fsyncs every regular file and directory in a freshly-copied tree, so the
/// source can be unlinked knowing the copy survives a crash.
///
/// Symlinks are skipped: fsyncing one needs an `O_PATH`/`O_NOFOLLOW` handle Rust
/// does not expose, and opening it the ordinary way would follow the link and
/// sync the target instead -- reaching through the entry. The link is persisted
/// by its parent directory's fsync anyway, which is where a link actually lives.
fn sync_tree(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_symlink() {
        return Ok(());
    }
    if !meta.is_dir() {
        return fs::File::open(path)?.sync_all();
    }
    for child in fs::read_dir(path)? {
        sync_tree(&child?.path())?;
    }
    sync_dir(path)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

/// Windows cannot open a directory as a `File`, so there is no directory fsync
/// to perform. Windows is out of MVP scope (DESIGN.md); this exists so the crate
/// compiles for it, and the durability gap is recorded rather than hidden.
#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink_to(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows needs to know at creation time whether a link points at a directory,
/// which Unix does not. `is_dir()` resolves the target against the *process*
/// cwd, so a relative or dangling target can be misclassified -- acceptable
/// only because Windows is out of MVP scope, and flagged here so it is fixed
/// rather than discovered.
#[cfg(windows)]
fn symlink_to(target: &Path, link: &Path) -> io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// An entry's disk bytes -- what removing it actually reclaims.
///
/// **This is not ADR 0028's on-demand walk, and must never be given its
/// exclusions.** That walk skips `.git`, `node_modules`, and nested `SKILL.md`
/// subtrees because they cannot enter *context*. This one answers what leaves
/// the *disk*, where a 60 MB `.git` and a 704 MB `node_modules` are most of the
/// answer. The exclusions that make one walk correct make the other a lie.
///
/// A symlink counts as the link, not its target, for the same reason the move
/// preserves it: purging the entry unlinks a pointer and reclaims nothing else.
pub fn entry_size(path: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_symlink() || !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0;
    for child in fs::read_dir(path)? {
        total += entry_size(&child?.path())?;
    }
    Ok(total)
}

/// Deletes an entry: a tree if it is a real directory, a single unlink if it is
/// a file or a symlink.
///
/// The symlink case is why this is not a bare `remove_dir_all`. `symlink_metadata`
/// reports a symlink-to-directory as *not* a directory, so a link is unlinked
/// and its target is untouched -- which is the difference between removing a
/// gstack shim and deleting the file it points at.
pub fn delete_entry(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// `delete_entry`, treating "already gone" as success and every other failure as
/// itself. For the paths that are legitimately allowed not to exist -- a staging
/// file from a crash, a stored entry a user deleted by hand -- where a bare
/// `.ok()` would also swallow the permission error that means a purge silently
/// reclaimed nothing.
pub fn delete_if_exists(path: &Path) -> io::Result<()> {
    match delete_entry(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Removes a unit's storage directory once its entries are gone. Tolerates a
/// non-empty or missing directory: this is housekeeping after the operation that
/// mattered already succeeded, so failing the whole restore because a stray file
/// kept the directory alive would be the wrong trade.
pub fn remove_dir_if_empty(path: &Path) {
    let _ = fs::remove_dir(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        File::create(path).unwrap().write_all(contents.as_bytes()).unwrap();
    }

    #[cfg(unix)]
    fn link(target: &Path, at: &Path) {
        if let Some(parent) = at.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    #[test]
    fn move_entry_relocates_a_real_directory_and_creates_the_destination_parent() {
        let tmp = tempdir().unwrap();
        let from = tmp.path().join("skills/vercel-react");
        write_file(&from.join("SKILL.md"), "---\nname: vercel-react\n---\nbody");
        let to = tmp.path().join("skillmon/removed/1/0-vercel-react");

        move_entry(&from, &to).unwrap();

        assert!(!from.exists(), "the entry left the scan root");
        assert_eq!(fs::read_to_string(to.join("SKILL.md")).unwrap(), "---\nname: vercel-react\n---\nbody");
    }

    /// The `.agents` shape (ADR 0027): the entry is a symlink whose target is
    /// the only copy of the file. Moving the entry must move the pointer.
    #[cfg(unix)]
    #[test]
    fn move_entry_moves_a_symlink_as_a_link_and_never_touches_its_target() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("agents/skills/tdd");
        write_file(&target.join("SKILL.md"), "the only copy");
        let entry = tmp.path().join("skills/tdd");
        link(&target, &entry);
        let to = tmp.path().join("removed/1/0-tdd");

        move_entry(&entry, &to).unwrap();

        assert!(fs::symlink_metadata(&to).unwrap().is_symlink(), "still a link, not a copy");
        assert_eq!(fs::read_link(&to).unwrap(), target);
        assert_eq!(fs::read_to_string(target.join("SKILL.md")).unwrap(), "the only copy");
    }

    /// A rename that fails for a reason other than EXDEV must surface, not
    /// silently take the copy path.
    #[test]
    fn move_entry_propagates_a_missing_source_rather_than_falling_back() {
        let tmp = tempdir().unwrap();
        let err = move_entry(&tmp.path().join("nope"), &tmp.path().join("removed/1/0-nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn cross_device_fallback_copies_a_tree_then_unlinks_the_source() {
        let tmp = tempdir().unwrap();
        let from = tmp.path().join("skills/gstack");
        write_file(&from.join("SKILL.md"), "top");
        write_file(&from.join("references/deep/note.md"), "nested");
        let to = tmp.path().join("removed/1/0-gstack");
        fs::create_dir_all(to.parent().unwrap()).unwrap();

        copy_swap_unlink(&from, &to).unwrap();

        assert!(!from.exists(), "the source is unlinked only after the copy lands");
        assert_eq!(fs::read_to_string(to.join("SKILL.md")).unwrap(), "top");
        assert_eq!(fs::read_to_string(to.join("references/deep/note.md")).unwrap(), "nested");
    }

    /// The fallback must not become the one path that reaches through an entry.
    #[cfg(unix)]
    #[test]
    fn cross_device_fallback_reproduces_a_symlink_instead_of_resolving_it() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("agents/skills/tdd");
        write_file(&target.join("SKILL.md"), "the only copy");
        let entry = tmp.path().join("skills/tdd");
        link(&target, &entry);
        let to = tmp.path().join("removed/1/0-tdd");
        fs::create_dir_all(to.parent().unwrap()).unwrap();

        copy_swap_unlink(&entry, &to).unwrap();

        assert!(fs::symlink_metadata(&to).unwrap().is_symlink());
        assert_eq!(fs::read_link(&to).unwrap(), target);
        assert!(target.join("SKILL.md").exists(), "the target survived");
    }

    /// The restore direction is the dangerous one: its destination is back under
    /// the depth-1 scan root, so a staging slot that were a *sibling* of the
    /// destination would be a discoverable skill dir. Discovery filters nothing
    /// by name, so a crash mid-copy would leave a bogus skill live in context
    /// that no purge would ever clean up. Nesting puts it at depth 2, out of
    /// reach.
    #[test]
    fn the_staging_slot_is_never_a_sibling_of_its_destination() {
        let skills = Path::new("/home/me/.claude/skills");
        let staging = staging_path(&skills.join("tdd")).unwrap();

        assert_eq!(staging, skills.join(".skillmon-partial/tdd"));
        assert_ne!(
            staging.parent(),
            Some(skills),
            "a sibling of the destination is a depth-1 entry, and so is discoverable"
        );
        // Same device as the destination, which is what makes the swap atomic.
        assert!(staging.starts_with(skills));
    }

    #[test]
    fn the_cross_device_fallback_leaves_no_staging_directory_behind() {
        let tmp = tempdir().unwrap();
        let from = tmp.path().join("removed/1/0-tdd");
        write_file(&from.join("SKILL.md"), "restored");
        let to = tmp.path().join("skills/tdd");
        fs::create_dir_all(to.parent().unwrap()).unwrap();

        copy_swap_unlink(&from, &to).unwrap();

        assert_eq!(fs::read_to_string(to.join("SKILL.md")).unwrap(), "restored");
        assert!(
            !to.parent().unwrap().join(STAGING_DIR_NAME).exists(),
            "the staging directory is cleaned up, not left in the scan root"
        );
    }

    #[test]
    fn cross_device_fallback_clears_a_staging_file_left_by_a_crashed_attempt() {
        let tmp = tempdir().unwrap();
        let from = tmp.path().join("skills/x");
        write_file(&from.join("SKILL.md"), "fresh");
        let to = tmp.path().join("removed/1/0-x");
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        // A half-written tree from an attempt that died before the swap.
        write_file(&staging_path(&to).unwrap().join("SKILL.md"), "stale garbage");

        copy_swap_unlink(&from, &to).unwrap();

        assert_eq!(fs::read_to_string(to.join("SKILL.md")).unwrap(), "fresh");
        assert!(!staging_path(&to).unwrap().exists(), "staging is consumed by the swap");
    }

    #[test]
    fn entry_size_sums_a_tree_including_what_the_on_demand_walk_excludes() {
        // ADR 0028 skips .git and node_modules because they cannot enter
        // context. They are exactly what a purge reclaims, so they count here.
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("gstack");
        write_file(&dir.join("SKILL.md"), "0123456789"); // 10
        write_file(&dir.join(".git/objects/abc"), "01234"); // 5
        write_file(&dir.join("node_modules/pkg/index.js"), "012345678901234"); // 15
        write_file(&dir.join("nested/SKILL.md"), "01234"); // 5

        assert_eq!(entry_size(&dir).unwrap(), 35);
    }

    #[cfg(unix)]
    #[test]
    fn entry_size_counts_a_symlink_as_the_link_not_the_target() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("huge");
        write_file(&target.join("blob"), &"x".repeat(100_000));
        let entry = tmp.path().join("skills/shim");
        link(&target, &entry);

        let size = entry_size(&entry).unwrap();
        assert!(size < 1_000, "a shim reclaims the link's bytes, not the target's: got {size}");
    }

    #[cfg(unix)]
    #[test]
    fn delete_entry_unlinks_a_symlink_and_leaves_its_target_intact() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("agents/skills/tdd");
        write_file(&target.join("SKILL.md"), "the only copy");
        let entry = tmp.path().join("skills/tdd");
        link(&target, &entry);

        delete_entry(&entry).unwrap();

        assert!(fs::symlink_metadata(&entry).is_err(), "the link is gone");
        assert_eq!(fs::read_to_string(target.join("SKILL.md")).unwrap(), "the only copy");
    }

    #[test]
    fn delete_entry_removes_a_real_tree() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("gstack");
        write_file(&dir.join("a/b/c.md"), "x");

        delete_entry(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn delete_if_exists_forgives_a_missing_path_but_not_other_failures() {
        let tmp = tempdir().unwrap();
        delete_if_exists(&tmp.path().join("never-existed")).unwrap();

        // A path whose *parent* is a file, not a directory: not NotFound, so it
        // must surface rather than be swallowed as "already gone".
        let file = tmp.path().join("a-file");
        write_file(&file, "x");
        assert!(delete_if_exists(&file.join("child")).is_err());
    }
}
