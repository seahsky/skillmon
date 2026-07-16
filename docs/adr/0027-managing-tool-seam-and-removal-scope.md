# 27. Remove entries, not content — and put managing-tool knowledge in its own seam

## Status

Accepted, and implemented by issue #31 with one signature corrected — see the Update below.
Qualifies ADR 0002 (adapter boundary) and refines ADR 0007 (quarantine/trash).

## Context

"Delete this skill" has no single meaning, because `~/.claude/skills/<name>/SKILL.md` is the same path in every row but resolves three different ways:

| entry | dir | `SKILL.md` | `rm <name>/SKILL.md` does |
| --- | --- | --- | --- |
| `ship` (gstack) | real | **symlink** | deletes the link; gstack keeps the file |
| `tdd` (`.agents`) | **symlink** | real | deletes `~/.agents/skills/tdd/SKILL.md` — **the only copy** (same inode, verified) |
| `vercel-react-…` | real | real | deletes the file; permanent |

One command, three outcomes: a no-op that reverts, a destructive reach into another tool's directory, and the correct thing.

Measured facts about the two managing tools present:

- **gstack** rebuilds unconditionally. `setup:404` runs `mkdir -p` + `ln -snf` over every skill it knows, with no opt-out state consulted. Its only flags are `--prefix` / `--no-prefix` — **there is no per-skill opt-out**. `/gstack-upgrade` runs `git stash && git fetch && git reset --hard origin/main`, and `ship/SKILL.md` is tracked, so deleting content from the checkout is *guaranteed* to revert — and the `git stash` first captures the deletion into a stash the tool then tells the user to `pop`.
- **`.agents`** is not installed as a resident CLI; it runs on demand, and keeps a versioned (v3) `.skill-lock.json` recording `skillFolderHash` per skill.

Neither tool has a daemon. **A rebuild is always user-triggered** — nothing races skillmon in the background.

## Decision

**skillmon removes the entry, never through it.** Entry removal is the default and is uniform: it deletes what sits in the scan root (a symlink, or a shim dir), so damaging a managing tool's content is impossible by construction rather than by warning.

**Source removal is offered only where a managing tool can make it stick**, behind a second, explicit opt-in that names both paths.

Managing-tool knowledge lives in a **new seam parallel to `HarnessAdapter`**, not inside it:

```rust
trait ManagingTool {
    fn detects(&self, root: &Path) -> bool;
    fn can_remove_source(&self) -> Option<&str>;  // None = capable; Some = why not
    fn remove_source(&self, skill: &DiscoveredSkill) -> Result<()>;
}
```

- `.agents` — capable (trash the content, prune its lock entry)
- gstack — **incapable**; the UI omits the option and states why (`git reset --hard` restores it)
- unmanaged — entry and source are the same thing
- unknown tool — entry-only, honestly labeled

**All removals move the entry out of the scan root, reversibly** — one operation, with intent recorded in state rather than encoded in the destination:

- `Disabled` — retained indefinitely
- `Trashed` — eligible for purge

This collapses ADR 0007's two mechanisms (quarantine to `skillmon/disabled/`, trash to `skillmon/trash/<ts>/`) into one.
Once every removal is reversible with a per-skill undo, quarantine and trash are the same `rename(2)`, and differ only in retention.
Disable and delete become a labeling choice over one code path, not a fork.

**A row with dependents is not a skill removal — it is a tool uninstall, and is labeled as one.**
Removing such an entry cascades to every dependent, as one trash unit with one undo.
The entry rule forces this rather than merely permitting it: `gstack`'s entry *is* its content, so there is no removal of that row which is not also a removal of the 1.1 GB checkout that 46 shims resolve into.
A plain delete is offered only where `provides_for == 0`.

## Considered Options

