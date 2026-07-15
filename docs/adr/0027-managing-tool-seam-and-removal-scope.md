# 27. Remove entries, not content — and put managing-tool knowledge in its own seam

## Status

Proposed. Qualifies ADR 0002 (adapter boundary) and refines ADR 0007 (quarantine/trash).

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