- **Entry-only; point users at their tool.** Smallest surface, zero tool code. Rejected: gstack has no per-skill removal to point at, so 46 rows would get "your tool owns this, and there's no way to remove just one."
- **Widen `ClaudeCodeAdapter`** to hold the gstack/`.agents` logic, since both install into `~/.claude/skills`. Rejected: ADR 0002 scopes the adapter to *one agent's* specifics, and `.skill-lock.json` is not Claude Code's format — it belongs to a tool whose own lock file lists 14 target agents. A second harness would need a copy of the same parser.
- **Delete the target by default.** Rejected: it is precisely the overreach above, promoted to default, and it silently desyncs `.skill-lock.json`.
- **Desired-state reconciliation** — hold intent and re-apply it via the ADR 0019 watcher whenever a tool rebuilds. Rejected as over-engineering once measurement showed neither tool runs in the background: a pre-removal warning is sufficient, since every rebuild is user-initiated.

## Consequences

ADR 0026 keeps *detection* structural and tool-agnostic; tool-specific knowledge is confined to *removal*. The apparent contradiction between the two dissolves along that line, and the boundary is worth defending: detection must handle unknown tools, removal need not.

`can_remove_source` returning a reason (not a bare `false`) is deliberate — the UI must explain *why* an option is missing, or it reads as a bug.

**Trash is uniform even though the model could avoid it.** Trash is only strictly necessary when a removal is unrecoverable, and 66 of 71 entry removals are recoverable from a manager. It is applied anyway: entries are tiny (a symlink, or a dir holding one), so the cost is nil, it keeps ADR 0007's rule literally true with one code path, and — decisively — "recoverable from gstack" means re-running `/gstack-upgrade`, which restores **all 46** shims at once, including others deliberately deleted. That is a reset, not an undo. Trash buys a per-skill undo the managing tool cannot offer.

Trashing `gstack` moves 1.1 GB. Purge (DESIGN #6 tombstones) is a prerequisite, not a follow-up.

ADR 0007's quarantine acquires a hazard it does not currently record: quarantining a gstack shim is silently reverted by the next `/gstack-upgrade`, leaving skillmon's state claiming the skill is disabled while it is live in context, and a re-enable colliding with the path gstack rebuilt. Any quarantine of a managed skill must warn, and must reconcile its state on rescan.

Cascading is the user's only real lever on gstack's 46 skills, not a cleanup detail. gstack has no per-skill opt-out, and `/gstack-upgrade` is itself a gstack skill — so removing the checkout removes the thing that rebuilds, and this is the *only* durable way to remove a gstack skill at all.

"Leave dependents dangling" was rejected on evidence, not taste: `discovery/scan.rs:60-68` turns each dangling shim into a `"no readable SKILL.md"` warning and skips it, so the outcome is 46 warning strings and 46 silently vanished rows.

**`provides_for` is a floor, not a total.** It counts only *discovered* skills, and skillmon scans Claude Code's paths alone. gstack's `setup` also links Codex, Factory, and OpenCode installs into the same checkout (`setup:585-663`) — none present on the reference machine, but a managing tool can always have dependents skillmon cannot see. The uninstall dialog must not claim the count is exhaustive.

skillmon's own writes land inside the personal skills dir, which `adapters/claude_code/watcher.rs` watches **recursively** (ADR 0019), so every mutation triggers its own rescan. Self-write suppression is needed.

The seam cannot live in `adapters/claude_code/`. Discovery rightly sits there, because where an entry points is a Claude Code fact. But `~/.agents` serves 14 agents and gstack installs into Codex, Factory, and OpenCode paths too — so what a managing tool can remove is not a fact about Claude Code, and would be copy-pasted by a second harness.

## Update (issue #31: `remove_source` cannot both remove and stay reversible; the source is staged into its skill's entry)

The trait above writes source removal as one method that does the whole job:

```rust
fn remove_source(&self, skill: &DiscoveredSkill) -> Result<()>;
```

That cannot hold against this ADR's own reversibility rule, and the way it fails is the outcome this ADR already rejected once.

A tool that removes its own source has to put those bytes somewhere reversible, and the only such place is a trash unit.
So the source becomes a unit of its own, and the skill now has **two** undos — while ADR 0029 gives the entry one.
Restore the entry's unit alone and `~/.claude/skills/tdd` is rebuilt as a symlink whose target is still staged: a dangling entry.
`discovery/scan.rs:51-59` turns exactly that into a `"symlink target is not a directory"` warning and skips the row.
That is the "46 warning strings and 46 silently vanished rows" this ADR rejected "leave dependents dangling" on — reintroduced as an undo path.
The lock prune is unreversed by either unit too, which is the `.skill-lock.json` desync this ADR rejected "delete the target by default" for.

So the source is staged **into its skill's own trash entry** (`domain::removal::TrashedSource`), not beside it as a second entry:

- One unit, one undo, unchanged. A restore puts back the entry and the content together, so the link always resolves.
- A unit's entry count stays a count of *skills*, so "47 entries" still means 47 skills rather than a number inflated by whichever of them had a source.
- The tool's job narrows to the bookkeeping that makes the removal stick, and that bookkeeping is reversible:

```rust
trait ManagingTool {
    fn name(&self) -> &'static str;
    fn detects(&self, root: &Path) -> bool;
    fn can_remove_source(&self) -> Option<&str>;      // unchanged: Some = why not
    fn source_of(&self, skill: &DiscoveredSkill) -> Option<PathBuf>;
    fn forget_source(&self, skill: &DiscoveredSkill) -> Result<Option<String>, SourceError>;
    fn relearn_source(&self, state: &str) -> Result<(), SourceError>;
}
```

`forget_source` returns the state it dropped; skillmon stores it verbatim on the trash entry and hands it back on restore.
The state is **opaque** — the ledger never parses it — which keeps the trash tool-neutral for the same reason it is harness-neutral.

`forget_source` runs **before** anything moves, because it is the step that can refuse (a lock at a version this build has not read).
If the removal then fails, `plan::untake_source` hands the state back: otherwise a *refused* removal would leave the tool having forgotten a skill that is still installed, which is worse than doing nothing.

`can_remove_source` returning a reason survives intact, and earned its keep: the panel renders it in the option's place.

### What the two tools actually are, read from their own sources

Neither is installed on the reference machine any more, so both impls were written against the tools themselves rather than inferred from their output.

- **gstack** (`github.com/garrytan/gstack`, public). `setup:540-575` (`link_claude_skill_dirs`) loops `"$gstack_dir"/*/`, `mkdir -p`s a real directory under `~/.claude/skills`, and links `"$gstack_dir/$dir_name/SKILL.md"` into it — confirming issue #25's shape, and confirming that a shim's `manager_root` resolves to the checkout root itself. The loop consults no opt-out state. Detection is a marker trio (`setup`, `VERSION`, `SKILL.md`) at the manager root; a false positive only *withholds* a source removal, so it fails safe. The skills are flat top-level directories, not the `skills/<category>/<name>` the domain's own unit-test fixtures imagine — the fixtures are illustrative, and the ancestor test covers both.
- **`.agents`** (the `skills` CLI). The lock is at `$XDG_STATE_HOME/skills/.skill-lock.json` when that variable is set and `~/.agents/.skill-lock.json` otherwise. `skills` is keyed by the **unsanitized** name (`addSkillToLock(skill.name, …)`) while the installed folder is `sanitizeName(skill.name)` — so a lock key **cannot be read off a directory name**, and has to be searched for by inverting the tool's own rule. `readSkillLock` discards any lock below version 3 wholesale, which is why an unrecognized version is refused rather than rewritten: guessing at the shape could cost the user every entry they have.

`serde_json`'s `preserve_order` is enabled crate-wide so pruning one key does not silently re-alphabetize a file skillmon does not own.

### The hazard is reconciled by derivation, not by a flag

This ADR requires a quarantined managed skill to "reconcile its state on rescan".
`TrashUnit::is_reverted` derives it from the disk on every read instead of storing it: it is precisely the condition that makes a restore fail with `OriginOccupied`, so a stored flag would be a second source of truth able to contradict the operation it describes.
It uses `symlink_metadata`, never `exists()` — a shim rebuilt ahead of its target is a *dangling* link, which `exists()` reports as absent, and that is exactly the shape a mid-rebuild tool leaves behind.
